// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # User Volume Service (Gap 079-5)
//!
//! Manages the lifecycle of user-owned persistent volumes, enforcing per-tier
//! storage quotas and publishing user-scoped domain events.

use std::sync::Arc;

use chrono::Utc;

use crate::application::volume_manager::{CreateUserVolumeCommand, VolumeService};
use crate::domain::events::VolumeEvent;
use crate::domain::git_repo_tier_limits::GitRepoTierLimits;
use crate::domain::iam::ZaruTier;
use crate::domain::repository::{RepositoryError, VolumeRepository};
use crate::domain::volume::{
    QuotaUsage, StorageClass, StorageTierLimits, TenantId, Volume, VolumeId, VolumeOwnership,
    VolumeStatus,
};
use crate::infrastructure::event_bus::EventBus;

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum UserVolumeError {
    #[error("volume not found: {0}")]
    NotFound(VolumeId),
    #[error("unauthorized")]
    Unauthorized,
    #[error("volume count quota exceeded for tier")]
    VolumeCountQuotaExceeded,
    #[error("storage quota exceeded for tier")]
    StorageQuotaExceeded,
    #[error("volume with name '{0}' already exists")]
    DuplicateName(String),
    #[error("volume is currently attached and cannot be deleted")]
    VolumeAttached,
    #[error("unknown tier")]
    UnknownTier,
    #[error("repository error: {0}")]
    Repository(String),
    #[error("volume service error: {0}")]
    VolumeService(String),
}

impl From<RepositoryError> for UserVolumeError {
    fn from(e: RepositoryError) -> Self {
        UserVolumeError::Repository(e.to_string())
    }
}

// ============================================================================
// Read-model projection
// ============================================================================

/// Volume with actual SeaweedFS usage attached.
///
/// Read-model DTO returned by `UserVolumeService::list_volumes_with_usage`.
/// `used_bytes` is fetched on-demand from the storage backend (ADR-036) and
/// deliberately lives outside the `Volume` aggregate — usage is read-model
/// data, not a domain invariant. `None` when the volume's usage could not be
/// read (ADR-079's Update of 2026-10-15): a failed probe is reported, never
/// read as zero.
#[derive(Debug, Clone)]
pub struct VolumeWithUsage {
    pub volume: Volume,
    pub used_bytes: Option<u64>,
}

impl VolumeWithUsage {
    /// Whether this volume's usage could not be read.
    pub fn usage_incomplete(&self) -> bool {
        self.used_bytes.is_none()
    }
}

/// The quota usage as read now, with whether any volume's usage could not
/// be read (ADR-079's Update of 2026-10-15). When `usage_incomplete`,
/// `usage.total_bytes_used` is the sum of the volumes that could be read.
#[derive(Debug, Clone)]
pub struct QuotaReading {
    pub usage: QuotaUsage,
    pub usage_incomplete: bool,
}

// ============================================================================
// The chat-attachments volume
// ============================================================================

/// Reserved volume name used for chat attachments (ADR-113).
///
/// A person's `chat-attachments` volume is provisioned on first use (an
/// upload naming this literal in place of a volume id, or a mail attachment
/// saved by `mail.attachment`) and reused afterwards. The name is reserved
/// per ADR-079 and counts against the person's `ZaruTier` storage quota like
/// any other volume.
pub const CHAT_ATTACHMENTS_VOLUME_NAME: &str = "chat-attachments";

/// Hard ceiling for the lazy-provisioned `chat-attachments` volume on tiers
/// whose `total_storage_bytes` is unbounded (Enterprise = `u64::MAX`). A raw
/// `u64::MAX` allocation is nonsensical at the storage layer; clamp to a
/// sensible per-volume default that still leaves room for additional volumes
/// within the same tier budget.
pub const CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The name prefix of a repository binding's clone volume (`git-<label>`).
/// The quota check reads it to count a clone volume against the bindings'
/// allowance rather than the chat-attachments budget (AEGIS ADR-136
/// Update G2c).
pub const CLONE_VOLUME_NAME_PREFIX: &str = "git-";

