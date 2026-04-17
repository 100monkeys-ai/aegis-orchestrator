// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Team Tenancy Application Service (ADR-111)
//!
//! Orchestrates the [`Team`] / [`Membership`] / [`TeamInvitation`] aggregate
//! lifecycle for the Colony (Zaru Client Phase 5) product surface. Wires the
//! team domain aggregates to:
//!
//! - [`TeamRepository`] / [`MembershipRepository`] / [`TeamInvitationRepository`]
//! - [`TenantRepository`] — inserts the backing `tenants` row (ADR-056)
//! - [`BillingService`] — Stripe customer provisioning + seat synchronization
//! - [`EventBus`] — publishes [`TeamEvent`]s
//!
//! ## Bounded Context
//!
//! BC-1 Agent Lifecycle shares the tenancy boundary with this service —
//! team-tenancy is a tenant-lifecycle concern that happens to be driven by
//! human invitation rather than operator provisioning (contrast with
//! [`tenant_onboarding`](super::tenant_onboarding)).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::application::billing_service::{BillingService, BillingServiceError};
use crate::domain::repository::{RepositoryError, TenantRepository};
use crate::domain::team::{
    InvitationStatus, Membership, MembershipRepository, MembershipRole, Team, TeamEvent, TeamId,
    TeamInvitation, TeamInvitationId, TeamInvitationRepository, TeamRepository,
};
use crate::domain::tenancy::{Tenant, TenantTier};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;

type HmacSha256 = Hmac<Sha256>;

// ============================================================================
// Errors
// ============================================================================

/// Error type returned by [`TeamService`] methods (ADR-111 §Application
/// Service).
#[derive(Debug, thiserror::Error)]
pub enum TeamServiceError {
    /// The caller's tier does not permit team creation (Free rejected).
    #[error("tier does not permit team creation: {0:?}")]
    TierNotPermitted(TenantTier),
    /// The team's seat cap for the current tier has been reached.
    #[error("seat cap reached for tier (limit={limit}, current={current})")]
    SeatCapReached { limit: u32, current: u32 },
    /// The caller is not authorized to perform this action on the team
    /// (typically: not an Owner or Admin).
    #[error("caller is not authorized for this team action")]
    Unauthorized,
    /// The target team, invitation, or membership does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The invitation token does not match any pending invitation, or the
    /// authenticated email does not match the invitation's invitee.
    #[error("invalid invitation token")]
    InvalidInvitation,
    /// The invitation has expired.
    #[error("invitation has expired")]
    InvitationExpired,
    /// The invitation is not in [`InvitationStatus::Pending`].
    #[error("invitation is not pending")]
    InvitationNotPending,
    /// Owner role cannot be modified through [`TeamService::update_role`] —
    /// it requires a distinct owner-transfer flow (out of scope for Phase 1).
    #[error("owner role cannot be reassigned through role change")]
    OwnerRoleImmutable,
    /// Invalid command payload (e.g. empty display_name).
    #[error("invalid command: {0}")]
    InvalidCommand(String),
    /// Underlying aggregate invariant rejected.
    #[error("domain invariant violation: {0}")]
    Domain(String),
    /// Repository-layer error.
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
    /// Billing service error.
    #[error("billing error: {0}")]
    Billing(#[from] BillingServiceError),
    /// Team invitations are disabled because no HMAC key is configured.
    #[error("team invitations are not configured")]
    InvitationsNotConfigured,
}

// ============================================================================
// Commands
// ============================================================================

/// Command to provision a new team (ADR-111 §Decision).
#[derive(Debug, Clone)]
pub struct ProvisionTeamCommand {
    pub display_name: String,
    pub owner_user_id: String,
    pub owner_email: String,
    pub tier: TenantTier,
}

/// Command to invite a user to a team (ADR-111 §Invitation Flow).
#[derive(Debug, Clone)]
pub struct InviteMemberCommand {
    pub team_id: TeamId,
    pub invitee_email: String,
    pub invited_by_user_id: String,
}

/// Command to accept an invitation.
#[derive(Debug, Clone)]
pub struct AcceptInvitationCommand {
    /// Raw invitation token from the email link.
    pub token: String,
    /// Email address of the authenticated caller — must match the
    /// invitation's invitee (case-insensitive).
    pub authenticated_email: String,
    /// Keycloak `sub` of the authenticated caller.
    pub authenticated_user_id: String,
}

/// Outcome of a successful invitation send. Exposes the raw token so a Phase
/// 2 email pipeline can deliver it; the orchestrator never re-reads it after
/// this point.
#[derive(Debug, Clone)]
pub struct InvitationIssued {
    pub invitation_id: TeamInvitationId,
    pub team_id: TeamId,
    pub invitee_email: String,
    pub raw_token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

// ============================================================================
// Service Trait
// ============================================================================

/// Primary interface for managing team tenants (ADR-111 §Application Service).
#[async_trait]
pub trait TeamService: Send + Sync {
    /// Provision a new team: creates the [`Tenant`] row, the [`Team`]
    /// aggregate, the Owner [`Membership`], and the Stripe Customer.
    /// Publishes [`TeamEvent::TeamProvisioned`].
    async fn provision_team(&self, cmd: ProvisionTeamCommand) -> Result<Team, TeamServiceError>;

