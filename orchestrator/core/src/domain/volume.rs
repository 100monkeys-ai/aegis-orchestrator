// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use uuid::Uuid;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use crate::domain::execution::ExecutionId;

// ============================================================================
// Value Objects
// ============================================================================

/// Unique identifier for a volume
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub Uuid);

impl VolumeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for VolumeId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for VolumeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a tenant
/// 
/// Enables multi-tenant namespace isolation.
/// Default tenant UUID for MVP: 00000000-0000-0000-0000-000000000001
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(pub Uuid);

impl TenantId {
    /// Create a new random tenant ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse tenant ID from string
    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }

    /// Get the default tenant ID for single-tenant MVP deployments
    pub fn default_tenant() -> Self {
        Self(Uuid::parse_str("00000000-0000-0000-0000-000000000001")
            .expect("Default tenant UUID is valid"))
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::default_tenant()
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Storage classification for volume lifecycle management
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageClass {
    /// Ephemeral storage with automatic TTL-based cleanup
    Ephemeral {
        /// Time-to-live duration for auto-cleanup
        #[serde(with = "duration_serialization")]
        ttl: Duration,
    },
    /// Persistent storage with no expiration
    Persistent,
}

impl StorageClass {
    /// Create ephemeral storage class with specified TTL
    pub fn ephemeral(ttl: Duration) -> Self {
        Self::Ephemeral { ttl }
    }

    /// Create ephemeral storage class with TTL in hours
    pub fn ephemeral_hours(hours: i64) -> Self {
        Self::Ephemeral {
            ttl: Duration::hours(hours),
        }
    }

    /// Create persistent storage class
    pub fn persistent() -> Self {
        Self::Persistent
    }

    /// Check if storage class is ephemeral
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, Self::Ephemeral { .. })
    }

    /// Get TTL if ephemeral, None otherwise
    pub fn ttl(&self) -> Option<Duration> {
        match self {
            Self::Ephemeral { ttl } => Some(*ttl),
            Self::Persistent => None,
        }
    }

    /// Calculate expiration timestamp from creation time
    pub fn calculate_expiry(&self, created_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Ephemeral { ttl } => Some(created_at + *ttl),
            Self::Persistent => None,
        }
    }
}

impl Default for StorageClass {
    fn default() -> Self {
        Self::ephemeral_hours(24) // Default: 24 hour TTL
    }
}

/// Duration serialization module for chrono::Duration
mod duration_serialization {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize as ISO 8601 duration string (e.g., "PT24H")
        let hours = duration.num_hours();
        let remaining_minutes = duration.num_minutes() % 60;
        let duration_str = if remaining_minutes == 0 {
            format!("PT{}H", hours)
        } else {
            format!("PT{}H{}M", hours, remaining_minutes)
        };
        serializer.serialize_str(&duration_str)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_iso8601_duration(&s).map_err(serde::de::Error::custom)
    }

    fn parse_iso8601_duration(s: &str) -> Result<Duration, String> {
        // Simple ISO 8601 duration parser (supports PTnHnM format)
        if !s.starts_with("PT") {
            return Err(format!("Invalid ISO 8601 duration: {}", s));
        }

        let rest = &s[2..];
        let mut hours = 0i64;
        let mut minutes = 0i64;

        let parts: Vec<&str> = rest.split('H').collect();
        if parts.len() == 2 {
            hours = parts[0].parse().map_err(|_| format!("Invalid hours: {}", parts[0]))?;
            if !parts[1].is_empty() {
                let min_part = parts[1].trim_end_matches('M');
                if !min_part.is_empty() {
                    minutes = min_part.parse().map_err(|_| format!("Invalid minutes: {}", min_part))?;
                }
            }
        } else if rest.ends_with('H') {
            hours = rest.trim_end_matches('H').parse()
                .map_err(|_| format!("Invalid hours: {}", rest))?;
        } else if rest.ends_with('M') {
            minutes = rest.trim_end_matches('M').parse()
                .map_err(|_| format!("Invalid minutes: {}", rest))?;
        } else {
            return Err(format!("Invalid duration format: {}", s));
        }

        Ok(Duration::hours(hours) + Duration::minutes(minutes))
    }
}

/// Volume access mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessMode {
    /// Read-only access
    ReadOnly,
    /// Read-write access
    ReadWrite,
}

