// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Domain-layer unit tests for the **Storage Gateway** bounded context (BC-7).
//!
//! Covers the `Volume` aggregate root, `StorageClass`, `VolumeOwnership`,
//! `FilerEndpoint`, `PathSanitizer`, and cross-cutting `SecurityPolicy` /
//! `NetworkPolicy` / `FilesystemPolicy` / `ResourceLimits` types.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Validates invariants, state transitions, and serialization
//!   round-trips for Volume and related value objects

use std::path::PathBuf;

use chrono::{Duration, Utc};
use uuid::Uuid;

use aegis_orchestrator_core::domain::path_sanitizer::PathSanitizer;
use aegis_orchestrator_core::domain::policy::{
    FilesystemPolicy, IsolationType, NetworkPolicy, PolicyMode, ResourceLimits, SecurityPolicy,
};
use aegis_orchestrator_core::domain::shared_kernel::{ExecutionId, TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
    VolumeStatus,
};

// ============================================================================
// Helpers
// ============================================================================

/// Build a minimal valid `Volume` with SeaweedFS backend and persistent storage.
fn make_volume(name: &str, storage_class: StorageClass) -> Volume {
    Volume::new(
        name.to_string(),
        TenantId::default(),
        storage_class,
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
            remote_path: "/aegis/test-volume".to_string(),
        },
        1_073_741_824, // 1 GiB
        VolumeOwnership::persistent("test-user"),
    )
    .expect("helper must create a valid volume")
}

fn make_persistent_volume(name: &str) -> Volume {
    make_volume(name, StorageClass::persistent())
}

fn make_ephemeral_volume(name: &str, ttl: Duration) -> Volume {
    make_volume(name, StorageClass::ephemeral(ttl))
}

// ============================================================================
// 1. Volume::new() — valid creation & defaults
// ============================================================================

#[test]
fn volume_new_creates_with_creating_status() {
    let vol = make_persistent_volume("my-vol");
    assert_eq!(vol.status, VolumeStatus::Creating);
    assert_eq!(vol.name, "my-vol");
    assert!(vol.attached_at.is_none());
    assert!(vol.detached_at.is_none());
}

#[test]
fn volume_new_ephemeral_sets_expires_at() {
    let vol = make_ephemeral_volume("eph-vol", Duration::hours(6));
    assert!(vol.expires_at.is_some());

    let expected_lower = Utc::now() + Duration::hours(6) - Duration::seconds(5);
    let expected_upper = Utc::now() + Duration::hours(6) + Duration::seconds(5);
    let expires = vol.expires_at.unwrap();
    assert!(expires > expected_lower && expires < expected_upper);
}

#[test]
fn volume_new_persistent_has_no_expiry() {
    let vol = make_persistent_volume("persist-vol");
    assert!(vol.expires_at.is_none());
}

#[test]
fn volume_new_rejects_empty_name() {
    let result = Volume::new(
        "".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
            remote_path: "/p".to_string(),
        },
        1024,
        VolumeOwnership::persistent("u"),
    );
    assert!(result.is_err());
}

#[test]
fn volume_new_rejects_whitespace_only_name() {
    let result = Volume::new(
        "   ".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
            remote_path: "/p".to_string(),
        },
        1024,
        VolumeOwnership::persistent("u"),
    );
    assert!(result.is_err());
}

#[test]
fn volume_new_rejects_zero_size_limit() {
    let result = Volume::new(
        "name".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
            remote_path: "/p".to_string(),
        },
        0,
        VolumeOwnership::persistent("u"),
    );
    assert!(result.is_err());
}

// ============================================================================
// 2. Volume status transitions — happy path
// ============================================================================

