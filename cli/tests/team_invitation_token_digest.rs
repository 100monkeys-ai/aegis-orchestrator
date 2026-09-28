// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The team invitation token is stored as a digest, never as itself.
//!
//! These tests run the real invitation service and the real PostgreSQL
//! repositories against a real PostgreSQL, with the migrations this binary
//! ships. CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a
//! database a superuser can connect to. Each test creates its own database
//! there and drops it at the end. In CI (`CI` set) a missing URL fails the
//! test; elsewhere the tests say they were skipped and pass.
//!
//! They cover: the stored column is not the token and cannot be presented in
//! its place; an invitation stored before migration 033, in the old form, is
//! accepted after it with the token its link carries, once; an expired one
//! is refused; migration 033 run again changes nothing; a row already in the
//! digest form is not converted again; and the column refuses a token.

use std::pin::Pin;
use std::sync::Arc;

use aegis_orchestrator_core::application::billing_service::{BillingService, BillingServiceError};
use aegis_orchestrator_core::application::team_service::{
    AcceptInvitationCommand, InviteMemberCommand, ProvisionTeamCommand, StandardTeamService,
    TeamService, TeamServiceError,
};
use aegis_orchestrator_core::domain::secrets::{SensitiveBytes, SensitiveString};
use aegis_orchestrator_core::domain::team::{Team, TeamId};
use aegis_orchestrator_core::domain::tenancy::TenantTier;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_team::{
    PgMembershipRepository, PgTeamInvitationRepository, PgTeamRepository,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_tenant::PostgresTenantRepository;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use sqlx::error::BoxDynError;
use sqlx::migrate::{Migration, MigrationSource, Migrator};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The last migration before the invitation token was stored as a digest.
const LAST_MIGRATION_WITH_THE_TOKEN_STORED: i64 = 32;

const INVITATION_KEY: &[u8] = b"test-invitation-key-not-a-secret";
const OWNER: &str = "owner-sub";

// ── The database each test gets ─────────────────────────────────────────────

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI these tests must reach PostgreSQL")
        }
        _ => {
            eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
            None
        }
    }
}

struct TestDb {
    server: PgPool,
    name: String,
    pool: PgPool,
}