impl AccessMode {
    pub fn is_writable(&self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

impl Default for AccessMode {
    fn default() -> Self {
        Self::ReadWrite
    }
}

/// SeaweedFS filer endpoint
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilerEndpoint {
    /// Filer URL (e.g., "http://localhost:8888")
    pub url: String,
}

impl FilerEndpoint {
    /// Create new filer endpoint with validation
    pub fn new(url: impl Into<String>) -> Result<Self, VolumeError> {
        let url = url.into();
        
        // Basic URL validation
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(VolumeError::InvalidFilerEndpoint(
                "Filer URL must start with http:// or https://".to_string()
            ));
        }

        Ok(Self { url })
    }

    /// Get the filer host (hostname:port)
    pub fn host(&self) -> Result<String, VolumeError> {
        let parsed = reqwest::Url::parse(&self.url)
            .map_err(|e| VolumeError::InvalidFilerEndpoint(e.to_string()))?;
        
        let host = parsed.host_str()
            .ok_or_else(|| VolumeError::InvalidFilerEndpoint("No host in URL".to_string()))?;
        
        let port = parsed.port().unwrap_or(8888);
        
        Ok(format!("{}:{}", host, port))
    }
}

/// Volume mount specification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMount {
    /// Volume ID to mount
    pub volume_id: VolumeId,
    
    /// Mount point path inside container/VM
    pub mount_point: PathBuf,
    
    /// Access mode (read-only or read-write)
    pub access_mode: AccessMode,
    
    /// Filer endpoint for NFS mounting
    pub filer_endpoint: FilerEndpoint,
    
    /// Remote path on SeaweedFS filer
    pub remote_path: String,
}

impl VolumeMount {
    /// Create a new volume mount
    pub fn new(
        volume_id: VolumeId,
        mount_point: PathBuf,
        access_mode: AccessMode,
        filer_endpoint: FilerEndpoint,
        remote_path: String,
    ) -> Self {
        Self {
            volume_id,
            mount_point,
            access_mode,
            filer_endpoint,
            remote_path,
        }
    }
}

/// Volume ownership model
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum VolumeOwnership {
    /// Owned by a single agent execution (auto-cleanup on execution end)
    Execution {
        execution_id: ExecutionId,
    },
    /// Shared across workflow execution (auto-cleanup on workflow end)
    WorkflowExecution {
        workflow_execution_id: Uuid,
    },
    /// User-owned persistent volume (manual cleanup only)
    Persistent {
        owner: String,
    },
}

impl VolumeOwnership {
    /// Create execution-scoped ownership
    pub fn execution(execution_id: ExecutionId) -> Self {
        Self::Execution { execution_id }
    }

    /// Create workflow-scoped ownership
    pub fn workflow(workflow_execution_id: Uuid) -> Self {
        Self::WorkflowExecution { workflow_execution_id }
    }

    /// Create persistent user-owned volume
    pub fn persistent(owner: impl Into<String>) -> Self {
        Self::Persistent { owner: owner.into() }
    }

    /// Get execution ID if execution-scoped
    pub fn execution_id(&self) -> Option<ExecutionId> {
        match self {
            Self::Execution { execution_id } => Some(*execution_id),
            _ => None,
        }
    }

    /// Get workflow execution ID if workflow-scoped
    pub fn workflow_execution_id(&self) -> Option<Uuid> {
        match self {
            Self::WorkflowExecution { workflow_execution_id } => Some(*workflow_execution_id),
            _ => None,
        }
    }
}

/// Volume status lifecycle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeStatus {
    /// Volume is being created
    Creating,
    /// Volume is available for mounting
    Available,
    /// Volume is currently attached to an instance
    Attached,
    /// Volume is detached but still exists
    Detached,
    /// Volume is being deleted
    Deleting,
    /// Volume has been deleted
    Deleted,
    /// Volume creation or operation failed
    Failed,
}

impl VolumeStatus {
    pub fn can_attach(&self) -> bool {
        matches!(self, Self::Available | Self::Detached)
    }

    pub fn can_detach(&self) -> bool {
        matches!(self, Self::Attached)
    }

    pub fn can_delete(&self) -> bool {
        !matches!(self, Self::Deleting | Self::Deleted)
    }
}

// ============================================================================
// Aggregate Root: Volume
// ============================================================================

