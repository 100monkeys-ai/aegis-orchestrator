// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # GitRepoBinding Domain Aggregate Tests (BC-7, ADR-081)
//!
//! Unit tests for the [`GitRepoBinding`] aggregate root covering:
//!
//! - Status transitions (`Pending → Cloning → Ready`,
//!   `Ready → Refreshing → Ready`, `Cloning → Failed`, `Refreshing → Failed`)
//! - Domain-event emission at every transition
//! - [`GitRepoTierLimits::for_tier`] resolution per [`ZaruTier`]
//! - [`validate_repo_url`] URL allow-list (HTTPS + git@ only, no IP hosts)
//! - [`GitRef::default`] returns `Branch("main")`

use aegis_orchestrator_core::domain::git_repo::{
    validate_repo_url, CloneStrategy, GitRef, GitRepoBinding, GitRepoEvent, GitRepoStatus,
};
use aegis_orchestrator_core::domain::git_repo_tier_limits::GitRepoTierLimits;
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};

// ============================================================================
// Test Helpers
// ============================================================================

fn make_binding() -> GitRepoBinding {
    GitRepoBinding::new(
        TenantId::consumer(),
        None,
        "https://github.com/octocat/Hello-World.git".to_string(),
        GitRef::default(),
        None,
        VolumeId::new(),
        "hello-world".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    )
}

// ============================================================================
// GitRef::default
// ============================================================================

#[test]
fn git_ref_default_is_branch_main() {
    assert_eq!(GitRef::default(), GitRef::Branch("main".to_string()));
}

// ============================================================================
// Status transitions + event emission
// ============================================================================

#[test]
fn new_binding_emits_binding_created_event() {
    let mut binding = make_binding();
    assert_eq!(binding.status, GitRepoStatus::Pending);
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        GitRepoEvent::BindingCreated {
            id,
            repo_url,
            git_ref,
            volume_id,
            ..
        } => {
            assert_eq!(*id, binding.id);
            assert_eq!(repo_url, "https://github.com/octocat/Hello-World.git");
            assert_eq!(*git_ref, GitRef::Branch("main".to_string()));
            assert_eq!(*volume_id, binding.volume_id);
        }
        other => panic!("expected BindingCreated, got {other:?}"),
    }
}

#[test]
fn pending_to_cloning_to_ready() {
    let mut binding = make_binding();
    let _ = binding.take_events(); // drain BindingCreated

    binding.start_clone();
    assert_eq!(binding.status, GitRepoStatus::Cloning);
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        GitRepoEvent::CloneStarted {
            id,
            volume_id,
            strategy,
            ..
        } => {
            assert_eq!(*id, binding.id);
            assert_eq!(*volume_id, binding.volume_id);
            assert_eq!(*strategy, CloneStrategy::Libgit2);
        }
        other => panic!("expected CloneStarted, got {other:?}"),
    }

    binding.complete_clone("abc123".to_string(), 4200);
    assert_eq!(binding.status, GitRepoStatus::Ready);
    assert_eq!(binding.last_commit_sha.as_deref(), Some("abc123"));
    assert!(binding.last_cloned_at.is_some());
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        GitRepoEvent::CloneCompleted {
            id,
            commit_sha,
            duration_ms,
            ..
        } => {
            assert_eq!(*id, binding.id);
            assert_eq!(commit_sha, "abc123");
            assert_eq!(*duration_ms, 4200);
        }
        other => panic!("expected CloneCompleted, got {other:?}"),
    }
}

#[test]
fn ready_to_refreshing_to_ready() {
    let mut binding = make_binding();
    binding.start_clone();
    binding.complete_clone("old-sha".to_string(), 1000);
    let _ = binding.take_events();

    binding.start_refresh();
    assert_eq!(binding.status, GitRepoStatus::Refreshing);
    let events = binding.take_events();
    assert!(matches!(events[0], GitRepoEvent::RefreshStarted { .. }));

    binding.complete_refresh("old-sha".to_string(), "new-sha".to_string(), 500);
    assert_eq!(binding.status, GitRepoStatus::Ready);
    assert_eq!(binding.last_commit_sha.as_deref(), Some("new-sha"));
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        GitRepoEvent::RefreshCompleted {
            old_commit_sha,
            new_commit_sha,
            duration_ms,
            ..
        } => {
            assert_eq!(old_commit_sha, "old-sha");
            assert_eq!(new_commit_sha, "new-sha");
            assert_eq!(*duration_ms, 500);
        }
        other => panic!("expected RefreshCompleted, got {other:?}"),
    }
}