    /// Issue a new invitation. Caller must be an Owner or Admin of the team
    /// and the team must be below its per-tier seat cap. Publishes
    /// [`TeamEvent::InvitationSent`] and returns the raw token for out-of-band
    /// email delivery.
    async fn invite_member(
        &self,
        cmd: InviteMemberCommand,
    ) -> Result<InvitationIssued, TeamServiceError>;

    /// Accept a pending invitation. Verifies the token, creates an Active
    /// membership, and synchronizes Stripe seats. Publishes
    /// [`TeamEvent::InvitationAccepted`] and [`TeamEvent::SeatCountChanged`]
    /// if the seat count actually changed.
    async fn accept_invitation(
        &self,
        cmd: AcceptInvitationCommand,
    ) -> Result<Membership, TeamServiceError>;

    /// Cancel a pending invitation. Caller must be an Owner or Admin of the
    /// team.
    async fn cancel_invitation(
        &self,
        invitation_id: TeamInvitationId,
        actor_user_id: String,
    ) -> Result<(), TeamServiceError>;

    /// Revoke an active membership. Caller must be an Owner or Admin of the
    /// team; Owner role cannot be revoked.
    async fn revoke_membership(
        &self,
        team_id: TeamId,
        user_id: String,
        actor_user_id: String,
    ) -> Result<(), TeamServiceError>;

    /// Change a member's role. Caller must be an Owner or Admin of the team;
    /// Owner role cannot be demoted through this path.
    async fn update_role(
        &self,
        team_id: TeamId,
        user_id: String,
        new_role: MembershipRole,
        actor_user_id: String,
    ) -> Result<(), TeamServiceError>;

    /// List all memberships (active and revoked) for a user across every team
    /// they belong to.
    async fn list_memberships_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<Membership>, TeamServiceError>;
}

// ============================================================================
// Standard Implementation
// ============================================================================

/// Production implementation of [`TeamService`].
pub struct StandardTeamService {
    team_repo: Arc<dyn TeamRepository>,
    membership_repo: Arc<dyn MembershipRepository>,
    invitation_repo: Arc<dyn TeamInvitationRepository>,
    tenant_repo: Arc<dyn TenantRepository>,
    billing_service: Arc<dyn BillingService>,
    event_bus: Arc<EventBus>,
    /// Raw HMAC key bytes used to derive invitation token hashes. `None` when
    /// `BillingConfig.invitation_hmac_key` is absent — invitation operations
    /// then return [`TeamServiceError::InvitationsNotConfigured`].
    invitation_hmac_key: Option<Vec<u8>>,
}

impl StandardTeamService {
    pub fn new(
        team_repo: Arc<dyn TeamRepository>,
        membership_repo: Arc<dyn MembershipRepository>,
        invitation_repo: Arc<dyn TeamInvitationRepository>,
        tenant_repo: Arc<dyn TenantRepository>,
        billing_service: Arc<dyn BillingService>,
        event_bus: Arc<EventBus>,
        invitation_hmac_key: Option<Vec<u8>>,
    ) -> Self {
        Self {
            team_repo,
            membership_repo,
            invitation_repo,
            tenant_repo,
            billing_service,
            event_bus,
            invitation_hmac_key,
        }
    }

