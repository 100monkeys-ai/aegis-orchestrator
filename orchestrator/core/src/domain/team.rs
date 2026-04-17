// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Team Tenancy Domain (ADR-111)
//!
//! Domain model for [`Team`] — a first-class tenant kind that wraps a Keycloak
//! group (Pro/Business) or a dedicated realm (Enterprise) and owns a single
//! Stripe subscription whose seat quantity tracks active membership.
//!
//! ## Type Map
//!
//! | Type | Role |
//! |------|------|
//! | [`TeamId`] | UUID newtype — aggregate root identity |
//! | [`TeamSlug`] | User-facing slug of the form `t-{uuid}` |
//! | [`TeamInvitationId`] | UUID newtype — invitation identity |
//! | [`MembershipRole`] | Owner / Admin / Member |
//! | [`MembershipStatus`] | Active / Revoked |
//! | [`InvitationStatus`] | Pending / Accepted / Cancelled / Expired |
//! | [`Team`] | Aggregate root |
//! | [`Membership`] | Value object: `(team_id, user_id)` composite |
//! | [`TeamInvitation`] | Aggregate: pending → accepted / cancelled / expired |
//! | [`TeamEvent`] | Domain events published to the event bus |
//! | [`TeamRepository`] / [`MembershipRepository`] / [`TeamInvitationRepository`] | Persistence traits |

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::repository::RepositoryError;
use crate::domain::tenancy::TenantTier;
use crate::domain::tenant::TenantId;

// ============================================================================
// Value Object — Identity
// ============================================================================

/// Unique identifier for a [`Team`] aggregate root (ADR-111 §Domain Model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TeamId(pub Uuid);

impl TeamId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TeamId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TeamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a [`TeamInvitation`] aggregate (ADR-111 §Domain Model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TeamInvitationId(pub Uuid);

impl TeamInvitationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TeamInvitationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TeamInvitationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// User-facing slug of the form `t-{uuid}` (ADR-111 §Decision).
///
/// The slug is the public identifier for a team and is foreign-keyed into the
/// `tenants` table. Instances are created via [`TeamSlug::new`] from a
/// [`TeamId`], or via [`TeamSlug::parse`] when reconstructing from storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TeamSlug(String);

impl TeamSlug {
    /// Construct a team slug from a [`TeamId`]. Always produces a `t-{uuid}`
    /// string.
    pub fn new(id: TeamId) -> Self {
        Self(format!("t-{}", id.0))
    }

    /// Parse and validate a `t-{uuid}` slug. Returns `Err` if the string does
    /// not have the `t-` prefix or if the remainder is not a valid UUID.
    pub fn parse(s: &str) -> Result<Self, String> {
        let rest = s
            .strip_prefix("t-")
            .ok_or_else(|| format!("team slug must start with 't-': {s}"))?;
        Uuid::parse_str(rest).map_err(|e| format!("invalid team slug uuid: {e}"))?;
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert this slug to the matching [`TenantId`] used as the team's
    /// tenant anchor in the `tenants` table.
    pub fn to_tenant_id(&self) -> Result<TenantId, String> {
        TenantId::from_realm_slug(&self.0).map_err(|e| e.to_string())
    }
}

impl std::fmt::Display for TeamSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ============================================================================
// Value Object — Roles, Statuses
// ============================================================================

/// Role of a [`Membership`] within a team (ADR-111 §Domain Model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipRole {
    /// Exactly one per team; same user as [`Team::owner_user_id`]. Cannot be
    /// demoted through role change — owner transfer is a distinct operation.
    Owner,
    /// Can manage members and invitations; cannot change billing.
    Admin,
    /// Can access team resources; cannot manage membership.
    Member,
}

impl MembershipRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MembershipRole::Owner => "owner",
            MembershipRole::Admin => "admin",
            MembershipRole::Member => "member",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "owner" => Ok(MembershipRole::Owner),
            "admin" => Ok(MembershipRole::Admin),
            "member" => Ok(MembershipRole::Member),
            other => Err(format!("unknown membership role: {other}")),
        }
    }

    /// `true` if this role may manage membership (invite, kick, change role).
    /// Both Owner and Admin qualify.
    pub fn can_manage_membership(&self) -> bool {
        matches!(self, MembershipRole::Owner | MembershipRole::Admin)
    }
}