#[test]
fn volume_happy_path_creating_through_deleted() {
    let mut vol = make_persistent_volume("lifecycle");

    // Creating → Available
    vol.mark_available().unwrap();
    assert_eq!(vol.status, VolumeStatus::Available);

    // Available → Attached
    vol.mark_attached().unwrap();
    assert_eq!(vol.status, VolumeStatus::Attached);
    assert!(vol.attached_at.is_some());

    // Attached → Detached
    vol.mark_detached().unwrap();
    assert_eq!(vol.status, VolumeStatus::Detached);
    assert!(vol.detached_at.is_some());

    // Detached → Deleting
    vol.mark_deleting().unwrap();
    assert_eq!(vol.status, VolumeStatus::Deleting);

    // Deleting → Deleted
    vol.mark_deleted().unwrap();
    assert_eq!(vol.status, VolumeStatus::Deleted);
}

#[test]
fn volume_available_can_go_directly_to_deleting() {
    let mut vol = make_persistent_volume("skip-attach");
    vol.mark_available().unwrap();
    vol.mark_deleting().unwrap();
    assert_eq!(vol.status, VolumeStatus::Deleting);
}

#[test]
fn volume_detached_can_reattach() {
    let mut vol = make_persistent_volume("reattach");
    vol.mark_available().unwrap();
    vol.mark_attached().unwrap();
    vol.mark_detached().unwrap();

    // Detached → Attached again
    vol.mark_attached().unwrap();
    assert_eq!(vol.status, VolumeStatus::Attached);
}

// ============================================================================
// 3. Invalid transitions
// ============================================================================

#[test]
fn volume_creating_cannot_attach() {
    let mut vol = make_persistent_volume("bad-attach");
    assert!(vol.mark_attached().is_err());
}

#[test]
fn volume_creating_cannot_detach() {
    let mut vol = make_persistent_volume("bad-detach");
    assert!(vol.mark_detached().is_err());
}

#[test]
fn volume_deleting_cannot_attach() {
    let mut vol = make_persistent_volume("deleting-attach");
    vol.mark_available().unwrap();
    vol.mark_deleting().unwrap();
    assert!(vol.mark_attached().is_err());
}

#[test]
fn volume_deleting_cannot_mark_available() {
    let mut vol = make_persistent_volume("deleting-avail");
    vol.mark_available().unwrap();
    vol.mark_deleting().unwrap();
    assert!(vol.mark_available().is_err());
}

#[test]
fn volume_deleted_cannot_transition_to_deleting() {
    let mut vol = make_persistent_volume("deleted-cycle");
    vol.mark_available().unwrap();
    vol.mark_deleting().unwrap();
    vol.mark_deleted().unwrap();
    assert!(vol.mark_deleting().is_err());
}

#[test]
fn volume_available_cannot_mark_deleted_directly() {
    let mut vol = make_persistent_volume("skip-deleting");
    vol.mark_available().unwrap();
    assert!(vol.mark_deleted().is_err());
}

// ============================================================================
// 4. can_attach / can_detach / can_delete predicates
// ============================================================================

#[test]
fn predicates_on_creating_volume() {
    let vol = make_persistent_volume("pred-creating");
    assert!(!vol.can_attach());
    assert!(!vol.can_detach());
    assert!(vol.can_delete());
}

#[test]
fn predicates_on_available_volume() {
    let mut vol = make_persistent_volume("pred-avail");
    vol.mark_available().unwrap();
    assert!(vol.can_attach());
    assert!(!vol.can_detach());
    assert!(vol.can_delete());
}

#[test]
fn predicates_on_attached_volume() {
    let mut vol = make_persistent_volume("pred-attached");
    vol.mark_available().unwrap();
    vol.mark_attached().unwrap();
    assert!(!vol.can_attach());
    assert!(vol.can_detach());
    assert!(vol.can_delete());
}

#[test]
fn predicates_on_detached_volume() {
    let mut vol = make_persistent_volume("pred-detached");
    vol.mark_available().unwrap();
    vol.mark_attached().unwrap();
    vol.mark_detached().unwrap();
    assert!(vol.can_attach());
    assert!(!vol.can_detach());
    assert!(vol.can_delete());
}