/// The chat-attachments budget of a tier: its `total_storage_bytes` less the
/// bindings' allowance (the tier's clone volume size times its
/// `max_bindings`, [`GitRepoTierLimits::bindings_allowance_bytes`]), so a
/// person whose `chat-attachments` volume holds the whole budget can still
/// create every binding the tier allows (AEGIS ADR-136 Update G2c). It is
/// the size a lazy-provisioned `chat-attachments` volume is given, and the
/// size an older, larger one is shrunk to when its person creates a binding
/// ([`UserVolumeService::fit_chat_attachments_to_budget`]).
///
/// Unbounded tiers (Enterprise = `u64::MAX`) are clamped to
/// [`CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES`] because a raw `u64::MAX` is
/// nonsensical at the storage backend.
///
/// Returns `None` for an unrecognized tier so callers fail closed rather than
/// admitting the volume creation under an unbounded cap.
pub fn chat_attachments_volume_size_for_tier(
    limits: &StorageTierLimits,
    tier: &ZaruTier,
) -> Option<u64> {
    limits.limits.get(tier).map(|l| {
        if l.total_storage_bytes == u64::MAX {
            return CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES;
        }
        let allowance = GitRepoTierLimits::for_tier(tier.clone())
            .bindings_allowance_bytes(l.total_storage_bytes)
            .unwrap_or(0);
        l.total_storage_bytes.saturating_sub(allowance)
    })
}

/// The size of one repository binding's clone volume under `tier`
/// ([`GitRepoTierLimits::clone_volume_bytes`] of the tier's budget). `None`
/// for an unrecognized tier.
pub fn clone_volume_size_for_tier(limits: &StorageTierLimits, tier: &ZaruTier) -> Option<u64> {
    limits.limits.get(tier).map(|l| {
        GitRepoTierLimits::for_tier(tier.clone()).clone_volume_bytes(l.total_storage_bytes)
    })
}

// ============================================================================
// Service
// ============================================================================

pub struct UserVolumeService {
    pub volume_repo: Arc<dyn VolumeRepository>,
    pub volume_service: Arc<dyn VolumeService>,
    pub event_bus: Arc<EventBus>,
    pub tier_limits: StorageTierLimits,
}

impl UserVolumeService {
    pub fn new(
        volume_repo: Arc<dyn VolumeRepository>,
        volume_service: Arc<dyn VolumeService>,
        event_bus: Arc<EventBus>,
        tier_limits: StorageTierLimits,
    ) -> Self {
        Self {
            volume_repo,
            volume_service,
            event_bus,
            tier_limits,
        }
    }

    pub async fn create_volume(
        &self,
        cmd: CreateUserVolumeCommand,
    ) -> Result<Volume, UserVolumeError> {
        let limit = self
            .tier_limits
            .limits
            .get(&cmd.zaru_tier)
            .ok_or(UserVolumeError::UnknownTier)?
            .clone();

        // Quota: volume count
        let current_count = self
            .volume_repo
            .count_by_owner(&cmd.tenant_id, &cmd.owner_user_id)
            .await?;
        if current_count >= limit.max_volumes {
            return Err(UserVolumeError::VolumeCountQuotaExceeded);
        }

        // Quota: total storage. Pre-create check uses the *allocated* size sum
        // (size_limit_bytes) — actual bytes consumed isn't relevant here since
        // a freshly-created volume reserves its full allocation against the
        // tier ceiling regardless of how empty it is.
        let allocated_bytes = self
            .volume_repo
            .sum_allocated_size_by_owner(&cmd.tenant_id, &cmd.owner_user_id)
            .await?;
        if allocated_bytes.saturating_add(cmd.size_limit_bytes) > limit.total_storage_bytes {
            return Err(UserVolumeError::StorageQuotaExceeded);
        }

        // Quota: a clone volume counts against the bindings' allowance, never
        // against the chat-attachments budget (AEGIS ADR-136 Update G2c).
        if cmd.label.starts_with(CLONE_VOLUME_NAME_PREFIX) {
            if let Some(allowance) = GitRepoTierLimits::for_tier(cmd.zaru_tier.clone())
                .bindings_allowance_bytes(limit.total_storage_bytes)
            {
                let clone_bytes = self
                    .volume_repo
                    .find_by_owner(&cmd.tenant_id, &cmd.owner_user_id)
                    .await?
                    .iter()
                    .filter(|v| v.name.starts_with(CLONE_VOLUME_NAME_PREFIX))
                    .fold(0u64, |sum, v| sum.saturating_add(v.size_limit_bytes));
                if clone_bytes.saturating_add(cmd.size_limit_bytes) > allowance {
                    return Err(UserVolumeError::StorageQuotaExceeded);
                }
            }
        }

        // Uniqueness: name within owner namespace
        let existing = self
            .volume_repo
            .find_by_owner(&cmd.tenant_id, &cmd.owner_user_id)
            .await?;
        if existing.iter().any(|v| v.name == cmd.label) {
            return Err(UserVolumeError::DuplicateName(cmd.label.clone()));
        }

        // Delegate creation to VolumeService (size_limit_mb is bytes / 1024 / 1024)
        let size_limit_mb = cmd.size_limit_bytes.div_ceil(1024 * 1024);
        let volume_id = self
            .volume_service
            .create_volume(
                cmd.label.clone(),
                cmd.tenant_id.clone(),
                StorageClass::persistent(),
                size_limit_mb,
                VolumeOwnership::persistent(cmd.owner_user_id.clone()),
            )
            .await
            .map_err(|e| UserVolumeError::VolumeService(e.to_string()))?;

        let volume = self
            .volume_service
            .get_volume(volume_id)
            .await
            .map_err(|e| UserVolumeError::VolumeService(e.to_string()))?;

        self.event_bus
            .publish_volume_event(VolumeEvent::UserVolumeCreated {
                volume_id,
                owner_user_id: cmd.owner_user_id.clone(),
                tenant_id: cmd.tenant_id.clone(),
                label: cmd.label.clone(),
                size_limit_bytes: cmd.size_limit_bytes,
                created_at: Utc::now(),
            });

        Ok(volume)
    }