impl TestDb {
    /// A new, empty database. `None` when no PostgreSQL is configured.
    async fn create() -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_invite_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        Some(Self { server, name, pool })
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

/// The migrations this binary ships, up to and including `last`.
#[derive(Debug)]
struct MigrationsUpTo(i64);

impl MigrationSource<'static> for MigrationsUpTo {
    fn resolve(
        self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<Migration>, BoxDynError>> + Send>>
    {
        Box::pin(async move {
            Ok(MIGRATOR
                .iter()
                .filter(|m| m.version <= self.0)
                .cloned()
                .collect())
        })
    }
}

/// The schema as it stood while the token itself was stored.
async fn migrate_to_the_schema_that_stored_the_token(pool: &PgPool) {
    Migrator::new(MigrationsUpTo(LAST_MIGRATION_WITH_THE_TOKEN_STORED))
        .await
        .expect("resolve the earlier migrations")
        .run(pool)
        .await
        .expect("apply the earlier migrations");
}

/// Every migration this binary ships, through the runner the daemon uses.
async fn migrate_fully(pool: &PgPool) {
    MIGRATOR.run(pool).await.expect("apply every migration");
}

// ── The service under test ──────────────────────────────────────────────────

/// Billing is not under test: seats are counted and nothing is sent.
struct NoBilling;

#[async_trait]
impl BillingService for NoBilling {
    async fn sync_seats(
        &self,
        _team_id: TeamId,
        _tenant_id: &TenantId,
        _active_member_count: u32,
    ) -> Result<u32, BillingServiceError> {
        Ok(1)
    }
    async fn provision_team_customer(
        &self,
        _team_id: TeamId,
        _owner_email: String,
        _tenant_id: &TenantId,
        _tier: TenantTier,
    ) -> Result<String, BillingServiceError> {
        Ok("cus_test".to_string())
    }
    async fn cancel_team_subscription(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<(), BillingServiceError> {
        Ok(())
    }
}

fn service(pool: &PgPool) -> StandardTeamService {
    StandardTeamService::new(
        Arc::new(PgTeamRepository::new(pool.clone())),
        Arc::new(PgMembershipRepository::new(pool.clone())),
        Arc::new(PgTeamInvitationRepository::new(pool.clone())),
        Arc::new(PostgresTenantRepository::new(pool.clone())),
        Arc::new(NoBilling),
        Arc::new(EventBus::with_default_capacity()),
        Some(SensitiveBytes::new(INVITATION_KEY.to_vec())),
        None,
        None,
    )
}

async fn provision(svc: &StandardTeamService) -> Team {
    svc.provision_team(ProvisionTeamCommand {
        display_name: "Acme".to_string(),
        owner_user_id: OWNER.to_string(),
        owner_email: "owner@example.com".to_string(),
        tier: TenantTier::Business,
    })
    .await
    .expect("provision the team")
}

async fn accept(
    svc: &StandardTeamService,
    token: &str,
    email: &str,
    user: &str,
) -> Result<(), TeamServiceError> {
    svc.accept_invitation(AcceptInvitationCommand {
        token: SensitiveString::new(token),
        authenticated_email: email.to_string(),
        authenticated_user_id: user.to_string(),
    })
    .await
    .map(|_| ())
}

// ── The token and its digest, computed here independently ───────────────────

/// The invitation token as the service issues it:
/// hex HMAC-SHA256 over `team_id ":" email` under the invitation key.
fn token_for(team: &Team, email: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(INVITATION_KEY).unwrap();
    mac.update(team.id.to_string().as_bytes());
    mac.update(b":");
    mac.update(email.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// `sha256:` and the lowercase hex SHA-256 of the token's text.
fn digest_of(token: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(token.as_bytes())))
}

/// Insert an invitation row directly, as the daemon wrote it before migration
/// 033 (`stored` is then the token) or after it (`stored` is the digest).
async fn insert_invitation(
    pool: &PgPool,
    team: &Team,
    email: &str,
    stored: &str,
    expires_in: &str,
) {
    sqlx::query(
        "INSERT INTO team_invitations \
         (id, team_id, invitee_email, token_hash, status, expires_at, invited_by) \
         VALUES (gen_random_uuid(), $1::uuid, $2, $3, 'pending', NOW() + $4::interval, $5)",
    )
    .bind(team.id.to_string())
    .bind(email)
    .bind(stored)
    .bind(expires_in)
    .bind(OWNER)
    .execute(pool)
    .await
    .expect("insert the invitation row");
}

async fn stored_value(pool: &PgPool, email: &str) -> String {
    sqlx::query("SELECT token_hash FROM team_invitations WHERE invitee_email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .expect("read the stored invitation")
        .get("token_hash")
}

async fn all_stored_rows(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query(
        "SELECT id::text AS id, invitee_email, token_hash FROM team_invitations ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("read the invitation rows")
    .iter()
    .map(|r| (r.get("id"), r.get("invitee_email"), r.get("token_hash")))
    .collect()
}

async fn is_active_member(pool: &PgPool, team: &Team, user: &str) -> bool {
    sqlx::query(
        "SELECT 1 FROM team_memberships WHERE team_id = $1::uuid AND user_id = $2 AND status = 'active'",
    )
    .bind(team.id.to_string())
    .bind(user)
    .fetch_optional(pool)
    .await
    .expect("read the membership")
    .is_some()
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// The column holds the digest of the token the invitee is sent, not the
/// token.
#[tokio::test]
async fn the_stored_invitation_is_a_digest_of_the_token() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    migrate_fully(&db.pool).await;
    let svc = service(&db.pool);
    let team = provision(&svc).await;
    let issued = svc
        .invite_member(InviteMemberCommand {
            team_id: team.id,
            invitee_email: "invitee@example.com".to_string(),
            invited_by_user_id: OWNER.to_string(),
        })
        .await
        .expect("invite");

    let stored = stored_value(&db.pool, "invitee@example.com").await;
    assert_ne!(
        stored,
        issued.raw_token.expose(),
        "team_invitations.token_hash holds the invitation token itself"
    );
    assert_eq!(stored, digest_of(issued.raw_token.expose()));
    db.remove().await;
}

/// Whoever reads the column cannot accept the invitation with what they read,
/// even when signed in as the invitee. The token in the link still accepts.
#[tokio::test]
async fn the_stored_value_cannot_accept_the_invitation() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    migrate_fully(&db.pool).await;
    let svc = service(&db.pool);
    let team = provision(&svc).await;
    let issued = svc
        .invite_member(InviteMemberCommand {
            team_id: team.id,
            invitee_email: "invitee@example.com".to_string(),
            invited_by_user_id: OWNER.to_string(),
        })
        .await
        .expect("invite");
    let stored = stored_value(&db.pool, "invitee@example.com").await;

    let with_stored = accept(&svc, &stored, "invitee@example.com", "invitee-sub").await;
    assert!(
        matches!(with_stored, Err(TeamServiceError::InvalidInvitation)),
        "the stored column's value accepted the invitation: {with_stored:?}"
    );
    assert!(!is_active_member(&db.pool, &team, "invitee-sub").await);

    accept(
        &svc,
        issued.raw_token.expose(),
        "invitee@example.com",
        "invitee-sub",
    )
    .await
    .expect("the token in the link accepts the invitation");
    assert!(is_active_member(&db.pool, &team, "invitee-sub").await);
    db.remove().await;
}

/// An invitation stored before migration 033, with the token itself in the
/// column, is converted by the migration and accepted after it with the
/// token its link carries. Once: a second use is refused. An expired one
/// stored the same way is refused.
#[tokio::test]
async fn an_invitation_stored_before_the_digest_migration_is_accepted_once_after_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    migrate_to_the_schema_that_stored_the_token(&db.pool).await;
    let svc = service(&db.pool);
    let team = provision(&svc).await;
    let token = token_for(&team, "early@example.com");
    let expired_token = token_for(&team, "late@example.com");
    insert_invitation(&db.pool, &team, "early@example.com", &token, "7 days").await;
    insert_invitation(
        &db.pool,
        &team,
        "late@example.com",
        &expired_token,
        "-1 day",
    )
    .await;

    migrate_fully(&db.pool).await;

    for (email, token) in [
        ("early@example.com", &token),
        ("late@example.com", &expired_token),
    ] {
        let stored = stored_value(&db.pool, email).await;
        assert_eq!(
            stored,
            digest_of(token),
            "the digest migration left {email}'s token in team_invitations.token_hash"
        );
    }

    accept(&svc, &token, "early@example.com", "early-sub")
        .await
        .expect("a link sent before the migration still accepts the invitation");
    assert!(is_active_member(&db.pool, &team, "early-sub").await);

    let second = accept(&svc, &token, "early@example.com", "early-sub").await;
    assert!(
        matches!(second, Err(TeamServiceError::InvitationNotPending)),
        "a second use of the token was not refused: {second:?}"
    );

    let expired = accept(&svc, &expired_token, "late@example.com", "late-sub").await;
    assert!(
        matches!(expired, Err(TeamServiceError::InvitationExpired)),
        "an expired invitation was not refused: {expired:?}"
    );
    assert!(!is_active_member(&db.pool, &team, "late-sub").await);
    db.remove().await;
}

/// Migration 033 run a second time over the converted rows changes nothing.
#[tokio::test]
async fn the_digest_migration_run_twice_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    migrate_to_the_schema_that_stored_the_token(&db.pool).await;
    let svc = service(&db.pool);
    let team = provision(&svc).await;
    for email in ["one@example.com", "two@example.com"] {
        insert_invitation(&db.pool, &team, email, &token_for(&team, email), "7 days").await;
    }
    migrate_fully(&db.pool).await;
    let after_first = all_stored_rows(&db.pool).await;