#[test]
fn predicates_on_deleting_volume() {
    let mut vol = make_persistent_volume("pred-deleting");
    vol.mark_available().unwrap();
    vol.mark_deleting().unwrap();
    assert!(!vol.can_attach());
    assert!(!vol.can_detach());
    assert!(!vol.can_delete());
}

#[test]
fn predicates_on_deleted_volume() {
    let mut vol = make_persistent_volume("pred-deleted");
    vol.mark_available().unwrap();
    vol.mark_deleting().unwrap();
    vol.mark_deleted().unwrap();
    assert!(!vol.can_attach());
    assert!(!vol.can_detach());
    assert!(!vol.can_delete());
}

// ============================================================================
// 5. is_expired()
// ============================================================================

#[test]
fn ephemeral_volume_not_expired_immediately() {
    let vol = make_ephemeral_volume("eph-fresh", Duration::hours(1));
    assert!(!vol.is_expired());
}

#[test]
fn ephemeral_volume_expired_when_past_ttl() {
    let mut vol = make_ephemeral_volume("eph-old", Duration::hours(1));
    // Force expiry into the past
    vol.expires_at = Some(Utc::now() - Duration::seconds(1));
    assert!(vol.is_expired());
}

#[test]
fn persistent_volume_never_expires() {
    let vol = make_persistent_volume("persist-forever");
    assert!(!vol.is_expired());
}

// ============================================================================
// 6. age()
// ============================================================================

#[test]
fn volume_age_is_non_negative() {
    let vol = make_persistent_volume("age-check");
    let age = vol.age();
    assert!(age.num_milliseconds() >= 0);
}

// ============================================================================
// 7. to_mount()
// ============================================================================

#[test]
fn to_mount_creates_correct_volume_mount() {
    let vol = make_persistent_volume("mount-test");
    let mount = vol.to_mount(PathBuf::from("/workspace"), AccessMode::ReadWrite);

    assert_eq!(mount.volume_id, vol.id);
    assert_eq!(mount.mount_point, PathBuf::from("/workspace"));
    assert_eq!(mount.access_mode, AccessMode::ReadWrite);
    assert_eq!(mount.filer_endpoint.url, "http://localhost:8888");
    assert_eq!(mount.remote_path, "/aegis/test-volume");
}

#[test]
fn to_mount_read_only() {
    let vol = make_persistent_volume("mount-ro");
    let mount = vol.to_mount(PathBuf::from("/data"), AccessMode::ReadOnly);
    assert_eq!(mount.access_mode, AccessMode::ReadOnly);
    assert!(!mount.access_mode.is_writable());
}

#[test]
fn to_mount_non_seaweedfs_uses_fallback() {
    let vol = Volume::new(
        "hostpath-vol".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::HostPath {
            path: PathBuf::from("/mnt/data"),
        },
        1024,
        VolumeOwnership::persistent("user"),
    )
    .unwrap();

    let mount = vol.to_mount(PathBuf::from("/workspace"), AccessMode::ReadWrite);
    assert_eq!(mount.filer_endpoint.url, "http://localhost:8888");
    assert!(mount.remote_path.contains(&vol.tenant_id.to_string()));
    assert!(mount.remote_path.contains(&vol.id.to_string()));
}

// ============================================================================
// 8. StorageClass — constructors & predicates
// ============================================================================

#[test]
fn storage_class_ephemeral_with_duration() {
    let sc = StorageClass::ephemeral(Duration::minutes(90));
    assert!(sc.is_ephemeral());
    assert_eq!(sc.ttl().unwrap().num_minutes(), 90);
}

#[test]
fn storage_class_ephemeral_hours() {
    let sc = StorageClass::ephemeral_hours(48);
    assert!(sc.is_ephemeral());
    assert_eq!(sc.ttl().unwrap().num_hours(), 48);
}

#[test]
fn storage_class_persistent() {
    let sc = StorageClass::persistent();
    assert!(!sc.is_ephemeral());
    assert!(sc.ttl().is_none());
}

#[test]
fn storage_class_default_is_ephemeral_24h() {
    let sc = StorageClass::default();
    assert!(sc.is_ephemeral());
    assert_eq!(sc.ttl().unwrap().num_hours(), 24);
}