/// Volume aggregate root
/// 
/// Represents an isolated storage context with lifecycle independent of agent execution.
/// Follows DDD pattern from AGENTS.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    /// Unique volume identifier
    pub id: VolumeId,
    
    /// Human-readable volume name
    pub name: String,
    
    /// Tenant identifier for multi-tenant isolation
    pub tenant_id: TenantId,
    
    /// Storage classification (ephemeral or persistent)
    pub storage_class: StorageClass,
    
    /// SeaweedFS filer endpoint
    pub filer_endpoint: FilerEndpoint,
    
    /// Remote path on SeaweedFS (e.g., "/aegis/volumes/{tenant_id}/{volume_id}")
    pub remote_path: String,
    
    /// Size limit in bytes (enforced by SeaweedFS quota)
    pub size_limit_bytes: u64,
    
    /// Current volume status
    pub status: VolumeStatus,
    
    /// Ownership model
    pub ownership: VolumeOwnership,
    
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    
    /// Last attached timestamp
    pub attached_at: Option<DateTime<Utc>>,
    
    /// Last detached timestamp
    pub detached_at: Option<DateTime<Utc>>,
    
    /// Expiration timestamp (for ephemeral volumes)
    pub expires_at: Option<DateTime<Utc>>,
}

impl Volume {
    /// Create a new volume (aggregate factory method)
    pub fn new(
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        filer_endpoint: FilerEndpoint,
        size_limit_bytes: u64,
        ownership: VolumeOwnership,
    ) -> Result<Self, VolumeError> {
        // Invariant: size_limit must be positive
        if size_limit_bytes == 0 {
            return Err(VolumeError::InvalidSizeLimit(
                "Size limit must be greater than zero".to_string()
            ));
        }

        // Invariant: name must not be empty
        if name.trim().is_empty() {
            return Err(VolumeError::InvalidName("Volume name cannot be empty".to_string()));
        }

        let id = VolumeId::new();
        let created_at = Utc::now();
        let expires_at = storage_class.calculate_expiry(created_at);

        // Construct remote path: /aegis/volumes/{tenant_id}/{volume_id}
        let remote_path = format!("/aegis/volumes/{}/{}", tenant_id, id);

        Ok(Self {
            id,
            name,
            tenant_id,
            storage_class,
            filer_endpoint,
            remote_path,
            size_limit_bytes,
            status: VolumeStatus::Creating,
            ownership,
            created_at,
            attached_at: None,
            detached_at: None,
            expires_at,
        })
    }

    // ========================================================================
    // Aggregate Commands (State Mutations)
    // ========================================================================

    /// Mark volume as available after successful creation
    pub fn mark_available(&mut self) -> Result<(), VolumeError> {
        if self.status != VolumeStatus::Creating {
            return Err(VolumeError::InvalidStateTransition {
                from: self.status,
                to: VolumeStatus::Available,
            });
        }
        self.status = VolumeStatus::Available;
        Ok(())
    }

    /// Mark volume as attached to an instance
    pub fn mark_attached(&mut self) -> Result<(), VolumeError> {
        if !self.status.can_attach() {
            return Err(VolumeError::CannotAttach(format!(
                "Volume {} cannot be attached in state {:?}",
                self.id, self.status
            )));
        }
        self.status = VolumeStatus::Attached;
        self.attached_at = Some(Utc::now());
        Ok(())
    }

    /// Mark volume as detached from an instance
    pub fn mark_detached(&mut self) -> Result<(), VolumeError> {
        if !self.status.can_detach() {
            return Err(VolumeError::CannotDetach(format!(
                "Volume {} cannot be detached in state {:?}",
                self.id, self.status
            )));
        }
        self.status = VolumeStatus::Detached;
        self.detached_at = Some(Utc::now());
        Ok(())
    }

    /// Mark volume as failed
    pub fn mark_failed(&mut self) {
        self.status = VolumeStatus::Failed;
    }

    /// Mark volume as deleting
    pub fn mark_deleting(&mut self) -> Result<(), VolumeError> {
        if !self.status.can_delete() {
            return Err(VolumeError::CannotDelete(format!(
                "Volume {} cannot be deleted in state {:?}",
                self.id, self.status
            )));
        }
        self.status = VolumeStatus::Deleting;
        Ok(())
    }