    let later: Vec<&Migration> = MIGRATOR
        .iter()
        .filter(|m| m.version > LAST_MIGRATION_WITH_THE_TOKEN_STORED)
        .collect();
    assert!(
        !later.is_empty(),
        "no migration after {LAST_MIGRATION_WITH_THE_TOKEN_STORED} converts the stored invitation tokens"
    );
    for migration in later {
        sqlx::raw_sql(&migration.sql)
            .execute(&db.pool)
            .await
            .unwrap_or_else(|e| panic!("migration {} run again failed: {e}", migration.version));
    }

    assert_eq!(
        all_stored_rows(&db.pool).await,
        after_first,
        "running the digest migration again changed the stored invitations"
    );
    db.remove().await;
}

/// A row already holding a digest when migration 033 runs is left as it is,
/// and its token accepts it.
#[tokio::test]
async fn a_row_already_holding_a_digest_is_not_converted_again() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    migrate_to_the_schema_that_stored_the_token(&db.pool).await;
    let svc = service(&db.pool);
    let team = provision(&svc).await;
    let token = token_for(&team, "new@example.com");
    insert_invitation(
        &db.pool,
        &team,
        "new@example.com",
        &digest_of(&token),
        "7 days",
    )
    .await;

    migrate_fully(&db.pool).await;

    assert_eq!(
        stored_value(&db.pool, "new@example.com").await,
        digest_of(&token),
        "the migration converted a row that already held a digest"
    );
    let accepted = accept(&svc, &token, "new@example.com", "new-sub").await;
    assert!(
        accepted.is_ok(),
        "a row holding the digest was not found by its token: {accepted:?}"
    );
    db.remove().await;
}

/// After the migration the column refuses a token: only a digest fits.
#[tokio::test]
async fn the_column_refuses_a_token_after_the_migration() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    migrate_fully(&db.pool).await;
    let svc = service(&db.pool);
    let team = provision(&svc).await;
    let token = token_for(&team, "raw@example.com");

    let refused = sqlx::query(
        "INSERT INTO team_invitations \
         (id, team_id, invitee_email, token_hash, status, expires_at, invited_by) \
         VALUES (gen_random_uuid(), $1::uuid, 'raw@example.com', $2, 'pending', NOW(), $3)",
    )
    .bind(team.id.to_string())
    .bind(&token)
    .bind(OWNER)
    .execute(&db.pool)
    .await;
    assert!(
        refused.is_err(),
        "team_invitations.token_hash accepted a token in place of a digest"
    );
    db.remove().await;
}