    /// The person's `chat-attachments` volume (ADR-113): found by name within
    /// (tenant, owner), else provisioned at the size
    /// [`chat_attachments_volume_size_for_tier`] gives `tier`. One rule for
    /// the upload route and the mail tools (AEGIS ADR-125's Update of
    /// 2026-10-08 (5) clause 31: "provisioned as uploads provision it").
    /// An unrecognized tier provisions nothing (`UnknownTier`).
    pub async fn find_or_provision_chat_attachments(
        &self,
        tenant_id: &TenantId,
        owner: &str,
        tier: &ZaruTier,
    ) -> Result<VolumeId, UserVolumeError> {
        if let Some(existing) = self
            .list_volumes(tenant_id, owner)
            .await?
            .into_iter()
            .find(|v| v.name == CHAT_ATTACHMENTS_VOLUME_NAME)
        {
            return Ok(existing.id);
        }
        let size_limit_bytes = chat_attachments_volume_size_for_tier(&self.tier_limits, tier)
            .ok_or(UserVolumeError::UnknownTier)?;
        self.create_volume(CreateUserVolumeCommand {
            tenant_id: tenant_id.clone(),
            owner_user_id: owner.to_string(),
            label: CHAT_ATTACHMENTS_VOLUME_NAME.to_string(),
            size_limit_bytes,
            zaru_tier: tier.clone(),
        })
        .await
        .map(|v| v.id)
    }

    /// Shrink the person's `chat-attachments` volume's stored size to their
    /// tier's chat-attachments budget when it is larger (a volume provisioned
    /// when it was given the tier's whole budget), so the bindings' allowance
    /// is free for a binding's clone volume (AEGIS ADR-136 Update G2c). The
    /// files stay; only `size_limit_bytes` changes. A volume at or under the
    /// budget, or no such volume, changes nothing. An unrecognized tier
    /// changes nothing and answers `UnknownTier`.
    pub async fn fit_chat_attachments_to_budget(
        &self,
        tenant_id: &TenantId,
        owner: &str,
        tier: &ZaruTier,
    ) -> Result<(), UserVolumeError> {
        let budget = chat_attachments_volume_size_for_tier(&self.tier_limits, tier)
            .ok_or(UserVolumeError::UnknownTier)?;
        let Some(mut volume) = self
            .volume_repo
            .find_by_owner(tenant_id, owner)
            .await?
            .into_iter()
            .find(|v| v.name == CHAT_ATTACHMENTS_VOLUME_NAME)
        else {
            return Ok(());
        };
        if volume.size_limit_bytes <= budget {
            return Ok(());
        }
        tracing::info!(
            volume_id = %volume.id,
            from_bytes = volume.size_limit_bytes,
            to_bytes = budget,
            "chat-attachments volume shrunk to its tier's chat budget"
        );
        volume.size_limit_bytes = budget;
        self.volume_repo.save(&volume).await?;
        Ok(())
    }

    pub async fn list_volumes(
        &self,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<Vec<Volume>, UserVolumeError> {
        // Repo layer already excludes `VolumeStatus::Deleted` (ADR-079).
        let volumes = self.volume_repo.find_by_owner(tenant_id, owner).await?;
        Ok(volumes)
    }

    /// List volumes with actual SeaweedFS usage attached to each entry.
    ///
    /// Read-model projection — usage is fetched via `VolumeService::get_volume_usage`
    /// (BC-7 storage abstraction, ADR-036) and is NOT stored on the `Volume`
    /// aggregate. If a per-volume usage query fails, that volume's
    /// `used_bytes` is `None` rather than failing the entire list — a
    /// usage-query outage must not blank the user's inventory, and must not
    /// read as zero either.
    pub async fn list_volumes_with_usage(
        &self,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<Vec<VolumeWithUsage>, UserVolumeError> {
        let volumes = self.volume_repo.find_by_owner(tenant_id, owner).await?;
        let mut out = Vec::with_capacity(volumes.len());
        for v in volumes {
            let used_bytes = match self.volume_service.get_volume_usage(v.id).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    tracing::warn!(volume_id = %v.id, error = %e, "volume usage could not be read");
                    None
                }
            };
            out.push(VolumeWithUsage {
                volume: v,
                used_bytes,
            });
        }
        Ok(out)
    }

