// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Team Repositories (ADR-111)
//!
//! Production implementations for [`TeamRepository`],
//! [`MembershipRepository`], and [`TeamInvitationRepository`] backed by the
//! `teams`, `team_memberships`, and `team_invitations` tables introduced in
//! migration `022_teams.sql`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::domain::repository::RepositoryError;
use crate::domain::team::{
    InvitationStatus, Membership, MembershipRepository, MembershipRole, MembershipStatus, Team,
    TeamId, TeamInvitation, TeamInvitationId, TeamInvitationRepository, TeamRepository, TeamSlug,
};
use crate::domain::tenancy::TenantTier;
use crate::domain::tenant::TenantId;

// ============================================================================
// Mapping helpers
// ============================================================================

fn tier_to_str(tier: &TenantTier) -> &'static str {
    match tier {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Business => "business",
        TenantTier::Enterprise => "enterprise",
        TenantTier::System => "system",
    }
}

fn parse_tier(s: &str) -> Result<TenantTier, RepositoryError> {
    match s {
        "free" => Ok(TenantTier::Free),
        "pro" => Ok(TenantTier::Pro),
        "business" => Ok(TenantTier::Business),
        "enterprise" => Ok(TenantTier::Enterprise),
        "system" => Ok(TenantTier::System),
        other => Err(RepositoryError::Serialization(format!(
            "unknown tier: {other}"
        ))),
    }
}

fn row_to_team(row: &sqlx::postgres::PgRow) -> Result<Team, RepositoryError> {
    let id: Uuid = row.get("id");
    let slug_str: String = row.get("slug");
    let slug = TeamSlug::parse(&slug_str)
        .map_err(|e| RepositoryError::Serialization(format!("invalid team slug: {e}")))?;
    let tenant_id_str: String = row.get("tenant_id");
    let tenant_id = TenantId::from_string(&tenant_id_str)
        .map_err(|e| RepositoryError::Serialization(format!("invalid tenant id: {e}")))?;
    let tier_str: String = row.get("tier");
    Ok(Team {
        id: TeamId(id),
        slug,
        display_name: row.get("display_name"),
        owner_user_id: row.get("owner_user_id"),
        tier: parse_tier(&tier_str)?,
        tenant_id,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        domain_events: Vec::new(),
    })
}

fn row_to_membership(row: &sqlx::postgres::PgRow) -> Result<Membership, RepositoryError> {
    let team_id: Uuid = row.get("team_id");
    let role_str: String = row.get("role");
    let status_str: String = row.get("status");
    let revoked_at: Option<DateTime<Utc>> = row.get("revoked_at");
    Ok(Membership {
        team_id: TeamId(team_id),
        user_id: row.get("user_id"),
        role: MembershipRole::from_str(&role_str).map_err(RepositoryError::Serialization)?,
        status: MembershipStatus::from_str(&status_str).map_err(RepositoryError::Serialization)?,
        joined_at: row.get("joined_at"),
        revoked_at,
    })
}

fn row_to_invitation(row: &sqlx::postgres::PgRow) -> Result<TeamInvitation, RepositoryError> {
    let id: Uuid = row.get("id");
    let team_id: Uuid = row.get("team_id");
    let status_str: String = row.get("status");
    let accepted_at: Option<DateTime<Utc>> = row.get("accepted_at");
    let cancelled_at: Option<DateTime<Utc>> = row.get("cancelled_at");
    Ok(TeamInvitation {
        id: TeamInvitationId(id),
        team_id: TeamId(team_id),
        invitee_email: row.get("invitee_email"),
        token_hash: row.get("token_hash"),
        status: InvitationStatus::from_str(&status_str).map_err(RepositoryError::Serialization)?,
        expires_at: row.get("expires_at"),
        invited_by: row.get("invited_by"),
        created_at: row.get("created_at"),
        accepted_at,
        cancelled_at,
        domain_events: Vec::new(),
    })
}

// ============================================================================
// PgTeamRepository
// ============================================================================

pub struct PgTeamRepository {
    pool: PgPool,
}