    /// Mark volume as deleted
    pub fn mark_deleted(&mut self) -> Result<(), VolumeError> {
        if self.status != VolumeStatus::Deleting {
            return Err(VolumeError::InvalidStateTransition {
                from: self.status,
                to: VolumeStatus::Deleted,
            });
        }
        self.status = VolumeStatus::Deleted;
        Ok(())
    }

    // ========================================================================
    // Aggregate Queries (State Inspection)
    // ========================================================================

    /// Check if volume can be attached
    pub fn can_attach(&self) -> bool {
        self.status.can_attach()
    }

    /// Check if volume can be detached
    pub fn can_detach(&self) -> bool {
        self.status.can_detach()
    }

    /// Check if volume can be deleted
    pub fn can_delete(&self) -> bool {
        self.status.can_delete()
    }

    /// Check if volume is expired (ephemeral only)
    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            Utc::now() > expires_at
        } else {
            false
        }
    }

    /// Get age of volume
    pub fn age(&self) -> Duration {
        Utc::now() - self.created_at
    }

    /// Create volume mount specification
    pub fn to_mount(&self, mount_point: PathBuf, access_mode: AccessMode) -> VolumeMount {
        VolumeMount::new(
            self.id,
            mount_point,
            access_mode,
            self.filer_endpoint.clone(),
            self.remote_path.clone(),
        )
    }
}