    pub async fn rename_volume(
        &self,
        id: &VolumeId,
        owner: &str,
        new_name: &str,
    ) -> Result<(), UserVolumeError> {
        let mut volume = self
            .volume_repo
            .find_by_id(*id)
            .await?
            .ok_or(UserVolumeError::NotFound(*id))?;

        // Ownership check
        match &volume.ownership {
            VolumeOwnership::Persistent { owner: vol_owner } if vol_owner == owner => {}
            _ => return Err(UserVolumeError::Unauthorized),
        }

        // Name uniqueness within owner namespace
        let existing = self
            .volume_repo
            .find_by_owner(&volume.tenant_id, owner)
            .await?;
        if existing.iter().any(|v| v.name == new_name && v.id != *id) {
            return Err(UserVolumeError::DuplicateName(new_name.to_string()));
        }

        let old_label = volume.name.clone();
        volume.name = new_name.to_string();
        self.volume_repo.save(&volume).await?;

        self.event_bus
            .publish_volume_event(VolumeEvent::UserVolumeRenamed {
                volume_id: *id,
                old_label,
                new_label: new_name.to_string(),
                renamed_at: Utc::now(),
            });

        Ok(())
    }

    pub async fn delete_volume(&self, id: &VolumeId, owner: &str) -> Result<(), UserVolumeError> {
        let volume = self
            .volume_repo
            .find_by_id(*id)
            .await?
            .ok_or(UserVolumeError::NotFound(*id))?;

        // Ownership check
        match &volume.ownership {
            VolumeOwnership::Persistent { owner: vol_owner } if vol_owner == owner => {}
            _ => return Err(UserVolumeError::Unauthorized),
        }

        // Guard: cannot delete if attached
        if volume.status == VolumeStatus::Attached {
            return Err(UserVolumeError::VolumeAttached);
        }

        let owner_id = owner.to_string();

        self.volume_service
            .delete_volume(*id)
            .await
            .map_err(|e| UserVolumeError::VolumeService(e.to_string()))?;

        self.event_bus
            .publish_volume_event(VolumeEvent::UserVolumeDeleted {
                volume_id: *id,
                owner_user_id: owner_id,
                deleted_at: Utc::now(),
            });

        Ok(())
    }

    /// The quota usage, the sum over the volumes whose usage could be read.
    /// [`Self::get_quota_reading`] also says whether any could not.
    pub async fn get_quota_usage(
        &self,
        tenant_id: &TenantId,
        owner: &str,
        tier: &ZaruTier,
    ) -> Result<QuotaUsage, UserVolumeError> {
        self.get_quota_reading(tenant_id, owner, tier)
            .await
            .map(|reading| reading.usage)
    }