/// Lifecycle state of a [`Membership`] (ADR-111 §Domain Model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    Active,
    Revoked,
}

impl MembershipStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MembershipStatus::Active => "active",
            MembershipStatus::Revoked => "revoked",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "active" => Ok(MembershipStatus::Active),
            "revoked" => Ok(MembershipStatus::Revoked),
            other => Err(format!("unknown membership status: {other}")),
        }
    }
}

/// Lifecycle state of a [`TeamInvitation`] (ADR-111 §Domain Model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Cancelled,
    Expired,
}

impl InvitationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            InvitationStatus::Pending => "pending",
            InvitationStatus::Accepted => "accepted",
            InvitationStatus::Cancelled => "cancelled",
            InvitationStatus::Expired => "expired",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "pending" => Ok(InvitationStatus::Pending),
            "accepted" => Ok(InvitationStatus::Accepted),
            "cancelled" => Ok(InvitationStatus::Cancelled),
            "expired" => Ok(InvitationStatus::Expired),
            other => Err(format!("unknown invitation status: {other}")),
        }
    }
}

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events published by team-tenancy state transitions (ADR-111
/// §Domain Events).
///
/// Emitted by aggregate methods and drained via `take_events` at the aggregate
/// boundary for publication through the event bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamEvent {
    TeamProvisioned {
        team_id: TeamId,
        tenant_id: TenantId,
        owner_user_id: String,
        tier: TenantTier,
        provisioned_at: DateTime<Utc>,
    },
    InvitationSent {
        invitation_id: TeamInvitationId,
        team_id: TeamId,
        invitee_email: String,
        invited_by: String,
        sent_at: DateTime<Utc>,
    },
    InvitationAccepted {
        invitation_id: TeamInvitationId,
        team_id: TeamId,
        user_id: String,
        accepted_at: DateTime<Utc>,
    },
    InvitationCancelled {
        invitation_id: TeamInvitationId,
        team_id: TeamId,
        cancelled_by: String,
        cancelled_at: DateTime<Utc>,
    },
    InvitationExpired {
        invitation_id: TeamInvitationId,
        team_id: TeamId,
        expired_at: DateTime<Utc>,
    },
    MembershipRevoked {
        team_id: TeamId,
        user_id: String,
        revoked_by: String,
        revoked_at: DateTime<Utc>,
    },
    MembershipRoleChanged {
        team_id: TeamId,
        user_id: String,
        from_role: MembershipRole,
        to_role: MembershipRole,
        changed_by: String,
        changed_at: DateTime<Utc>,
    },
    SeatCountChanged {
        team_id: TeamId,
        previous_count: u32,
        new_count: u32,
        changed_at: DateTime<Utc>,
    },
    TenantContextSwitched {
        caller_user_id: String,
        from_tenant_id: TenantId,
        to_tenant_id: TenantId,
        /// The header that triggered the switch (always `"X-Tenant-Id"` for
        /// team-context switches — operator and service-account paths have
        /// their own audit trails).
        via_header: &'static str,
        switched_at: DateTime<Utc>,
    },
}

// ============================================================================
// Aggregate Root — Team
// ============================================================================

/// Aggregate root for a team tenant (ADR-111 §Domain Model).
///
/// A team wraps a [`TenantId`] (`t-{uuid}`) and owns the tier, owner, and
/// display metadata. The Stripe subscription is keyed on `tenant_id` via the
/// existing [`TenantSubscription`](crate::domain::billing::TenantSubscription)
/// record; seat synchronization is the responsibility of `BillingService`.
#[derive(Debug, Clone)]
pub struct Team {
    pub id: TeamId,
    pub slug: TeamSlug,
    pub display_name: String,
    pub owner_user_id: String,
    pub tier: TenantTier,
    pub tenant_id: TenantId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Event buffer. Drained via [`take_events`](Self::take_events) at the
    /// aggregate boundary.
    pub domain_events: Vec<TeamEvent>,
}