// ============================================================================
// 9. StorageClass::calculate_expiry()
// ============================================================================

#[test]
fn calculate_expiry_ephemeral_returns_some() {
    let sc = StorageClass::ephemeral_hours(12);
    let now = Utc::now();
    let expiry = sc.calculate_expiry(now).unwrap();
    assert_eq!(expiry, now + Duration::hours(12));
}

#[test]
fn calculate_expiry_persistent_returns_none() {
    let sc = StorageClass::persistent();
    assert!(sc.calculate_expiry(Utc::now()).is_none());
}

// ============================================================================
// 10. VolumeOwnership constructors & accessors
// ============================================================================

#[test]
fn ownership_execution() {
    let eid = ExecutionId::new();
    let o = VolumeOwnership::execution(eid);
    assert_eq!(o.execution_id(), Some(eid));
    assert!(o.workflow_execution_id().is_none());
}

#[test]
fn ownership_workflow() {
    let wid = Uuid::new_v4();
    let o = VolumeOwnership::workflow(wid);
    assert!(o.execution_id().is_none());
    assert_eq!(o.workflow_execution_id(), Some(wid));
}

#[test]
fn ownership_persistent() {
    let o = VolumeOwnership::persistent("alice");
    assert!(o.execution_id().is_none());
    assert!(o.workflow_execution_id().is_none());
}

// ============================================================================
// 11. FilerEndpoint
// ============================================================================

#[test]
fn filer_endpoint_valid_http() {
    let ep = FilerEndpoint::new("http://filer.internal:8888").unwrap();
    assert_eq!(ep.url, "http://filer.internal:8888");
}

#[test]
fn filer_endpoint_valid_https() {
    let ep = FilerEndpoint::new("https://filer.prod.example.com").unwrap();
    assert_eq!(ep.url, "https://filer.prod.example.com");
}

#[test]
fn filer_endpoint_rejects_no_scheme() {
    assert!(FilerEndpoint::new("filer.example.com:8888").is_err());
}

#[test]
fn filer_endpoint_rejects_ftp() {
    assert!(FilerEndpoint::new("ftp://filer.example.com").is_err());
}

#[test]
fn filer_endpoint_host_extraction() {
    let ep = FilerEndpoint::new("http://seaweed.local:9333").unwrap();
    assert_eq!(ep.host().unwrap(), "seaweed.local:9333");
}

#[test]
fn filer_endpoint_host_default_port() {
    let ep = FilerEndpoint::new("http://seaweed.local").unwrap();
    // No explicit port → default 8888
    assert_eq!(ep.host().unwrap(), "seaweed.local:8888");
}

#[test]
fn filer_endpoint_display() {
    let ep = FilerEndpoint::new("http://localhost:8888").unwrap();
    assert_eq!(format!("{ep}"), "http://localhost:8888");
}

// ============================================================================
// 12. PathSanitizer::canonicalize()
// ============================================================================

#[test]
fn sanitizer_normal_path() {
    let s = PathSanitizer::new();
    let p = s
        .canonicalize("/workspace/src/main.rs", Some("/workspace"))
        .unwrap();
    assert_eq!(p, PathBuf::from("/workspace/src/main.rs"));
}

#[test]
fn sanitizer_dot_removal() {
    let s = PathSanitizer::new();
    let p = s
        .canonicalize("/workspace/./src/./file.rs", Some("/workspace"))
        .unwrap();
    assert_eq!(p, PathBuf::from("/workspace/src/file.rs"));
}

#[test]
fn sanitizer_rejects_double_dot() {
    let s = PathSanitizer::new();
    assert!(s
        .canonicalize("/workspace/../etc/passwd", Some("/workspace"))
        .is_err());
}

#[test]
fn sanitizer_rejects_relative_traversal() {
    let s = PathSanitizer::new();
    assert!(s.canonicalize("../../etc/shadow", None).is_err());
}