#[test]
fn cloning_to_failed() {
    let mut binding = make_binding();
    binding.start_clone();
    let _ = binding.take_events();

    binding.fail_clone("network unreachable".to_string());
    match &binding.status {
        GitRepoStatus::Failed { error } => assert_eq!(error, "network unreachable"),
        other => panic!("expected Failed, got {other:?}"),
    }
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        GitRepoEvent::CloneFailed { id, error, .. } => {
            assert_eq!(*id, binding.id);
            assert_eq!(error, "network unreachable");
        }
        other => panic!("expected CloneFailed, got {other:?}"),
    }
}

#[test]
fn refreshing_to_failed() {
    let mut binding = make_binding();
    binding.start_clone();
    binding.complete_clone("sha".to_string(), 1);
    binding.start_refresh();
    let _ = binding.take_events();

    binding.fail_refresh("remote ref not found".to_string());
    match &binding.status {
        GitRepoStatus::Failed { error } => assert_eq!(error, "remote ref not found"),
        other => panic!("expected Failed, got {other:?}"),
    }
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], GitRepoEvent::RefreshFailed { .. }));
}

#[test]
fn mark_deleted_emits_binding_deleted() {
    let mut binding = make_binding();
    binding.start_clone();
    binding.complete_clone("sha".to_string(), 1);
    let _ = binding.take_events();

    binding.mark_deleted();
    assert_eq!(binding.status, GitRepoStatus::Deleted);
    let events = binding.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        GitRepoEvent::BindingDeleted { id, volume_id, .. } => {
            assert_eq!(*id, binding.id);
            assert_eq!(*volume_id, binding.volume_id);
        }
        other => panic!("expected BindingDeleted, got {other:?}"),
    }
}

#[test]
fn take_events_drains_buffer() {
    let mut binding = make_binding();
    let first = binding.take_events();
    assert_eq!(first.len(), 1);
    let second = binding.take_events();
    assert!(second.is_empty());
}

// ============================================================================
// Tier limits
// ============================================================================

#[test]
fn tier_limits_free() {
    let limits = GitRepoTierLimits::for_tier(ZaruTier::Free);
    assert_eq!(limits.max_bindings, Some(1));
    assert!(!limits.auto_refresh);
    assert!(!limits.sparse_checkout);
}

#[test]
fn tier_limits_pro() {
    let limits = GitRepoTierLimits::for_tier(ZaruTier::Pro);
    assert_eq!(limits.max_bindings, Some(5));
    assert!(limits.auto_refresh);
    assert!(limits.sparse_checkout);
}

#[test]
fn tier_limits_business() {
    let limits = GitRepoTierLimits::for_tier(ZaruTier::Business);
    assert_eq!(limits.max_bindings, Some(20));
    assert!(limits.auto_refresh);
    assert!(limits.sparse_checkout);
}

#[test]
fn tier_limits_enterprise() {
    let limits = GitRepoTierLimits::for_tier(ZaruTier::Enterprise);
    assert_eq!(limits.max_bindings, None);
    assert!(limits.auto_refresh);
    assert!(limits.sparse_checkout);
}

// ============================================================================
// URL validation
// ============================================================================

#[test]
fn url_validation_accepts_https_github() {
    assert!(validate_repo_url("https://github.com/octocat/Hello-World.git").is_ok());
}

#[test]
fn url_validation_accepts_git_ssh_shortcut() {
    assert!(validate_repo_url("git@github.com:octocat/Hello-World.git").is_ok());
}

#[test]
fn url_validation_rejects_file_scheme() {
    assert!(validate_repo_url("file:///tmp/evil.git").is_err());
}

#[test]
fn url_validation_rejects_ftp_scheme() {
    assert!(validate_repo_url("ftp://example.com/repo.git").is_err());
}

#[test]
fn url_validation_rejects_plain_http() {
    assert!(validate_repo_url("http://github.com/foo/bar").is_err());
}

#[test]
fn url_validation_rejects_ipv4_host() {
    assert!(validate_repo_url("https://192.168.1.1/foo").is_err());
}

#[test]
fn url_validation_rejects_ipv6_host() {
    assert!(validate_repo_url("https://[::1]/foo").is_err());
}

#[test]
fn url_validation_rejects_empty_string() {
    assert!(validate_repo_url("").is_err());
}

#[test]
fn url_validation_rejects_ssh_with_ip_host() {
    assert!(validate_repo_url("git@10.0.0.1:foo/bar.git").is_err());
}