impl Team {
    /// Provision a new team. Rejects the Free tier (ADR-111 §Tier Gating).
    ///
    /// Emits a [`TeamEvent::TeamProvisioned`] event.
    pub fn provision(
        display_name: String,
        owner_user_id: String,
        tier: TenantTier,
    ) -> Result<Self, String> {
        if matches!(tier, TenantTier::Free) {
            return Err("teams cannot be provisioned on the Free tier".to_string());
        }
        if matches!(tier, TenantTier::System) {
            return Err("teams cannot be provisioned on the System tier".to_string());
        }
        if display_name.trim().is_empty() {
            return Err("team display_name must not be empty".to_string());
        }
        if owner_user_id.trim().is_empty() {
            return Err("team owner_user_id must not be empty".to_string());
        }

        let id = TeamId::new();
        let slug = TeamSlug::new(id);
        let tenant_id = slug.to_tenant_id()?;
        let now = Utc::now();

        let mut team = Self {
            id,
            slug,
            display_name,
            owner_user_id: owner_user_id.clone(),
            tier: tier.clone(),
            tenant_id: tenant_id.clone(),
            created_at: now,
            updated_at: now,
            domain_events: Vec::new(),
        };
        team.domain_events.push(TeamEvent::TeamProvisioned {
            team_id: id,
            tenant_id,
            owner_user_id,
            tier,
            provisioned_at: now,
        });
        Ok(team)
    }

    /// Drain and return the buffered [`TeamEvent`]s. The application service
    /// calls this at the aggregate boundary to publish them.
    pub fn take_events(&mut self) -> Vec<TeamEvent> {
        std::mem::take(&mut self.domain_events)
    }

    /// Return the tier-specific seat cap (ADR-111 §Tier Gating).
    ///
    /// - Pro: 5
    /// - Business: 25
    /// - Enterprise: unlimited (`u32::MAX`)
    /// - Free / System: 0 (no seats — teams cannot exist on these tiers)
    pub fn max_seats(&self) -> u32 {
        match self.tier {
            TenantTier::Pro => 5,
            TenantTier::Business => 25,
            TenantTier::Enterprise => u32::MAX,
            TenantTier::Free | TenantTier::System => 0,
        }
    }
}

// ============================================================================
// Value Object — Membership
// ============================================================================

/// Membership of a user in a team (ADR-111 §Domain Model).
///
/// Identified by the composite key `(team_id, user_id)`. Not an aggregate
/// root — its lifecycle is driven from the [`Team`] aggregate via the
/// application service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Membership {
    pub team_id: TeamId,
    pub user_id: String,
    pub role: MembershipRole,
    pub status: MembershipStatus,
    pub joined_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Membership {
    /// Construct an active membership.
    pub fn new_active(team_id: TeamId, user_id: String, role: MembershipRole) -> Self {
        Self {
            team_id,
            user_id,
            role,
            status: MembershipStatus::Active,
            joined_at: Utc::now(),
            revoked_at: None,
        }
    }
}

// ============================================================================
// Aggregate — TeamInvitation
// ============================================================================

/// Team invitation aggregate (ADR-111 §Domain Model).
///
/// Lifecycle: `Pending → Accepted | Cancelled | Expired` (single transition
/// from `Pending`). Transitions are guarded by the methods below.
#[derive(Debug, Clone)]
pub struct TeamInvitation {
    pub id: TeamInvitationId,
    pub team_id: TeamId,
    pub invitee_email: String,
    /// HMAC-SHA256 digest of the raw invitation token, bound to
    /// `(team_id, invitee_email)`. Only the digest is stored; the raw token is
    /// delivered once in the outbound email.
    pub token_hash: String,
    pub status: InvitationStatus,
    pub expires_at: DateTime<Utc>,
    pub invited_by: String,
    pub created_at: DateTime<Utc>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    /// Event buffer. Drained via [`take_events`](Self::take_events) at the
    /// aggregate boundary.
    pub domain_events: Vec<TeamEvent>,
}

impl TeamInvitation {
    /// Construct a fresh [`InvitationStatus::Pending`] invitation and buffer
    /// a [`TeamEvent::InvitationSent`] event.
    pub fn send(
        team_id: TeamId,
        invitee_email: String,
        token_hash: String,
        invited_by: String,
        expires_at: DateTime<Utc>,
    ) -> Self {
        let id = TeamInvitationId::new();
        let now = Utc::now();
        let mut inv = Self {
            id,
            team_id,
            invitee_email: invitee_email.clone(),
            token_hash,
            status: InvitationStatus::Pending,
            expires_at,
            invited_by: invited_by.clone(),
            created_at: now,
            accepted_at: None,
            cancelled_at: None,
            domain_events: Vec::new(),
        };
        inv.domain_events.push(TeamEvent::InvitationSent {
            invitation_id: id,
            team_id,
            invitee_email,
            invited_by,
            sent_at: now,
        });
        inv
    }