#[test]
fn sanitizer_boundary_enforcement() {
    let s = PathSanitizer::new();
    assert!(s.canonicalize("/etc/passwd", Some("/workspace")).is_err());
}

#[test]
fn sanitizer_relative_path_joined_to_root() {
    let s = PathSanitizer::new();
    let p = s
        .canonicalize("subdir/file.txt", Some("/workspace"))
        .unwrap();
    assert_eq!(p, PathBuf::from("/workspace/subdir/file.txt"));
}

#[test]
fn sanitizer_no_root_just_normalizes() {
    let s = PathSanitizer::new();
    let p = s.canonicalize("/any/path/file.txt", None).unwrap();
    assert_eq!(p, PathBuf::from("/any/path/file.txt"));
}

#[test]
fn sanitizer_path_too_long() {
    let s = PathSanitizer::with_max_length(20);
    assert!(s
        .canonicalize("/workspace/a-very-long-path-name", None)
        .is_err());
}

// ============================================================================
// 13. PathSanitizer::validate()
// ============================================================================

#[test]
fn validate_rejects_double_dot() {
    let s = PathSanitizer::new();
    assert!(s.validate("/workspace/../etc").is_err());
}

#[test]
fn validate_rejects_null_bytes() {
    let s = PathSanitizer::new();
    assert!(s.validate("/path\0/null").is_err());
}

#[test]
fn validate_accepts_clean_path() {
    let s = PathSanitizer::new();
    assert!(s.validate("/workspace/src/main.rs").is_ok());
}

// ============================================================================
// 14. PathSanitizer::strip_volume_root()
// ============================================================================

#[test]
fn strip_volume_root_extracts_relative() {
    let s = PathSanitizer::new();
    let rel = s
        .strip_volume_root("/workspace/sub/file.txt", "/workspace")
        .unwrap();
    assert_eq!(rel, PathBuf::from("sub/file.txt"));
}

#[test]
fn strip_volume_root_exact_root_gives_empty() {
    let s = PathSanitizer::new();
    let rel = s.strip_volume_root("/workspace", "/workspace").unwrap();
    assert_eq!(rel, PathBuf::from(""));
}

#[test]
fn strip_volume_root_outside_fails() {
    let s = PathSanitizer::new();
    assert!(s.strip_volume_root("/etc/passwd", "/workspace").is_err());
}

// ============================================================================
// 15. NetworkPolicy::allows()
// ============================================================================

#[test]
fn network_policy_allow_exact_match() {
    let p = NetworkPolicy::new(PolicyMode::Allow, vec!["api.github.com".into()]);
    assert!(p.allows("api.github.com"));
    assert!(!p.allows("evil.com"));
}

#[test]
fn network_policy_allow_wildcard_subdomain() {
    let p = NetworkPolicy::new(PolicyMode::Allow, vec!["*.example.com".into()]);
    assert!(p.allows("api.example.com"));
    assert!(p.allows("deep.example.com"));
    assert!(!p.allows("evil.com"));
}

#[test]
fn network_policy_deny_mode_blocks_listed() {
    let p = NetworkPolicy::new(PolicyMode::Deny, vec!["evil.com".into()]);
    assert!(!p.allows("evil.com"));
    assert!(p.allows("good.com"));
}

#[test]
fn network_policy_allow_empty_denies_all() {
    let p = NetworkPolicy::new(PolicyMode::Allow, vec![]);
    assert!(!p.allows("anything.com"));
}

#[test]
fn network_policy_deny_empty_allows_all() {
    let p = NetworkPolicy::new(PolicyMode::Deny, vec![]);
    assert!(p.allows("anything.com"));
}

// ============================================================================
// 16. FilesystemPolicy defaults
// ============================================================================

#[test]
fn filesystem_policy_default_workspace() {
    let fp = FilesystemPolicy::default();
    assert_eq!(fp.read, vec!["/workspace/**"]);
    assert_eq!(fp.write, vec!["/workspace/**"]);
}

// ============================================================================
// 17. ResourceLimits
// ============================================================================