    /// The quota usage and whether any volume's usage could not be read: a
    /// volume whose probe fails adds nothing to `total_bytes_used` and marks
    /// the reading incomplete, never zero bytes read as exact.
    pub async fn get_quota_reading(
        &self,
        tenant_id: &TenantId,
        owner: &str,
        tier: &ZaruTier,
    ) -> Result<QuotaReading, UserVolumeError> {
        let limit = self
            .tier_limits
            .limits
            .get(tier)
            .ok_or(UserVolumeError::UnknownTier)?
            .clone();

        let volume_count = self.volume_repo.count_by_owner(tenant_id, owner).await?;

        // ADR-079: `total_bytes_used` is *actual* SeaweedFS consumption, not
        // the sum of allocated `size_limit_bytes`. We aggregate per-volume
        // usage via the storage abstraction (BC-7); a single volume's usage
        // probe failing marks the reading incomplete and adds nothing, so the
        // quota endpoint remains responsive without reading the failure as 0.
        let volumes = self.volume_repo.find_by_owner(tenant_id, owner).await?;
        let mut total_bytes_used: u64 = 0;
        let mut usage_incomplete = false;
        for v in &volumes {
            match self.volume_service.get_volume_usage(v.id).await {
                Ok(used) => total_bytes_used = total_bytes_used.saturating_add(used),
                Err(e) => {
                    tracing::warn!(volume_id = %v.id, error = %e, "volume usage could not be read");
                    usage_incomplete = true;
                }
            }
        }

        let usage = QuotaUsage {
            volume_count,
            total_bytes_used,
            tier_limit: limit.clone(),
        };

        // Publish warning if usage >= 80%
        if limit.total_storage_bytes > 0 {
            let usage_percent =
                (total_bytes_used as f64 / limit.total_storage_bytes as f64 * 100.0) as f32;
            if usage_percent >= 80.0 {
                self.event_bus
                    .publish_volume_event(VolumeEvent::UserVolumeQuotaWarning {
                        owner_user_id: owner.to_string(),
                        tenant_id: tenant_id.clone(),
                        usage_percent,
                        warned_at: Utc::now(),
                    });
            }
        }

        Ok(QuotaReading {
            usage,
            usage_incomplete,
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::volume_manager::VolumeService;
    use crate::domain::runtime::InstanceId;
    use crate::domain::storage::StorageError;
    use crate::domain::volume::{AccessMode, VolumeMount};
    use crate::infrastructure::repositories::InMemoryVolumeRepository;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct MockVolumeService {
        repo: Arc<InMemoryVolumeRepository>,
        event_bus: Arc<EventBus>,
        /// Optional per-volume actual usage override (read-model regression coverage).
        usage_overrides: Mutex<HashMap<VolumeId, u64>>,
        /// Volumes whose usage probe fails, as a filer error would.
        usage_failures: Mutex<std::collections::HashSet<VolumeId>>,
    }

    impl MockVolumeService {
        fn set_usage(&self, volume_id: VolumeId, used_bytes: u64) {
            self.usage_overrides
                .lock()
                .unwrap()
                .insert(volume_id, used_bytes);
        }

        fn fail_usage(&self, volume_id: VolumeId) {
            self.usage_failures.lock().unwrap().insert(volume_id);
        }
    }

    #[async_trait::async_trait]
    impl VolumeService for MockVolumeService {
        async fn create_volume(
            &self,
            name: String,
            tenant_id: TenantId,
            storage_class: StorageClass,
            size_limit_mb: u64,
            ownership: VolumeOwnership,
        ) -> anyhow::Result<VolumeId> {
            use crate::domain::volume::{FilerEndpoint, VolumeBackend};
            let filer = FilerEndpoint::new("http://localhost:8888").unwrap();
            let vid = VolumeId::new();
            let mut vol = Volume {
                id: vid,
                name,
                tenant_id,
                storage_class,
                backend: VolumeBackend::SeaweedFS {
                    filer_endpoint: filer,
                    remote_path: format!("/aegis/volumes/{}", vid),
                },
                size_limit_bytes: size_limit_mb * 1024 * 1024,
                status: VolumeStatus::Available,
                ownership,
                created_at: Utc::now(),
                attached_at: None,
                detached_at: None,
                expires_at: None,
                host_node_id: None,
            };
            vol.status = VolumeStatus::Available;
            self.repo.save(&vol).await?;
            Ok(vid)
        }

        async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
            self.repo
                .find_by_id(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }

        async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_tenant(tenant_id).await?)
        }

        async fn list_volumes_by_ownership(
            &self,
            ownership: &VolumeOwnership,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_ownership(ownership).await?)
        }

        async fn attach_volume(
            &self,
            _volume_id: VolumeId,
            _instance_id: InstanceId,
            _mount_point: PathBuf,
            _access_mode: AccessMode,
        ) -> anyhow::Result<VolumeMount> {
            unimplemented!()
        }

        async fn detach_volume(
            &self,
            _volume_id: VolumeId,
            _instance_id: InstanceId,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn delete_volume(&self, volume_id: VolumeId) -> anyhow::Result<()> {
            self.repo.delete(volume_id).await?;
            self.event_bus
                .publish_volume_event(VolumeEvent::VolumeDeleted {
                    volume_id,
                    deleted_at: Utc::now(),
                });
            Ok(())
        }

        async fn get_volume_usage(&self, volume_id: VolumeId) -> anyhow::Result<u64> {
            if self.usage_failures.lock().unwrap().contains(&volume_id) {
                return Err(StorageError::UsageUnreadable("filer answered HTTP 500".into()).into());
            }
            Ok(self
                .usage_overrides
                .lock()
                .unwrap()
                .get(&volume_id)
                .copied()
                .unwrap_or(0))
        }

        async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn create_volumes_for_execution(
            &self,
            _execution_id: crate::domain::execution::ExecutionId,
            _tenant_id: TenantId,
            _volume_specs: &[crate::domain::agent::VolumeSpec],
            _storage_mode: &str,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(vec![])
        }

        async fn persist_external_volume(
            &self,
            _volume_id: VolumeId,
            _name: String,
            _tenant_id: TenantId,
            _remote_path: String,
            _size_limit_bytes: u64,
            _ownership: VolumeOwnership,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn make_svc() -> (UserVolumeService, Arc<InMemoryVolumeRepository>) {
        let (svc, repo, _mock) = make_svc_with_mock();
        (svc, repo)
    }

    fn make_svc_with_mock() -> (
        UserVolumeService,
        Arc<InMemoryVolumeRepository>,
        Arc<MockVolumeService>,
    ) {
        let repo = Arc::new(InMemoryVolumeRepository::new());
        let event_bus = Arc::new(EventBus::new(16));
        let mock_svc = Arc::new(MockVolumeService {
            repo: repo.clone(),
            event_bus: event_bus.clone(),
            usage_overrides: Mutex::new(HashMap::new()),
            usage_failures: Mutex::new(std::collections::HashSet::new()),
        });
        let svc = UserVolumeService::new(
            repo.clone() as Arc<dyn VolumeRepository>,
            mock_svc.clone() as Arc<dyn VolumeService>,
            event_bus,
            StorageTierLimits::default(),
        );
        (svc, repo, mock_svc)
    }

    /// The `chat-attachments` volume is provisioned once, at the size its
    /// tier gives, owned by the person, and found by name afterwards; another
    /// person gets their own.
    #[tokio::test]
    async fn chat_attachments_is_provisioned_once_at_the_tiers_size_and_reused() {
        let (svc, repo) = make_svc();
        let tenant = TenantId::consumer();
        let first = svc
            .find_or_provision_chat_attachments(&tenant, "user-1", &ZaruTier::Free)
            .await
            .expect("provisioned");
        let again = svc
            .find_or_provision_chat_attachments(&tenant, "user-1", &ZaruTier::Free)
            .await;
        assert!(
            matches!(&again, Ok(id) if *id == first),
            "the second call did not answer the person's chat-attachments volume {first}: {again:?}"
        );
        let owned = repo.find_by_owner(&tenant, "user-1").await.unwrap();
        assert_eq!(owned.len(), 1, "{owned:?}");
        assert_eq!(owned[0].name, CHAT_ATTACHMENTS_VOLUME_NAME);
        assert_eq!(
            Some(owned[0].size_limit_bytes),
            chat_attachments_volume_size_for_tier(&StorageTierLimits::default(), &ZaruTier::Free)
        );
        let other = svc
            .find_or_provision_chat_attachments(&tenant, "user-2", &ZaruTier::Free)
            .await
            .expect("another person's");
        assert_ne!(
            other, first,
            "another person was given the first person's volume"
        );
    }

    #[tokio::test]
    async fn free_tier_volume_count_limit() {
        let (svc, _repo) = make_svc();
        let tenant = TenantId::consumer();

        // Create 2 volumes (Free tier max)
        for i in 0..2 {
            let res = svc
                .create_volume(CreateUserVolumeCommand {
                    tenant_id: tenant.clone(),
                    owner_user_id: "user-1".to_string(),
                    label: format!("vol-{}", i),
                    size_limit_bytes: 1024 * 1024, // 1 MB each
                    zaru_tier: ZaruTier::Free,
                })
                .await;
            assert!(res.is_ok(), "Volume {i} creation failed: {:?}", res);
        }

        // Third volume must fail
        let res = svc
            .create_volume(CreateUserVolumeCommand {
                tenant_id: tenant.clone(),
                owner_user_id: "user-1".to_string(),
                label: "vol-2".to_string(),
                size_limit_bytes: 1024 * 1024,
                zaru_tier: ZaruTier::Free,
            })
            .await;
        assert!(
            matches!(res, Err(UserVolumeError::VolumeCountQuotaExceeded)),
            "Expected VolumeCountQuotaExceeded, got: {:?}",
            res
        );
    }

    #[tokio::test]
    async fn storage_quota_enforcement() {
        let (svc, _repo) = make_svc();
        let tenant = TenantId::consumer();
        // Free tier limit: 500 MB total
        let free_limit_bytes = 500 * 1024 * 1024u64;

        // Create first volume that uses all of the quota
        let res = svc
            .create_volume(CreateUserVolumeCommand {
                tenant_id: tenant.clone(),
                owner_user_id: "user-2".to_string(),
                label: "big-vol".to_string(),
                size_limit_bytes: free_limit_bytes,
                zaru_tier: ZaruTier::Free,
            })
            .await;
        assert!(res.is_ok(), "First volume should succeed: {:?}", res);

        // Second volume should fail because it would exceed 500 MB
        let res = svc
            .create_volume(CreateUserVolumeCommand {
                tenant_id: tenant.clone(),
                owner_user_id: "user-2".to_string(),
                label: "extra-vol".to_string(),
                size_limit_bytes: 1,
                zaru_tier: ZaruTier::Free,
            })
            .await;
        assert!(
            matches!(res, Err(UserVolumeError::StorageQuotaExceeded)),
            "Expected StorageQuotaExceeded, got: {:?}",
            res
        );
    }

    #[tokio::test]
    async fn duplicate_name_rejection() {
        let (svc, _repo) = make_svc();
        let tenant = TenantId::consumer();

        svc.create_volume(CreateUserVolumeCommand {
            tenant_id: tenant.clone(),
            owner_user_id: "user-3".to_string(),
            label: "my-vol".to_string(),
            size_limit_bytes: 1024 * 1024,
            zaru_tier: ZaruTier::Free,
        })
        .await
        .unwrap();

        let res = svc
            .create_volume(CreateUserVolumeCommand {
                tenant_id: tenant.clone(),
                owner_user_id: "user-3".to_string(),
                label: "my-vol".to_string(),
                size_limit_bytes: 1024 * 1024,
                zaru_tier: ZaruTier::Free,
            })
            .await;
        assert!(
            matches!(res, Err(UserVolumeError::DuplicateName(_))),
            "Expected DuplicateName, got: {:?}",
            res
        );
    }

    #[tokio::test]
    async fn delete_guard_when_attached() {
        let (svc, repo) = make_svc();
        let tenant = TenantId::consumer();

        let vol = svc
            .create_volume(CreateUserVolumeCommand {
                tenant_id: tenant.clone(),
                owner_user_id: "user-4".to_string(),
                label: "attach-vol".to_string(),
                size_limit_bytes: 1024 * 1024,
                zaru_tier: ZaruTier::Free,
            })
            .await
            .unwrap();

        // Force the volume into Attached state
        let mut attached = vol.clone();
        attached.status = VolumeStatus::Attached;
        repo.save(&attached).await.unwrap();

        let res = svc.delete_volume(&vol.id, "user-4").await;
        assert!(
            matches!(res, Err(UserVolumeError::VolumeAttached)),
            "Expected VolumeAttached, got: {:?}",
            res
        );
    }

    /// Regression (ADR-079): `list_volumes` MUST exclude `VolumeStatus::Deleted`.
    /// `Deleted` is a transient pipeline state during hard-delete and must
    /// never appear in user-facing inventory queries.
    #[tokio::test]
    async fn list_volumes_excludes_deleted_status() {
        let (svc, repo) = make_svc();
        let tenant = TenantId::consumer();
        let owner = "user-list-excl".to_string();

        // Seed three volumes directly via the repo: Available, Detached, Deleted.
        let mk = |status: VolumeStatus, name: &str| Volume {
            id: VolumeId::new(),
            name: name.to_string(),
            tenant_id: tenant.clone(),
            storage_class: StorageClass::persistent(),
            backend: crate::domain::volume::VolumeBackend::SeaweedFS {
                filer_endpoint: crate::domain::volume::FilerEndpoint::new("http://localhost:8888")
                    .unwrap(),
                remote_path: format!("/aegis/volumes/{name}"),
            },
            size_limit_bytes: 1024 * 1024,
            status,
            ownership: VolumeOwnership::persistent(owner.clone()),
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };

        let avail = mk(VolumeStatus::Available, "vol-available");
        let detached = mk(VolumeStatus::Detached, "vol-detached");
        let deleted = mk(VolumeStatus::Deleted, "vol-deleted");
        repo.save(&avail).await.unwrap();
        repo.save(&detached).await.unwrap();
        repo.save(&deleted).await.unwrap();

        let listed = svc.list_volumes(&tenant, &owner).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|v| v.name.as_str()).collect();

        assert_eq!(
            listed.len(),
            2,
            "expected 2 non-deleted volumes, got {names:?}"
        );
        assert!(
            names.contains(&"vol-available"),
            "Available volume missing: {names:?}"
        );
        assert!(
            names.contains(&"vol-detached"),
            "Detached volume missing: {names:?}"
        );
        assert!(
            !names.contains(&"vol-deleted"),
            "Deleted volume must not appear: {names:?}"
        );
    }

    /// Regression (ADR-079): `total_bytes_used` in the quota response MUST
    /// reflect actual SeaweedFS consumption, not the sum of allocated
    /// `size_limit_bytes`. Two volumes whose allocations sum past the tier
    /// ceiling but whose actual usage is small must report the small actual
    /// number.
    #[tokio::test]
    async fn quota_total_bytes_used_reflects_actual_usage_not_allocation() {
        let (svc, repo, mock) = make_svc_with_mock();
        let tenant = TenantId::consumer();
        let owner = "user-quota-actual".to_string();

        // Seed two Available volumes whose allocations sum > Pro tier
        // ceiling (10 GB) — 6 GB + 5 GB = 11 GB. Bypassing create_volume's
        // pre-allocation check is the point: this exercises the read-model
        // path, not the create-time check.
        let mk = |name: &str, size: u64| Volume {
            id: VolumeId::new(),
            name: name.to_string(),
            tenant_id: tenant.clone(),
            storage_class: StorageClass::persistent(),
            backend: crate::domain::volume::VolumeBackend::SeaweedFS {
                filer_endpoint: crate::domain::volume::FilerEndpoint::new("http://localhost:8888")
                    .unwrap(),
                remote_path: format!("/aegis/volumes/{name}"),
            },
            size_limit_bytes: size,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::persistent(owner.clone()),
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };

        let v1 = mk("big-1", 6 * 1024 * 1024 * 1024); // 6 GB allocated
        let v2 = mk("big-2", 5 * 1024 * 1024 * 1024); // 5 GB allocated
        repo.save(&v1).await.unwrap();
        repo.save(&v2).await.unwrap();

        // Inject realistic actual usage well under the tier ceiling.
        let actual_v1: u64 = 3 * 1024 * 1024; // 3 MiB
        let actual_v2: u64 = 9 * 1024 * 1024; // 9 MiB
        mock.set_usage(v1.id, actual_v1);
        mock.set_usage(v2.id, actual_v2);

        let usage = svc
            .get_quota_usage(&tenant, &owner, &ZaruTier::Pro)
            .await
            .unwrap();

        let allocation_sum = v1.size_limit_bytes + v2.size_limit_bytes;
        let actual_sum = actual_v1 + actual_v2;

        assert_eq!(
            usage.total_bytes_used, actual_sum,
            "total_bytes_used must equal actual SeaweedFS usage ({actual_sum}), \
             got {} (allocation_sum was {allocation_sum})",
            usage.total_bytes_used
        );
        assert_ne!(
            usage.total_bytes_used, allocation_sum,
            "total_bytes_used must NOT equal allocation sum (regression of pre-fix bug)"
        );
        assert!(
            usage.total_bytes_used < usage.tier_limit.total_storage_bytes,
            "actual usage must be under the tier ceiling for this scenario"
        );
    }

    /// Two Available volumes for `owner`, the first with 2,525,169 bytes
    /// read, the second's usage probe failing.
    async fn one_volume_read_one_failing(
        repo: &InMemoryVolumeRepository,
        mock: &MockVolumeService,
        tenant: &TenantId,
        owner: &str,
    ) -> (Volume, Volume) {
        let mk = |name: &str| Volume {
            id: VolumeId::new(),
            name: name.to_string(),
            tenant_id: tenant.clone(),
            storage_class: StorageClass::persistent(),
            backend: crate::domain::volume::VolumeBackend::SeaweedFS {
                filer_endpoint: crate::domain::volume::FilerEndpoint::new("http://localhost:8888")
                    .unwrap(),
                remote_path: format!("/aegis/volumes/{name}"),
            },
            size_limit_bytes: 250 * 1024 * 1024,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::persistent(owner.to_string()),
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };
        let read = mk("chat-attachments");
        let failing = mk("git-aegis-examples");
        repo.save(&read).await.unwrap();
        repo.save(&failing).await.unwrap();
        mock.set_usage(read.id, 2_525_169);
        mock.fail_usage(failing.id);
        (read, failing)
    }

    /// Regression (volume-usage-reads-zero): a volume whose usage probe
    /// failed was listed with `used_bytes` 0. It is listed with no usage and
    /// marked incomplete; the volume read beside it keeps its bytes.
    #[tokio::test]
    async fn list_marks_a_volume_whose_usage_could_not_be_read() {
        let (svc, repo, mock) = make_svc_with_mock();
        let tenant = TenantId::consumer();
        let owner = "user-usage-unreadable";
        let (read, failing) = one_volume_read_one_failing(&repo, &mock, &tenant, owner).await;

        let listed = svc.list_volumes_with_usage(&tenant, owner).await.unwrap();

        let of = |id: VolumeId| listed.iter().find(|v| v.volume.id == id).unwrap();
        assert_eq!(of(read.id).used_bytes, Some(2_525_169));
        assert!(!of(read.id).usage_incomplete());
        assert_eq!(
            of(failing.id).used_bytes,
            None,
            "a failed probe must not read as 0"
        );
        assert!(of(failing.id).usage_incomplete());
    }

    /// Regression (volume-usage-reads-zero): the quota summed a failed probe
    /// as 0. It sums the volumes it could read and marks the reading
    /// incomplete.
    #[tokio::test]
    async fn quota_reading_is_incomplete_when_a_volume_could_not_be_read() {
        let (svc, repo, mock) = make_svc_with_mock();
        let tenant = TenantId::consumer();
        let owner = "user-quota-unreadable";
        one_volume_read_one_failing(&repo, &mock, &tenant, owner).await;

        let reading = svc
            .get_quota_reading(&tenant, owner, &ZaruTier::Free)
            .await
            .unwrap();

        assert!(reading.usage_incomplete);
        assert_eq!(reading.usage.total_bytes_used, 2_525_169);
        assert_eq!(reading.usage.volume_count, 2);
    }
}