    /// Compute `HMAC-SHA256(key, team_id || ':' || invitee_email)`, returning
    /// the hex-encoded digest. This is both the raw token and the stored
    /// hash — because the "secret" is the HMAC key, an attacker who only sees
    /// the `token_hash` column cannot forge a valid token without the key.
    ///
    /// NOTE: Re-computing the hash for verification requires the caller's
    /// email and team_id; the raw token delivered in the email is therefore
    /// the hex digest itself. This matches ADR-111 §Invitation Flow's
    /// `token = HMAC-SHA256(secret, team_id || invitee_email)` statement and
    /// the `token_hash` stored column.
    fn token_for(&self, team_id: TeamId, invitee_email: &str) -> Result<String, TeamServiceError> {
        let key = self
            .invitation_hmac_key
            .as_deref()
            .ok_or(TeamServiceError::InvitationsNotConfigured)?;
        let mut mac = HmacSha256::new_from_slice(key)
            .map_err(|e| TeamServiceError::Domain(format!("hmac init failed: {e}")))?;
        mac.update(team_id.to_string().as_bytes());
        mac.update(b":");
        mac.update(invitee_email.to_ascii_lowercase().as_bytes());
        let bytes = mac.finalize().into_bytes();
        Ok(hex::encode(bytes))
    }

    async fn ensure_manager(
        &self,
        team_id: TeamId,
        actor_user_id: &str,
    ) -> Result<Membership, TeamServiceError> {
        let memberships = self.membership_repo.find_by_team(&team_id).await?;
        memberships
            .into_iter()
            .find(|m| m.user_id == actor_user_id && m.role.can_manage_membership())
            .ok_or(TeamServiceError::Unauthorized)
    }

    fn publish_team_events(&self, events: Vec<TeamEvent>) {
        for event in events {
            self.event_bus.publish_team_event(event);
        }
    }
}

#[async_trait]
impl TeamService for StandardTeamService {
    async fn provision_team(&self, cmd: ProvisionTeamCommand) -> Result<Team, TeamServiceError> {
        if matches!(cmd.tier, TenantTier::Free | TenantTier::System) {
            return Err(TeamServiceError::TierNotPermitted(cmd.tier));
        }

        let mut team = Team::provision(
            cmd.display_name.clone(),
            cmd.owner_user_id.clone(),
            cmd.tier.clone(),
        )
        .map_err(TeamServiceError::InvalidCommand)?;

        // Insert the backing tenants row (ADR-056). The Keycloak realm is
        // `zaru-consumer` for Pro/Business (group-scoped) and `team-{slug}`
        // for Enterprise (dedicated realm) — see ADR-111 §Keycloak Strategy.
        // Phase 1 inserts the row with the tier-driven realm name but does
        // not create the Keycloak group/realm yet; Phase 2 wires that.
        let keycloak_realm = match team.tier {
            TenantTier::Enterprise => format!("team-{}", team.slug.as_str()),
            _ => "zaru-consumer".to_string(),
        };
        let openbao_namespace = format!("tenant-{}", team.tenant_id.as_str());
        let tenant_row = Tenant::new(
            team.tenant_id.clone(),
            cmd.display_name.clone(),
            team.tier.clone(),
            keycloak_realm,
            openbao_namespace,
        );
        self.tenant_repo.insert(&tenant_row).await?;

        // Persist the team aggregate.
        self.team_repo.save(&team).await?;

        // Insert the Owner membership.
        let owner_membership =
            Membership::new_active(team.id, cmd.owner_user_id.clone(), MembershipRole::Owner);
        self.membership_repo.save(&owner_membership).await?;

        // Provision the Stripe customer + TenantSubscription row.
        self.billing_service
            .provision_team_customer(team.id, cmd.owner_email, &team.tenant_id, team.tier.clone())
            .await?;

        // Drain events & publish.
        let events = team.take_events();
        self.publish_team_events(events);

        Ok(team)
    }