#[test]
fn resource_limits_fields() {
    let rl = ResourceLimits {
        cpu_us: 500_000,
        memory_mb: 1024,
        timeout_seconds: 600,
    };
    assert_eq!(rl.cpu_us, 500_000);
    assert_eq!(rl.memory_mb, 1024);
    assert_eq!(rl.timeout_seconds, 600);
}

// ============================================================================
// 18. SecurityPolicy composition
// ============================================================================

#[test]
fn security_policy_composition() {
    let policy = SecurityPolicy {
        network: NetworkPolicy::new(PolicyMode::Allow, vec!["*.github.com".into()]),
        filesystem: FilesystemPolicy::default(),
        resources: ResourceLimits {
            cpu_us: 1_000_000,
            memory_mb: 512,
            timeout_seconds: 300,
        },
        isolation: IsolationType::Docker,
    };
    assert!(policy.network.allows("api.github.com"));
    assert!(!policy.network.allows("evil.com"));
    assert_eq!(policy.resources.memory_mb, 512);
    assert_eq!(policy.isolation, IsolationType::Docker);
}

// ============================================================================
// 19. Serialization round-trips
// ============================================================================

#[test]
fn serde_roundtrip_storage_class_ephemeral() {
    let sc = StorageClass::ephemeral_hours(12);
    let json = serde_json::to_string(&sc).unwrap();
    let back: StorageClass = serde_json::from_str(&json).unwrap();
    assert_eq!(sc, back);
}

#[test]
fn serde_roundtrip_storage_class_persistent() {
    let sc = StorageClass::persistent();
    let json = serde_json::to_string(&sc).unwrap();
    let back: StorageClass = serde_json::from_str(&json).unwrap();
    assert_eq!(sc, back);
}

#[test]
fn serde_roundtrip_filer_endpoint() {
    let ep = FilerEndpoint::new("http://filer:8888").unwrap();
    let json = serde_json::to_string(&ep).unwrap();
    let back: FilerEndpoint = serde_json::from_str(&json).unwrap();
    assert_eq!(ep, back);
}

#[test]
fn serde_roundtrip_volume_ownership_execution() {
    let o = VolumeOwnership::execution(ExecutionId::new());
    let json = serde_json::to_string(&o).unwrap();
    let back: VolumeOwnership = serde_json::from_str(&json).unwrap();
    assert_eq!(o, back);
}

#[test]
fn serde_roundtrip_volume_ownership_workflow() {
    let o = VolumeOwnership::workflow(Uuid::new_v4());
    let json = serde_json::to_string(&o).unwrap();
    let back: VolumeOwnership = serde_json::from_str(&json).unwrap();
    assert_eq!(o, back);
}

#[test]
fn serde_roundtrip_volume_ownership_persistent() {
    let o = VolumeOwnership::persistent("user-42");
    let json = serde_json::to_string(&o).unwrap();
    let back: VolumeOwnership = serde_json::from_str(&json).unwrap();
    assert_eq!(o, back);
}

#[test]
fn serde_roundtrip_volume() {
    let vol = make_persistent_volume("serde-vol");
    let json = serde_json::to_string(&vol).unwrap();
    let back: Volume = serde_json::from_str(&json).unwrap();
    assert_eq!(back.name, vol.name);
    assert_eq!(back.id, vol.id);
    assert_eq!(back.status, vol.status);
    assert_eq!(back.storage_class, vol.storage_class);
}

#[test]
fn serde_roundtrip_volume_mount() {
    let vol = make_persistent_volume("mount-serde");
    let mount = vol.to_mount(PathBuf::from("/workspace"), AccessMode::ReadOnly);
    let json = serde_json::to_string(&mount).unwrap();
    let back: VolumeMount = serde_json::from_str(&json).unwrap();
    assert_eq!(mount, back);
}

#[test]
fn serde_roundtrip_network_policy() {
    let np = NetworkPolicy::new(PolicyMode::Allow, vec!["*.github.com".into()]);
    let json = serde_json::to_string(&np).unwrap();
    let back: NetworkPolicy = serde_json::from_str(&json).unwrap();
    assert_eq!(np, back);
}