    /// Transition `Pending → Accepted`. Guarded: fails if not Pending or
    /// expired.
    pub fn accept(&mut self, user_id: String) -> Result<(), String> {
        if self.status != InvitationStatus::Pending {
            return Err(format!(
                "invitation is not pending (status={})",
                self.status.as_str()
            ));
        }
        let now = Utc::now();
        if now >= self.expires_at {
            return Err("invitation has expired".to_string());
        }
        self.status = InvitationStatus::Accepted;
        self.accepted_at = Some(now);
        self.domain_events.push(TeamEvent::InvitationAccepted {
            invitation_id: self.id,
            team_id: self.team_id,
            user_id,
            accepted_at: now,
        });
        Ok(())
    }

    /// Transition `Pending → Cancelled`. Guarded: fails if not Pending.
    pub fn cancel(&mut self, cancelled_by: String) -> Result<(), String> {
        if self.status != InvitationStatus::Pending {
            return Err(format!(
                "invitation is not pending (status={})",
                self.status.as_str()
            ));
        }
        let now = Utc::now();
        self.status = InvitationStatus::Cancelled;
        self.cancelled_at = Some(now);
        self.domain_events.push(TeamEvent::InvitationCancelled {
            invitation_id: self.id,
            team_id: self.team_id,
            cancelled_by,
            cancelled_at: now,
        });
        Ok(())
    }

    /// Transition `Pending → Expired`. Guarded: fails if not Pending.
    pub fn expire(&mut self) -> Result<(), String> {
        if self.status != InvitationStatus::Pending {
            return Err(format!(
                "invitation is not pending (status={})",
                self.status.as_str()
            ));
        }
        let now = Utc::now();
        self.status = InvitationStatus::Expired;
        self.domain_events.push(TeamEvent::InvitationExpired {
            invitation_id: self.id,
            team_id: self.team_id,
            expired_at: now,
        });
        Ok(())
    }

    /// Drain and return the buffered [`TeamEvent`]s.
    pub fn take_events(&mut self) -> Vec<TeamEvent> {
        std::mem::take(&mut self.domain_events)
    }

    /// `true` if the invitation is pending and not yet past its expiry.
    pub fn is_redeemable(&self) -> bool {
        self.status == InvitationStatus::Pending && Utc::now() < self.expires_at
    }
}

// ============================================================================
// Repository Traits
// ============================================================================

/// Persistence interface for the [`Team`] aggregate (ADR-111 §Domain Model).
#[async_trait]
pub trait TeamRepository: Send + Sync {
    async fn save(&self, team: &Team) -> Result<(), RepositoryError>;
    async fn find_by_id(&self, id: &TeamId) -> Result<Option<Team>, RepositoryError>;
    async fn find_by_slug(&self, slug: &TeamSlug) -> Result<Option<Team>, RepositoryError>;
    async fn find_by_owner(&self, owner_user_id: &str) -> Result<Vec<Team>, RepositoryError>;
    async fn delete(&self, id: &TeamId) -> Result<(), RepositoryError>;
}

/// Persistence interface for [`Membership`] value objects (ADR-111).
///
/// Membership is identified by the composite key `(team_id, user_id)` and
/// never lives outside a team context. This trait is the authorization
/// primitive for `X-Tenant-Id` team-context resolution — see
/// [`is_active_member`](MembershipRepository::is_active_member).
#[async_trait]
pub trait MembershipRepository: Send + Sync {
    async fn save(&self, membership: &Membership) -> Result<(), RepositoryError>;
    async fn find_by_team(&self, team_id: &TeamId) -> Result<Vec<Membership>, RepositoryError>;
    async fn find_by_user(&self, user_id: &str) -> Result<Vec<Membership>, RepositoryError>;

    /// Authorization primitive for team-context `X-Tenant-Id` switching.
    /// Returns `true` iff the user currently has an Active membership on the
    /// given team.
    async fn is_active_member(
        &self,
        user_id: &str,
        team_id: &TeamId,
    ) -> Result<bool, RepositoryError>;

    /// Count the active memberships for a team (ADR-111 §Billing Model —
    /// drives Stripe seat synchronization).
    async fn count_active(&self, team_id: &TeamId) -> Result<u32, RepositoryError>;