    async fn invite_member(
        &self,
        cmd: InviteMemberCommand,
    ) -> Result<InvitationIssued, TeamServiceError> {
        // Authorization: caller must be Owner/Admin.
        let _actor = self
            .ensure_manager(cmd.team_id, &cmd.invited_by_user_id)
            .await?;

        let team = self
            .team_repo
            .find_by_id(&cmd.team_id)
            .await?
            .ok_or_else(|| TeamServiceError::NotFound(format!("team {}", cmd.team_id)))?;

        // Seat-cap enforcement against the tier limit (ADR-111 §Tier Gating).
        // Pending invitations count toward the cap so we don't oversell.
        let current_active = self.membership_repo.count_active(&cmd.team_id).await?;
        let pending = self
            .invitation_repo
            .find_pending_by_team(&cmd.team_id)
            .await?;
        let projected = current_active
            .saturating_add(pending.len() as u32)
            .saturating_add(1);
        let limit = team.max_seats();
        if projected > limit {
            return Err(TeamServiceError::SeatCapReached {
                limit,
                current: current_active,
            });
        }

        // Normalize email for token binding and storage.
        let email_lower = cmd.invitee_email.trim().to_ascii_lowercase();
        let token = self.token_for(cmd.team_id, &email_lower)?;
        let token_hash = token.clone();
        let expires_at = Utc::now() + Duration::days(7);

        let mut invitation = TeamInvitation::send(
            cmd.team_id,
            email_lower.clone(),
            token_hash,
            cmd.invited_by_user_id.clone(),
            expires_at,
        );
        self.invitation_repo.save(&invitation).await?;
        let events = invitation.take_events();
        self.publish_team_events(events);

        // TODO Phase 2: wire email pipeline to deliver `token` to `email_lower`.
        Ok(InvitationIssued {
            invitation_id: invitation.id,
            team_id: cmd.team_id,
            invitee_email: email_lower,
            raw_token: token,
            expires_at,
        })
    }

    async fn accept_invitation(
        &self,
        cmd: AcceptInvitationCommand,
    ) -> Result<Membership, TeamServiceError> {
        let invitation = self
            .invitation_repo
            .find_by_token_hash(&cmd.token)
            .await?
            .ok_or(TeamServiceError::InvalidInvitation)?;

        if invitation.status != InvitationStatus::Pending {
            return Err(TeamServiceError::InvitationNotPending);
        }

        // Email binding: the token is HMAC'd over `(team_id, invitee_email)`,
        // so re-deriving the token and comparing is structurally equivalent,
        // but we still check the stored email here as a defense-in-depth
        // explicit check (and to give a clear error).
        if !invitation
            .invitee_email
            .eq_ignore_ascii_case(&cmd.authenticated_email)
        {
            return Err(TeamServiceError::InvalidInvitation);
        }

        if Utc::now() >= invitation.expires_at {
            return Err(TeamServiceError::InvitationExpired);
        }

        // Reconstruct the expected token and verify in constant time.
        let expected = self.token_for(
            invitation.team_id,
            &invitation.invitee_email.to_ascii_lowercase(),
        )?;
        if !constant_time_eq(expected.as_bytes(), cmd.token.as_bytes()) {
            return Err(TeamServiceError::InvalidInvitation);
        }

        // Mutate & persist invitation.
        let mut invitation = invitation;
        invitation
            .accept(cmd.authenticated_user_id.clone())
            .map_err(TeamServiceError::Domain)?;
        self.invitation_repo.mark_accepted(&invitation.id).await?;

        // Insert the new Active membership.
        let membership = Membership::new_active(
            invitation.team_id,
            cmd.authenticated_user_id.clone(),
            MembershipRole::Member,
        );
        self.membership_repo.save(&membership).await?;

        // Resync Stripe seats.
        let team = self
            .team_repo
            .find_by_id(&invitation.team_id)
            .await?
            .ok_or_else(|| TeamServiceError::NotFound(format!("team {}", invitation.team_id)))?;
        let new_count = self
            .membership_repo
            .count_active(&invitation.team_id)
            .await?;
        let previous = self
            .billing_service
            .sync_seats(team.id, &team.tenant_id, new_count)
            .await?;

        // Drain invitation events + emit SeatCountChanged if the count moved.
        let mut events = invitation.take_events();
        if previous != new_count {
            events.push(TeamEvent::SeatCountChanged {
                team_id: team.id,
                previous_count: previous,
                new_count,
                changed_at: Utc::now(),
            });
        }
        self.publish_team_events(events);

        Ok(membership)
    }

