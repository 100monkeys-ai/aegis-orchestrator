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
use crate::application::effective_tier_service::EffectiveTierService;
use crate::domain::repository::{RepositoryError, TenantRepository};
use crate::domain::team::{
    InvitationStatus, Membership, MembershipRepository, MembershipRole, Team, TeamEvent, TeamId,
    TeamInvitation, TeamInvitationId, TeamInvitationRepository, TeamRepository,
};
use crate::domain::tenancy::{Tenant, TenantTier};
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::iam::keycloak_admin_client::KeycloakAdminClient;

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
    /// The caller's *personal* tier does not meet the minimum required to
    /// own a colony at the requested tier (ADR-111 §Tier Gating).
    #[error("personal tier {personal:?} cannot own a colony at {requested:?}; upgrade to Business or Enterprise")]
    PersonalTierTooLow {
        personal: TenantTier,
        requested: TenantTier,
    },
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

    /// List pending invitations for a team. Caller must be an Owner or Admin
    /// (enforced by callers via `ensure_manager`-equivalent checks — this
    /// method is currently a thin read-through, with authorization handled at
    /// the HTTP handler layer that already resolved the active team).
    async fn list_pending_invitations(
        &self,
        team_id: TeamId,
    ) -> Result<Vec<TeamInvitation>, TeamServiceError>;
}

// ============================================================================
// TeamMembershipsSyncPort — write the `team_memberships` Keycloak attribute
// ============================================================================

/// Port that stamps the multi-valued `team_memberships` attribute on a
/// Keycloak user. The attribute carries the list of `t-{uuid}` tenant slugs
/// the user is currently an active member of; the upstream MCP middleware
/// reads it off the JWT to authorize team-tenant tool calls and fails closed
/// with 403 when an entry is missing.
///
/// Production wiring is [`KeycloakTeamMembershipsSyncPort`]; tests provide an
/// in-memory recorder.
#[async_trait]
pub trait TeamMembershipsSyncPort: Send + Sync {
    /// Replace the user's `team_memberships` attribute with `tenants`.
    /// An empty slice clears the attribute (no active memberships).
    /// Implementations MUST be idempotent.
    async fn set_team_memberships(
        &self,
        user_id: &str,
        tenants: &[String],
    ) -> Result<(), TeamServiceError>;
}

/// Keycloak-backed implementation that writes the attribute in the shared
/// `zaru-consumer` realm. Enterprise team realms are out of scope for this
/// port: the MCP middleware authorizes against the consumer JWT.
pub struct KeycloakTeamMembershipsSyncPort {
    keycloak_admin: Arc<KeycloakAdminClient>,
    realm: String,
}

impl KeycloakTeamMembershipsSyncPort {
    pub fn new(keycloak_admin: Arc<KeycloakAdminClient>) -> Self {
        Self {
            keycloak_admin,
            realm: "zaru-consumer".to_string(),
        }
    }

    pub fn with_realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = realm.into();
        self
    }
}

#[async_trait]
impl TeamMembershipsSyncPort for KeycloakTeamMembershipsSyncPort {
    async fn set_team_memberships(
        &self,
        user_id: &str,
        tenants: &[String],
    ) -> Result<(), TeamServiceError> {
        self.keycloak_admin
            .set_user_team_memberships(&self.realm, user_id, tenants)
            .await
            .map_err(|e| {
                TeamServiceError::Repository(RepositoryError::Unknown(format!(
                    "set_user_team_memberships: {e}"
                )))
            })
    }
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
    /// Effective tier synchronizer (ADR-111 Phase 3). When set, membership
    /// transitions (accept / revoke) trigger a recompute + Keycloak sync for
    /// the affected user. Optional so the service works in environments
    /// without Keycloak wired (e.g. tests).
    effective_tier_service: Option<Arc<dyn EffectiveTierService>>,
    /// `team_memberships` JWT claim synchronizer. When set, every membership
    /// mutation (provision_team / accept_invitation / revoke_membership)
    /// recomputes the affected user's active team-tenant slug list and
    /// stamps it on their Keycloak user record. Optional so the service
    /// works in environments without Keycloak wired (e.g. tests).
    team_memberships_sync: Option<Arc<dyn TeamMembershipsSyncPort>>,
}