#[test]
fn serde_roundtrip_security_policy() {
    let sp = SecurityPolicy {
        network: NetworkPolicy::new(PolicyMode::Deny, vec!["evil.com".into()]),
        filesystem: FilesystemPolicy::default(),
        resources: ResourceLimits {
            cpu_us: 250_000,
            memory_mb: 256,
            timeout_seconds: 120,
        },
        isolation: IsolationType::Process,
    };
    let json = serde_json::to_string(&sp).unwrap();
    let back: SecurityPolicy = serde_json::from_str(&json).unwrap();
    assert_eq!(sp, back);
}

// ============================================================================
// 20. AccessMode
// ============================================================================

#[test]
fn access_mode_default_is_read_write() {
    assert_eq!(AccessMode::default(), AccessMode::ReadWrite);
}

#[test]
fn access_mode_writable_check() {
    assert!(AccessMode::ReadWrite.is_writable());
    assert!(!AccessMode::ReadOnly.is_writable());
}

// ============================================================================
// 21. VolumeStatus predicates exhaustive
// ============================================================================

#[test]
fn volume_status_can_attach_only_available_or_detached() {
    assert!(!VolumeStatus::Creating.can_attach());
    assert!(VolumeStatus::Available.can_attach());
    assert!(!VolumeStatus::Attached.can_attach());
    assert!(VolumeStatus::Detached.can_attach());
    assert!(!VolumeStatus::Deleting.can_attach());
    assert!(!VolumeStatus::Deleted.can_attach());
    assert!(!VolumeStatus::Failed.can_attach());
}

#[test]
fn volume_status_can_detach_only_attached() {
    assert!(!VolumeStatus::Creating.can_detach());
    assert!(!VolumeStatus::Available.can_detach());
    assert!(VolumeStatus::Attached.can_detach());
    assert!(!VolumeStatus::Detached.can_detach());
    assert!(!VolumeStatus::Deleting.can_detach());
    assert!(!VolumeStatus::Deleted.can_detach());
    assert!(!VolumeStatus::Failed.can_detach());
}

#[test]
fn volume_status_can_delete_except_deleting_and_deleted() {
    assert!(VolumeStatus::Creating.can_delete());
    assert!(VolumeStatus::Available.can_delete());
    assert!(VolumeStatus::Attached.can_delete());
    assert!(VolumeStatus::Detached.can_delete());
    assert!(!VolumeStatus::Deleting.can_delete());
    assert!(!VolumeStatus::Deleted.can_delete());
    assert!(VolumeStatus::Failed.can_delete());
}

// ============================================================================
// 22. mark_failed() — always succeeds
// ============================================================================

#[test]
fn mark_failed_from_creating() {
    let mut vol = make_persistent_volume("fail-creating");
    vol.mark_failed();
    assert_eq!(vol.status, VolumeStatus::Failed);
}

#[test]
fn mark_failed_from_available() {
    let mut vol = make_persistent_volume("fail-avail");
    vol.mark_available().unwrap();
    vol.mark_failed();
    assert_eq!(vol.status, VolumeStatus::Failed);
}

#[test]
fn failed_volume_can_be_deleted() {
    let mut vol = make_persistent_volume("fail-delete");
    vol.mark_failed();
    assert!(vol.can_delete());
    vol.mark_deleting().unwrap();
    assert_eq!(vol.status, VolumeStatus::Deleting);
}

// ============================================================================
// 23. VolumeId uniqueness
// ============================================================================

#[test]
fn volume_ids_are_unique() {
    let a = VolumeId::new();
    let b = VolumeId::new();
    assert_ne!(a, b);
}

#[test]
fn volume_id_from_string_roundtrip() {
    let id = VolumeId::new();
    let s = id.to_string();
    let parsed = VolumeId::from_string(&s).unwrap();
    assert_eq!(id, parsed);
}