    async fn cancel_invitation(
        &self,
        invitation_id: TeamInvitationId,
        actor_user_id: String,
    ) -> Result<(), TeamServiceError> {
        let invitation = self
            .invitation_repo
            .find_by_id(&invitation_id)
            .await?
            .ok_or_else(|| TeamServiceError::NotFound(format!("invitation {invitation_id}")))?;
        let _actor = self
            .ensure_manager(invitation.team_id, &actor_user_id)
            .await?;

        let mut invitation = invitation;
        invitation
            .cancel(actor_user_id)
            .map_err(TeamServiceError::Domain)?;
        self.invitation_repo.mark_cancelled(&invitation.id).await?;
        let events = invitation.take_events();
        self.publish_team_events(events);
        Ok(())
    }

    async fn revoke_membership(
        &self,
        team_id: TeamId,
        user_id: String,
        actor_user_id: String,
    ) -> Result<(), TeamServiceError> {
        let _actor = self.ensure_manager(team_id, &actor_user_id).await?;

        let memberships = self.membership_repo.find_by_team(&team_id).await?;
        let target = memberships
            .iter()
            .find(|m| m.user_id == user_id)
            .ok_or_else(|| TeamServiceError::NotFound(format!("membership {user_id}")))?;
        if target.role == MembershipRole::Owner {
            return Err(TeamServiceError::OwnerRoleImmutable);
        }

        self.membership_repo.revoke(&team_id, &user_id).await?;

        let team = self
            .team_repo
            .find_by_id(&team_id)
            .await?
            .ok_or_else(|| TeamServiceError::NotFound(format!("team {team_id}")))?;
        let new_count = self.membership_repo.count_active(&team_id).await?;
        let previous = self
            .billing_service
            .sync_seats(team.id, &team.tenant_id, new_count)
            .await?;

        let now = Utc::now();
        let mut events = vec![TeamEvent::MembershipRevoked {
            team_id,
            user_id: user_id.clone(),
            revoked_by: actor_user_id,
            revoked_at: now,
        }];
        if previous != new_count {
            events.push(TeamEvent::SeatCountChanged {
                team_id,
                previous_count: previous,
                new_count,
                changed_at: now,
            });
        }
        self.publish_team_events(events);
        Ok(())
    }

    async fn update_role(
        &self,
        team_id: TeamId,
        user_id: String,
        new_role: MembershipRole,
        actor_user_id: String,
    ) -> Result<(), TeamServiceError> {
        let _actor = self.ensure_manager(team_id, &actor_user_id).await?;

        let memberships = self.membership_repo.find_by_team(&team_id).await?;
        let target = memberships
            .into_iter()
            .find(|m| m.user_id == user_id)
            .ok_or_else(|| TeamServiceError::NotFound(format!("membership {user_id}")))?;

        if target.role == MembershipRole::Owner || new_role == MembershipRole::Owner {
            return Err(TeamServiceError::OwnerRoleImmutable);
        }

        let previous_role = target.role;
        let updated = Membership {
            role: new_role,
            ..target
        };
        self.membership_repo.save(&updated).await?;

        self.publish_team_events(vec![TeamEvent::MembershipRoleChanged {
            team_id,
            user_id,
            from_role: previous_role,
            to_role: new_role,
            changed_by: actor_user_id,
            changed_at: Utc::now(),
        }]);
        Ok(())
    }

    async fn list_memberships_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<Membership>, TeamServiceError> {
        Ok(self.membership_repo.find_by_user(user_id).await?)
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}