impl PgTeamRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TeamRepository for PgTeamRepository {
    async fn save(&self, team: &Team) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            INSERT INTO teams (
                id, slug, display_name, owner_user_id, tier, tenant_id,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO UPDATE SET
                display_name = EXCLUDED.display_name,
                tier = EXCLUDED.tier,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(team.id.0)
        .bind(team.slug.as_str())
        .bind(&team.display_name)
        .bind(&team.owner_user_id)
        .bind(tier_to_str(&team.tier))
        .bind(team.tenant_id.as_str())
        .bind(team.created_at)
        .bind(team.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn find_by_id(&self, id: &TeamId) -> Result<Option<Team>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM teams WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        row.as_ref().map(row_to_team).transpose()
    }

    async fn find_by_slug(&self, slug: &TeamSlug) -> Result<Option<Team>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM teams WHERE slug = $1")
            .bind(slug.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        row.as_ref().map(row_to_team).transpose()
    }

    async fn find_by_owner(&self, owner_user_id: &str) -> Result<Vec<Team>, RepositoryError> {
        let rows = sqlx::query("SELECT * FROM teams WHERE owner_user_id = $1 ORDER BY created_at")
            .bind(owner_user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        rows.iter().map(row_to_team).collect()
    }

    async fn delete(&self, id: &TeamId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM teams WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
}

// ============================================================================
// PgMembershipRepository
// ============================================================================

pub struct PgMembershipRepository {
    pool: PgPool,
}

impl PgMembershipRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MembershipRepository for PgMembershipRepository {
    async fn save(&self, membership: &Membership) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            INSERT INTO team_memberships (
                team_id, user_id, role, status, joined_at, revoked_at
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (team_id, user_id) DO UPDATE SET
                role = EXCLUDED.role,
                status = EXCLUDED.status,
                revoked_at = EXCLUDED.revoked_at
            "#,
        )
        .bind(membership.team_id.0)
        .bind(&membership.user_id)
        .bind(membership.role.as_str())
        .bind(membership.status.as_str())
        .bind(membership.joined_at)
        .bind(membership.revoked_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn find_by_team(&self, team_id: &TeamId) -> Result<Vec<Membership>, RepositoryError> {
        let rows = sqlx::query("SELECT * FROM team_memberships WHERE team_id = $1")
            .bind(team_id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        rows.iter().map(row_to_membership).collect()
    }

    async fn find_by_user(&self, user_id: &str) -> Result<Vec<Membership>, RepositoryError> {
        let rows = sqlx::query("SELECT * FROM team_memberships WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        rows.iter().map(row_to_membership).collect()
    }

    async fn is_active_member(
        &self,
        user_id: &str,
        team_id: &TeamId,
    ) -> Result<bool, RepositoryError> {
        let row = sqlx::query(
            "SELECT 1 AS x FROM team_memberships \
             WHERE user_id = $1 AND team_id = $2 AND status = 'active' LIMIT 1",
        )
        .bind(user_id)
        .bind(team_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn count_active(&self, team_id: &TeamId) -> Result<u32, RepositoryError> {
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS c FROM team_memberships \
             WHERE team_id = $1 AND status = 'active'",
        )
        .bind(team_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let count: i64 = row.get("c");
        Ok(count as u32)
    }

    async fn revoke(&self, team_id: &TeamId, user_id: &str) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE team_memberships SET status = 'revoked', revoked_at = NOW() \
             WHERE team_id = $1 AND user_id = $2",
        )
        .bind(team_id.0)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
}

// ============================================================================
// PgTeamInvitationRepository
// ============================================================================

pub struct PgTeamInvitationRepository {
    pool: PgPool,
}

impl PgTeamInvitationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TeamInvitationRepository for PgTeamInvitationRepository {
    async fn save(&self, invitation: &TeamInvitation) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            INSERT INTO team_invitations (
                id, team_id, invitee_email, token_hash, status, expires_at,
                invited_by, created_at, accepted_at, cancelled_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                accepted_at = EXCLUDED.accepted_at,
                cancelled_at = EXCLUDED.cancelled_at
            "#,
        )
        .bind(invitation.id.0)
        .bind(invitation.team_id.0)
        .bind(&invitation.invitee_email)
        .bind(&invitation.token_hash)
        .bind(invitation.status.as_str())
        .bind(invitation.expires_at)
        .bind(&invitation.invited_by)
        .bind(invitation.created_at)
        .bind(invitation.accepted_at)
        .bind(invitation.cancelled_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &TeamInvitationId,
    ) -> Result<Option<TeamInvitation>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM team_invitations WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        row.as_ref().map(row_to_invitation).transpose()
    }

    async fn find_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<TeamInvitation>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM team_invitations WHERE token_hash = $1")
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        row.as_ref().map(row_to_invitation).transpose()
    }

    async fn find_pending_by_team(
        &self,
        team_id: &TeamId,
    ) -> Result<Vec<TeamInvitation>, RepositoryError> {
        let rows =
            sqlx::query("SELECT * FROM team_invitations WHERE team_id = $1 AND status = 'pending'")
                .bind(team_id.0)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        rows.iter().map(row_to_invitation).collect()
    }

    async fn mark_accepted(&self, id: &TeamInvitationId) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE team_invitations SET status = 'accepted', accepted_at = NOW() WHERE id = $1",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn mark_cancelled(&self, id: &TeamInvitationId) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE team_invitations SET status = 'cancelled', cancelled_at = NOW() WHERE id = $1",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn mark_expired(&self, id: &TeamInvitationId) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE team_invitations SET status = 'expired' WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
}