// ============================================================================
// Domain Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum VolumeError {
    #[error("Invalid volume name: {0}")]
    InvalidName(String),

    #[error("Invalid size limit: {0}")]
    InvalidSizeLimit(String),

    #[error("Invalid filer endpoint: {0}")]
    InvalidFilerEndpoint(String),

    #[error("Cannot attach volume: {0}")]
    CannotAttach(String),

    #[error("Cannot detach volume: {0}")]
    CannotDetach(String),

    #[error("Cannot delete volume: {0}")]
    CannotDelete(String),

    #[error("Invalid state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        from: VolumeStatus,
        to: VolumeStatus,
    },

    #[error("Volume not found: {0}")]
    NotFound(VolumeId),

    #[error("Volume quota exceeded: {0}")]
    QuotaExceeded(String),
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_id_creation() {
        let id1 = VolumeId::new();
        let id2 = VolumeId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_volume_id_from_string() {
        let uuid_str = "123e4567-e89b-12d3-a456-426614174000";
        let id = VolumeId::from_string(uuid_str).unwrap();
        assert_eq!(id.0.to_string(), uuid_str);
    }

    #[test]
    fn test_tenant_id_default() {
        let tenant = TenantId::default();
        assert_eq!(tenant.0.to_string(), "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn test_storage_class_ephemeral() {
        let storage = StorageClass::ephemeral_hours(24);
        assert!(storage.is_ephemeral());
        assert_eq!(storage.ttl(), Some(Duration::hours(24)));
    }

    #[test]
    fn test_storage_class_persistent() {
        let storage = StorageClass::persistent();
        assert!(!storage.is_ephemeral());
        assert_eq!(storage.ttl(), None);
    }

    #[test]
    fn test_storage_class_expiry_calculation() {
        let storage = StorageClass::ephemeral_hours(24);
        let created_at = Utc::now();
        let expires_at = storage.calculate_expiry(created_at).unwrap();
        assert_eq!(expires_at, created_at + Duration::hours(24));
    }

    #[test]
    fn test_filer_endpoint_validation() {
        assert!(FilerEndpoint::new("http://localhost:8888").is_ok());
        assert!(FilerEndpoint::new("https://filer.example.com").is_ok());
        assert!(FilerEndpoint::new("invalid-url").is_err());
    }

    #[test]
    fn test_filer_endpoint_host() {
        let endpoint = FilerEndpoint::new("http://localhost:8888").unwrap();
        assert_eq!(endpoint.host().unwrap(), "localhost:8888");
    }

    #[test]
    fn test_volume_creation() {
        let volume = Volume::new(
            "test-volume".to_string(),
            TenantId::default(),
            StorageClass::ephemeral_hours(24),
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            1_000_000_000, // 1GB
            VolumeOwnership::persistent("user-123"),
        ).unwrap();

        assert_eq!(volume.name, "test-volume");
        assert_eq!(volume.status, VolumeStatus::Creating);
        assert!(volume.expires_at.is_some());
        assert_eq!(volume.remote_path, format!("/aegis/volumes/{}/{}", volume.tenant_id, volume.id));
    }

    #[test]
    fn test_volume_creation_invalid_size() {
        let result = Volume::new(
            "test-volume".to_string(),
            TenantId::default(),
            StorageClass::persistent(),
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            0, // Invalid: zero size
            VolumeOwnership::persistent("user-123"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_volume_creation_empty_name() {
        let result = Volume::new(
            "".to_string(), // Invalid: empty name
            TenantId::default(),
            StorageClass::persistent(),
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            1_000_000_000,
            VolumeOwnership::persistent("user-123"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_volume_state_transitions() {
        let mut volume = Volume::new(
            "test-volume".to_string(),
            TenantId::default(),
            StorageClass::persistent(),
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            1_000_000_000,
            VolumeOwnership::persistent("user-123"),
        ).unwrap();

        // Creating -> Available
        assert!(volume.mark_available().is_ok());
        assert_eq!(volume.status, VolumeStatus::Available);

        // Available -> Attached
        assert!(volume.mark_attached().is_ok());
        assert_eq!(volume.status, VolumeStatus::Attached);
        assert!(volume.attached_at.is_some());

        // Attached -> Detached
        assert!(volume.mark_detached().is_ok());
        assert_eq!(volume.status, VolumeStatus::Detached);
        assert!(volume.detached_at.is_some());

        // Detached -> Deleting
        assert!(volume.mark_deleting().is_ok());
        assert_eq!(volume.status, VolumeStatus::Deleting);

        // Deleting -> Deleted
        assert!(volume.mark_deleted().is_ok());
        assert_eq!(volume.status, VolumeStatus::Deleted);
    }

    #[test]
    fn test_volume_invalid_state_transition() {
        let mut volume = Volume::new(
            "test-volume".to_string(),
            TenantId::default(),
            StorageClass::persistent(),
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            1_000_000_000,
            VolumeOwnership::persistent("user-123"),
        ).unwrap();

        // Cannot attach while Creating
        assert!(volume.mark_attached().is_err());

        // Transition to Available
        volume.mark_available().unwrap();

        // Cannot mark as deleted without going through Deleting
        assert!(volume.mark_deleted().is_err());
    }

    #[test]
    fn test_volume_expiry_check() {
        let storage = StorageClass::ephemeral(Duration::seconds(1)); // 1 second TTL
        let mut volume = Volume::new(
            "test-volume".to_string(),
            TenantId::default(),
            storage,
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            1_000_000_000,
            VolumeOwnership::persistent("user-123"),
        ).unwrap();

        // Should not be expired immediately
        assert!(!volume.is_expired());

        // Manually set expires_at to past
        volume.expires_at = Some(Utc::now() - Duration::hours(1));
        assert!(volume.is_expired());
    }

    #[test]
    fn test_volume_mount_creation() {
        let volume = Volume::new(
            "test-volume".to_string(),
            TenantId::default(),
            StorageClass::persistent(),
            FilerEndpoint::new("http://localhost:8888").unwrap(),
            1_000_000_000,
            VolumeOwnership::persistent("user-123"),
        ).unwrap();

        let mount = volume.to_mount(PathBuf::from("/workspace"), AccessMode::ReadWrite);
        assert_eq!(mount.volume_id, volume.id);
        assert_eq!(mount.mount_point, PathBuf::from("/workspace"));
        assert_eq!(mount.access_mode, AccessMode::ReadWrite);
    }

    #[test]
    fn test_volume_ownership_patterns() {
        let exec_id = ExecutionId::new();
        let workflow_id = Uuid::new_v4();

        let exec_ownership = VolumeOwnership::execution(exec_id);
        assert_eq!(exec_ownership.execution_id(), Some(exec_id));
        assert!(exec_ownership.workflow_execution_id().is_none());

        let workflow_ownership = VolumeOwnership::workflow(workflow_id);
        assert!(workflow_ownership.execution_id().is_none());
        assert_eq!(workflow_ownership.workflow_execution_id(), Some(workflow_id));

        let persistent_ownership = VolumeOwnership::persistent("user-123");
        assert!(persistent_ownership.execution_id().is_none());
        assert!(persistent_ownership.workflow_execution_id().is_none());
    }
}