impl StandardTeamService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        team_repo: Arc<dyn TeamRepository>,
        membership_repo: Arc<dyn MembershipRepository>,
        invitation_repo: Arc<dyn TeamInvitationRepository>,
        tenant_repo: Arc<dyn TenantRepository>,
        billing_service: Arc<dyn BillingService>,
        event_bus: Arc<EventBus>,
        invitation_hmac_key: Option<Vec<u8>>,
        effective_tier_service: Option<Arc<dyn EffectiveTierService>>,
        team_memberships_sync: Option<Arc<dyn TeamMembershipsSyncPort>>,
    ) -> Self {
        Self {
            team_repo,
            membership_repo,
            invitation_repo,
            tenant_repo,
            billing_service,
            event_bus,
            invitation_hmac_key,
            effective_tier_service,
            team_memberships_sync,
        }
    }

    /// Recompute the user's active team-tenant list from the membership
    /// repository and stamp it via [`TeamMembershipsSyncPort`]. No-op when
    /// the port is not wired. Failures are logged but never abort the
    /// caller's workflow — the backfill loop heals divergence on next
    /// startup.
    async fn sync_team_memberships(&self, user_id: &str) {
        let Some(port) = &self.team_memberships_sync else {
            return;
        };
        let tenants = match self
            .membership_repo
            .find_active_team_tenants_for_user(user_id)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    user_id,
                    "find_active_team_tenants_for_user failed; skipping team_memberships sync"
                );
                return;
            }
        };
        if let Err(e) = port.set_team_memberships(user_id, &tenants).await {
            tracing::warn!(
                error = %e,
                user_id,
                tenant_count = tenants.len(),
                "team_memberships Keycloak stamp failed (MCP middleware may fail-closed until next sync)"
            );
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
        // ADR-111 §Tier Gating: the caller's personal effective tier must meet
        // the colony-ownership floor. This is the authoritative gate; the UI
        // hides ineligible options but the server is the source of truth.
        if let Some(svc) = &self.effective_tier_service {
            let personal = svc
                .compute_effective_tier(&cmd.owner_user_id)
                .await
                .map_err(|e| {
                    TeamServiceError::Repository(RepositoryError::Unknown(format!(
                        "effective_tier: {e}"
                    )))
                })?;
            if !personal.allows_colony() {
                return Err(TeamServiceError::PersonalTierTooLow {
                    personal,
                    requested: cmd.tier,
                });
            }
        }

        // Per ADR-111 (colony tier model): only Business and Enterprise may
        // own a colony. Free/Pro are personal-only; System is never a
        // user-facing tier.
        if !cmd.tier.allows_colony() {
            return Err(TeamServiceError::TierNotPermitted(cmd.tier));
        }

        let mut team = Team::provision(
            cmd.display_name.clone(),
            cmd.owner_user_id.clone(),
            cmd.tier,
        )
        .map_err(TeamServiceError::InvalidCommand)?;

        // Insert the backing tenants row (ADR-056). Business shares the
        // `zaru-consumer` realm via group-scoped membership; Enterprise gets
        // its own dedicated realm. The `allows_colony()` guard above
        // guarantees we never see Free/Pro/System here.
        let keycloak_realm = match team.tier {
            TenantTier::Business => "zaru-consumer".to_string(),
            TenantTier::Enterprise => format!("team-{}", team.slug.as_str()),
            TenantTier::Free | TenantTier::Pro | TenantTier::System => {
                unreachable!("allows_colony() guard rejects these tiers")
            }
        };
        let openbao_namespace = format!("tenant-{}", team.tenant_id.as_str());
        let tenant_row = Tenant::new(
            team.tenant_id.clone(),
            cmd.display_name.clone(),
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
            .provision_team_customer(team.id, cmd.owner_email, &team.tenant_id, team.tier)
            .await?;

        // Stamp the `team_memberships` JWT claim on the owner — they are now
        // an active member of this newly-provisioned tenant. Without this the
        // upstream MCP middleware fails closed with 403 on the owner's first
        // tenant-scoped tool call.
        self.sync_team_memberships(&cmd.owner_user_id).await;

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

        // Stamp the `team_memberships` JWT claim — the new tenant must be
        // present before the invitee makes their first tenant-scoped MCP
        // call, or the middleware fails closed with 403.
        self.sync_team_memberships(&cmd.authenticated_user_id).await;

        // ADR-111 Phase 3: lift the joining member's effective personal tier
        // to match this colony's tier. Failures here are logged but do not
        // abort the invitation flow — the membership is saved and seats are
        // resynced regardless; a subsequent recompute will heal the tier.
        if let Some(service) = &self.effective_tier_service {
            if let Err(e) = service.recompute_for_user(&cmd.authenticated_user_id).await {
                tracing::warn!(
                    error = %e,
                    user_id = %cmd.authenticated_user_id,
                    team_id = %invitation.team_id,
                    "effective tier recompute failed on accept_invitation"
                );
            }
        }

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

        // Drop the revoked tenant from the user's `team_memberships` JWT
        // claim immediately — leaving a stale entry would let the MCP
        // middleware authorize calls against a tenant the user no longer
        // belongs to.
        self.sync_team_memberships(&user_id).await;

        // ADR-111 Phase 3: drop the revoked member's effective tier back to
        // their personal subscription tier (or any remaining active colony).
        if let Some(service) = &self.effective_tier_service {
            if let Err(e) = service.recompute_for_user(&user_id).await {
                tracing::warn!(
                    error = %e,
                    user_id = %user_id,
                    team_id = %team_id,
                    "effective tier recompute failed on revoke_membership"
                );
            }
        }

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

    async fn list_pending_invitations(
        &self,
        team_id: TeamId,
    ) -> Result<Vec<TeamInvitation>, TeamServiceError> {
        Ok(self.invitation_repo.find_pending_by_team(&team_id).await?)
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::team::{MembershipStatus, TeamInvitationRepository, TeamSlug, TeamStatus};
    use crate::domain::tenancy::{Tenant, TenantStatus};
    use std::sync::Mutex;

    // ── In-memory fixtures ──────────────────────────────────────────────────

    #[derive(Default)]
    struct MemTeamRepo {
        teams: Mutex<Vec<Team>>,
    }

    #[async_trait]
    impl TeamRepository for MemTeamRepo {
        async fn save(&self, team: &Team) -> Result<(), RepositoryError> {
            let mut g = self.teams.lock().unwrap();
            if let Some(existing) = g.iter_mut().find(|t| t.id == team.id) {
                *existing = team.clone();
            } else {
                g.push(team.clone());
            }
            Ok(())
        }
        async fn find_by_id(&self, id: &TeamId) -> Result<Option<Team>, RepositoryError> {
            Ok(self
                .teams
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id == *id)
                .cloned())
        }
        async fn find_by_slug(&self, slug: &TeamSlug) -> Result<Option<Team>, RepositoryError> {
            Ok(self
                .teams
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.slug == *slug)
                .cloned())
        }
        async fn find_by_owner(&self, owner_user_id: &str) -> Result<Vec<Team>, RepositoryError> {
            Ok(self
                .teams
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.owner_user_id == owner_user_id)
                .cloned()
                .collect())
        }
        async fn find_by_tenant_id(
            &self,
            tenant_id: &crate::domain::tenant::TenantId,
        ) -> Result<Option<Team>, RepositoryError> {
            Ok(self
                .teams
                .lock()
                .unwrap()
                .iter()
                .find(|t| &t.tenant_id == tenant_id)
                .cloned())
        }
        async fn delete(&self, id: &TeamId) -> Result<(), RepositoryError> {
            self.teams.lock().unwrap().retain(|t| t.id != *id);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemMembershipRepo {
        items: Mutex<Vec<Membership>>,
    }

    #[async_trait]
    impl MembershipRepository for MemMembershipRepo {
        async fn save(&self, m: &Membership) -> Result<(), RepositoryError> {
            self.items.lock().unwrap().push(m.clone());
            Ok(())
        }
        async fn find_by_team(&self, team_id: &TeamId) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.team_id == *team_id)
                .cloned()
                .collect())
        }
        async fn find_by_user(&self, user_id: &str) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.user_id == user_id)
                .cloned()
                .collect())
        }
        async fn find_active_for_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.user_id == user_id && m.status == MembershipStatus::Active)
                .cloned()
                .collect())
        }
        async fn find_active_team_tenants_for_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<String>, RepositoryError> {
            // The in-memory fake derives the slug from the team_id; production
            // performs the equivalent JOIN against the `teams.tenant_id`
            // column. This stays consistent with `Team::provision`, which
            // sets `tenant_id = TeamSlug::new(team_id).to_tenant_id()`.
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.user_id == user_id && m.status == MembershipStatus::Active)
                .map(|m| TeamSlug::new(m.team_id).as_str().to_string())
                .collect())
        }
        async fn is_active_member(
            &self,
            user_id: &str,
            team_id: &TeamId,
        ) -> Result<bool, RepositoryError> {
            Ok(self.items.lock().unwrap().iter().any(|m| {
                m.user_id == user_id
                    && m.team_id == *team_id
                    && m.status == MembershipStatus::Active
            }))
        }
        async fn count_active(&self, team_id: &TeamId) -> Result<u32, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.team_id == *team_id && m.status == MembershipStatus::Active)
                .count() as u32)
        }
        async fn revoke(&self, team_id: &TeamId, user_id: &str) -> Result<(), RepositoryError> {
            for m in self.items.lock().unwrap().iter_mut() {
                if m.team_id == *team_id && m.user_id == user_id {
                    m.status = MembershipStatus::Revoked;
                }
            }
            Ok(())
        }
    }

    struct MemInvitationRepo;

    #[async_trait]
    impl TeamInvitationRepository for MemInvitationRepo {
        async fn save(&self, _: &TeamInvitation) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn find_by_id(
            &self,
            _: &TeamInvitationId,
        ) -> Result<Option<TeamInvitation>, RepositoryError> {
            Ok(None)
        }
        async fn find_by_token_hash(
            &self,
            _: &str,
        ) -> Result<Option<TeamInvitation>, RepositoryError> {
            Ok(None)
        }
        async fn find_pending_by_team(
            &self,
            _: &TeamId,
        ) -> Result<Vec<TeamInvitation>, RepositoryError> {
            Ok(Vec::new())
        }
        async fn mark_accepted(&self, _: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn mark_cancelled(&self, _: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn mark_expired(&self, _: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemTenantRepo {
        rows: Mutex<Vec<Tenant>>,
    }

    #[async_trait]
    impl crate::domain::repository::TenantRepository for MemTenantRepo {
        async fn find_by_slug(
            &self,
            slug: &crate::domain::tenant::TenantId,
        ) -> Result<Option<Tenant>, RepositoryError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|t| &t.slug == slug)
                .cloned())
        }
        async fn find_all_active(&self) -> Result<Vec<Tenant>, RepositoryError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.status == TenantStatus::Active)
                .cloned()
                .collect())
        }
        async fn insert(&self, tenant: &Tenant) -> Result<(), RepositoryError> {
            self.rows.lock().unwrap().push(tenant.clone());
            Ok(())
        }
        async fn update_status(
            &self,
            _slug: &crate::domain::tenant::TenantId,
            _status: &TenantStatus,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    struct NoopBilling;

    #[async_trait]
    impl BillingService for NoopBilling {
        async fn sync_seats(
            &self,
            _: TeamId,
            _: &crate::domain::tenant::TenantId,
            _: u32,
        ) -> Result<u32, BillingServiceError> {
            Ok(0)
        }
        async fn provision_team_customer(
            &self,
            _: TeamId,
            _: String,
            _: &crate::domain::tenant::TenantId,
            _: TenantTier,
        ) -> Result<String, BillingServiceError> {
            Ok("cus_test".to_string())
        }
        async fn cancel_team_subscription(
            &self,
            _: &crate::domain::tenant::TenantId,
        ) -> Result<(), BillingServiceError> {
            Ok(())
        }
    }

    fn build_service() -> StandardTeamService {
        StandardTeamService::new(
            Arc::new(MemTeamRepo::default()),
            Arc::new(MemMembershipRepo::default()),
            Arc::new(MemInvitationRepo),
            Arc::new(MemTenantRepo::default()),
            Arc::new(NoopBilling),
            Arc::new(EventBus::with_default_capacity()),
            Some(b"test-key-32-bytes-long-xxxxxxxxxx".to_vec()),
            None,
            None,
        )
    }

    fn cmd(tier: TenantTier) -> ProvisionTeamCommand {
        ProvisionTeamCommand {
            display_name: "Acme".into(),
            owner_user_id: "user-1".into(),
            owner_email: "owner@example.com".into(),
            tier,
        }
    }

    // ── provision_team tier gating (ADR-111) ────────────────────────────────

    #[tokio::test]
    async fn provision_team_rejects_free() {
        let svc = build_service();
        let err = svc.provision_team(cmd(TenantTier::Free)).await;
        assert!(matches!(
            err,
            Err(TeamServiceError::TierNotPermitted(TenantTier::Free))
        ));
    }

    #[tokio::test]
    async fn provision_team_rejects_pro() {
        // Per ADR-111: Pro is personal-only and cannot own a colony.
        let svc = build_service();
        let err = svc.provision_team(cmd(TenantTier::Pro)).await;
        assert!(matches!(
            err,
            Err(TeamServiceError::TierNotPermitted(TenantTier::Pro))
        ));
    }

    #[tokio::test]
    async fn provision_team_rejects_system() {
        let svc = build_service();
        let err = svc.provision_team(cmd(TenantTier::System)).await;
        assert!(matches!(
            err,
            Err(TeamServiceError::TierNotPermitted(TenantTier::System))
        ));
    }

    #[tokio::test]
    async fn provision_team_accepts_business_with_5_included_seats() {
        let svc = build_service();
        let team = svc.provision_team(cmd(TenantTier::Business)).await.unwrap();
        assert_eq!(team.tier, TenantTier::Business);
        assert_eq!(team.status, TeamStatus::Active);
        assert_eq!(team.tier.included_seats(), 5);
    }

    #[tokio::test]
    async fn provision_team_accepts_enterprise_with_10_included_seats() {
        let svc = build_service();
        let team = svc
            .provision_team(cmd(TenantTier::Enterprise))
            .await
            .unwrap();
        assert_eq!(team.tier, TenantTier::Enterprise);
        assert_eq!(team.status, TeamStatus::Active);
        assert_eq!(team.tier.included_seats(), 10);
    }

    // ── ADR-111 §Tier Gating: personal-tier gate on provision ──────────────

    use crate::application::effective_tier_service::{EffectiveTierError, EffectiveTierService};

    /// Stub `EffectiveTierService` returning a fixed personal tier.
    /// `recompute_*` are not exercised by `provision_team`.
    struct StubPersonalTier(TenantTier);

    #[async_trait]
    impl EffectiveTierService for StubPersonalTier {
        async fn recompute_for_user(
            &self,
            _user_id: &str,
        ) -> Result<TenantTier, EffectiveTierError> {
            Ok(self.0)
        }
        async fn recompute_for_team(&self, _team_id: &TeamId) -> Result<(), EffectiveTierError> {
            Ok(())
        }
        async fn compute_effective_tier(
            &self,
            _user_id: &str,
        ) -> Result<TenantTier, EffectiveTierError> {
            Ok(self.0)
        }
    }

    fn build_service_with_personal(personal: TenantTier) -> StandardTeamService {
        StandardTeamService::new(
            Arc::new(MemTeamRepo::default()),
            Arc::new(MemMembershipRepo::default()),
            Arc::new(MemInvitationRepo),
            Arc::new(MemTenantRepo::default()),
            Arc::new(NoopBilling),
            Arc::new(EventBus::with_default_capacity()),
            Some(b"test-key-32-bytes-long-xxxxxxxxxx".to_vec()),
            Some(Arc::new(StubPersonalTier(personal))),
            None,
        )
    }

    #[tokio::test]
    async fn provision_team_rejects_when_personal_tier_below_business() {
        let svc = build_service_with_personal(TenantTier::Free);
        let err = svc.provision_team(cmd(TenantTier::Business)).await;
        assert!(
            matches!(
                err,
                Err(TeamServiceError::PersonalTierTooLow {
                    personal: TenantTier::Free,
                    requested: TenantTier::Business,
                })
            ),
            "expected PersonalTierTooLow, got {err:?}"
        );
    }

    #[tokio::test]
    async fn provision_team_allows_business_personal() {
        let svc = build_service_with_personal(TenantTier::Business);
        let team = svc
            .provision_team(cmd(TenantTier::Business))
            .await
            .expect("business personal must allow business colony");
        assert_eq!(team.tier, TenantTier::Business);
    }

    // ── team_memberships JWT claim stamping (MCP middleware fail-closed gap) ─

    /// Recording fake of [`TeamMembershipsSyncPort`]. Captures every
    /// `set_team_memberships(user_id, tenants)` call in order so tests can
    /// assert both the sequence of stamps and the resulting tenant set per
    /// user.
    #[derive(Default)]
    struct RecordingMembershipsSync {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    #[async_trait]
    impl TeamMembershipsSyncPort for RecordingMembershipsSync {
        async fn set_team_memberships(
            &self,
            user_id: &str,
            tenants: &[String],
        ) -> Result<(), TeamServiceError> {
            self.calls
                .lock()
                .unwrap()
                .push((user_id.to_string(), tenants.to_vec()));
            Ok(())
        }
    }

    /// Stateful in-memory invitation repo so accept_invitation can round-trip
    /// a token through `find_by_token_hash` → `mark_accepted`.
    #[derive(Default)]
    struct StatefulInvitationRepo {
        items: Mutex<Vec<TeamInvitation>>,
    }

    #[async_trait]
    impl TeamInvitationRepository for StatefulInvitationRepo {
        async fn save(&self, invitation: &TeamInvitation) -> Result<(), RepositoryError> {
            let mut g = self.items.lock().unwrap();
            if let Some(slot) = g.iter_mut().find(|i| i.id == invitation.id) {
                *slot = invitation.clone();
            } else {
                g.push(invitation.clone());
            }
            Ok(())
        }
        async fn find_by_id(
            &self,
            id: &TeamInvitationId,
        ) -> Result<Option<TeamInvitation>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .find(|i| i.id == *id)
                .cloned())
        }
        async fn find_by_token_hash(
            &self,
            token_hash: &str,
        ) -> Result<Option<TeamInvitation>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .find(|i| i.token_hash == token_hash)
                .cloned())
        }
        async fn find_pending_by_team(
            &self,
            team_id: &TeamId,
        ) -> Result<Vec<TeamInvitation>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|i| i.team_id == *team_id && i.status == InvitationStatus::Pending)
                .cloned()
                .collect())
        }
        async fn mark_accepted(&self, id: &TeamInvitationId) -> Result<(), RepositoryError> {
            for i in self.items.lock().unwrap().iter_mut() {
                if i.id == *id {
                    i.status = InvitationStatus::Accepted;
                }
            }
            Ok(())
        }
        async fn mark_cancelled(&self, _id: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn mark_expired(&self, _id: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    fn build_service_with_memberships_sync(
        sync: Arc<RecordingMembershipsSync>,
    ) -> StandardTeamService {
        StandardTeamService::new(
            Arc::new(MemTeamRepo::default()),
            Arc::new(MemMembershipRepo::default()),
            Arc::new(StatefulInvitationRepo::default()),
            Arc::new(MemTenantRepo::default()),
            Arc::new(NoopBilling),
            Arc::new(EventBus::with_default_capacity()),
            Some(b"test-key-32-bytes-long-xxxxxxxxxx".to_vec()),
            None,
            Some(sync),
        )
    }

    /// Regression: provisioning a team must stamp `team_memberships` on the
    /// owner so the new tenant is present in their JWT before they make any
    /// tenant-scoped MCP call. Without this the upstream middleware fails
    /// closed with 403 on the very first request.
    #[tokio::test]
    async fn team_memberships_stamped_on_provision_team() {
        let sync = Arc::new(RecordingMembershipsSync::default());
        let svc = build_service_with_memberships_sync(sync.clone());

        let team = svc
            .provision_team(cmd(TenantTier::Business))
            .await
            .expect("provision must succeed");

        let calls = sync.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "exactly one stamp on provision (owner)");
        assert_eq!(calls[0].0, "user-1", "owner is the stamp target");
        assert_eq!(
            calls[0].1,
            vec![team.tenant_id.as_str().to_string()],
            "owner's claim must contain exactly the new team tenant slug",
        );
    }

    /// Regression: accept_invitation must stamp `team_memberships` on the
    /// invitee with the new tenant included.
    #[tokio::test]
    async fn team_memberships_stamped_on_accept_invitation() {
        let sync = Arc::new(RecordingMembershipsSync::default());
        let svc = build_service_with_memberships_sync(sync.clone());

        let team = svc
            .provision_team(cmd(TenantTier::Business))
            .await
            .expect("provision must succeed");

        // Owner sends invite, then invitee accepts.
        let issued = svc
            .invite_member(InviteMemberCommand {
                team_id: team.id,
                invitee_email: "newbie@example.com".to_string(),
                invited_by_user_id: "user-1".to_string(),
            })
            .await
            .expect("invite must succeed");

        sync.calls.lock().unwrap().clear();

        svc.accept_invitation(AcceptInvitationCommand {
            token: issued.raw_token.clone(),
            authenticated_email: "newbie@example.com".to_string(),
            authenticated_user_id: "user-2".to_string(),
        })
        .await
        .expect("accept must succeed");

        let calls = sync.calls.lock().unwrap().clone();
        let stamps_for_user2: Vec<&Vec<String>> = calls
            .iter()
            .filter(|(uid, _)| uid == "user-2")
            .map(|(_, t)| t)
            .collect();
        assert_eq!(
            stamps_for_user2.len(),
            1,
            "exactly one stamp on accept (invitee)"
        );
        assert!(
            stamps_for_user2[0].contains(&team.tenant_id.as_str().to_string()),
            "invitee's claim must include the new tenant; got {:?}",
            stamps_for_user2[0]
        );
    }

    /// Regression: revoke_membership must re-stamp the affected user's
    /// `team_memberships` with the revoked tenant removed. A stale entry
    /// would let the MCP middleware authorize calls against a tenant the
    /// user no longer belongs to.
    #[tokio::test]
    async fn team_memberships_stamped_on_revoke_membership() {
        let sync = Arc::new(RecordingMembershipsSync::default());
        let svc = build_service_with_memberships_sync(sync.clone());

        let team = svc
            .provision_team(cmd(TenantTier::Business))
            .await
            .expect("provision must succeed");

        let issued = svc
            .invite_member(InviteMemberCommand {
                team_id: team.id,
                invitee_email: "victim@example.com".to_string(),
                invited_by_user_id: "user-1".to_string(),
            })
            .await
            .expect("invite must succeed");

        svc.accept_invitation(AcceptInvitationCommand {
            token: issued.raw_token,
            authenticated_email: "victim@example.com".to_string(),
            authenticated_user_id: "user-2".to_string(),
        })
        .await
        .expect("accept must succeed");

        sync.calls.lock().unwrap().clear();

        svc.revoke_membership(team.id, "user-2".to_string(), "user-1".to_string())
            .await
            .expect("revoke must succeed");

        let calls = sync.calls.lock().unwrap().clone();
        let stamps_for_user2: Vec<&Vec<String>> = calls
            .iter()
            .filter(|(uid, _)| uid == "user-2")
            .map(|(_, t)| t)
            .collect();
        assert_eq!(stamps_for_user2.len(), 1, "exactly one stamp on revoke");
        assert!(
            !stamps_for_user2[0].contains(&team.tenant_id.as_str().to_string()),
            "revoked tenant must NOT appear in the re-stamped claim; got {:?}",
            stamps_for_user2[0]
        );
        assert!(
            stamps_for_user2[0].is_empty(),
            "victim had only this one membership; claim must be empty"
        );
    }

    /// Regression for the new `find_active_team_tenants_for_user` repo
    /// method: it must return the `t-{uuid}` slug for every active
    /// membership and skip revoked rows. This is the source list the
    /// `team_memberships` JWT claim is computed from.
    #[tokio::test]
    async fn find_active_team_tenants_for_user_returns_active_only() {
        use crate::domain::team::TeamSlug;

        let repo = MemMembershipRepo::default();
        let team_a = TeamId::new();
        let team_b = TeamId::new();
        let team_c = TeamId::new();

        // Two active, one revoked (the revoked team_c slug must NOT appear).
        let active_a = Membership::new_active(team_a, "user-x".into(), MembershipRole::Member);
        let active_b = Membership::new_active(team_b, "user-x".into(), MembershipRole::Owner);
        let mut revoked_c = Membership::new_active(team_c, "user-x".into(), MembershipRole::Member);
        revoked_c.status = MembershipStatus::Revoked;

        repo.save(&active_a).await.unwrap();
        repo.save(&active_b).await.unwrap();
        repo.save(&revoked_c).await.unwrap();
        // A membership for someone else — must not leak.
        repo.save(&Membership::new_active(
            team_a,
            "user-y".into(),
            MembershipRole::Member,
        ))
        .await
        .unwrap();

        let mut tenants = repo
            .find_active_team_tenants_for_user("user-x")
            .await
            .unwrap();
        tenants.sort();

        let mut expected = vec![
            TeamSlug::new(team_a).as_str().to_string(),
            TeamSlug::new(team_b).as_str().to_string(),
        ];
        expected.sort();

        assert_eq!(tenants, expected);
        assert!(
            !tenants.contains(&TeamSlug::new(team_c).as_str().to_string()),
            "revoked team must not be reported"
        );
    }

    #[tokio::test]
    async fn provision_team_allows_business_personal_for_enterprise_team() {
        let svc = build_service_with_personal(TenantTier::Business);
        let team = svc
            .provision_team(cmd(TenantTier::Enterprise))
            .await
            .expect("business personal must allow enterprise colony");
        assert_eq!(team.tier, TenantTier::Enterprise);
    }
}