    /// Transition a membership to [`MembershipStatus::Revoked`].
    async fn revoke(&self, team_id: &TeamId, user_id: &str) -> Result<(), RepositoryError>;
}

/// Persistence interface for the [`TeamInvitation`] aggregate (ADR-111).
#[async_trait]
pub trait TeamInvitationRepository: Send + Sync {
    async fn save(&self, invitation: &TeamInvitation) -> Result<(), RepositoryError>;
    async fn find_by_id(
        &self,
        id: &TeamInvitationId,
    ) -> Result<Option<TeamInvitation>, RepositoryError>;
    async fn find_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<TeamInvitation>, RepositoryError>;
    async fn find_pending_by_team(
        &self,
        team_id: &TeamId,
    ) -> Result<Vec<TeamInvitation>, RepositoryError>;
    async fn mark_accepted(&self, id: &TeamInvitationId) -> Result<(), RepositoryError>;
    async fn mark_cancelled(&self, id: &TeamInvitationId) -> Result<(), RepositoryError>;
    async fn mark_expired(&self, id: &TeamInvitationId) -> Result<(), RepositoryError>;
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_roundtrip() {
        let id = TeamId::new();
        let slug = TeamSlug::new(id);
        let parsed = TeamSlug::parse(slug.as_str()).unwrap();
        assert_eq!(slug, parsed);
        assert!(slug.as_str().starts_with("t-"));
    }

    #[test]
    fn slug_rejects_wrong_prefix() {
        assert!(TeamSlug::parse("u-12345").is_err());
        assert!(TeamSlug::parse("bogus").is_err());
    }

    #[test]
    fn provision_rejects_free_tier() {
        let err = Team::provision("My Team".into(), "user-1".into(), TenantTier::Free);
        assert!(err.is_err());
    }

    #[test]
    fn provision_emits_team_provisioned() {
        let mut team = Team::provision("My Team".into(), "user-1".into(), TenantTier::Pro).unwrap();
        assert_eq!(team.tier, TenantTier::Pro);
        assert!(team.slug.as_str().starts_with("t-"));
        let events = team.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TeamEvent::TeamProvisioned { .. }));
    }

    #[test]
    fn max_seats_matches_tier_gating() {
        let pro = Team::provision("p".into(), "u".into(), TenantTier::Pro).unwrap();
        let biz = Team::provision("b".into(), "u".into(), TenantTier::Business).unwrap();
        let ent = Team::provision("e".into(), "u".into(), TenantTier::Enterprise).unwrap();
        assert_eq!(pro.max_seats(), 5);
        assert_eq!(biz.max_seats(), 25);
        assert_eq!(ent.max_seats(), u32::MAX);
    }

    #[test]
    fn invitation_accept_from_pending_emits_event() {
        let team_id = TeamId::new();
        let mut inv = TeamInvitation::send(
            team_id,
            "alice@example.com".into(),
            "hash-a".into(),
            "owner-1".into(),
            Utc::now() + chrono::Duration::days(7),
        );
        let _sent = inv.take_events();
        inv.accept("alice".into()).unwrap();
        let events = inv.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TeamEvent::InvitationAccepted { .. }));
        assert_eq!(inv.status, InvitationStatus::Accepted);
    }

    #[test]
    fn invitation_accept_rejects_expired() {
        let team_id = TeamId::new();
        let mut inv = TeamInvitation::send(
            team_id,
            "alice@example.com".into(),
            "hash-a".into(),
            "owner-1".into(),
            Utc::now() - chrono::Duration::days(1),
        );
        assert!(inv.accept("alice".into()).is_err());
    }

    #[test]
    fn invitation_double_transition_rejected() {
        let team_id = TeamId::new();
        let mut inv = TeamInvitation::send(
            team_id,
            "alice@example.com".into(),
            "hash-a".into(),
            "owner-1".into(),
            Utc::now() + chrono::Duration::days(7),
        );
        inv.cancel("owner-1".into()).unwrap();
        assert!(inv.accept("alice".into()).is_err());
        assert!(inv.expire().is_err());
    }

    #[test]
    fn membership_role_manage_gates() {
        assert!(MembershipRole::Owner.can_manage_membership());
        assert!(MembershipRole::Admin.can_manage_membership());
        assert!(!MembershipRole::Member.can_manage_membership());
    }
}
