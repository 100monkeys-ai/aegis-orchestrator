// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # CanvasSession Domain Aggregate Tests (BC-7, ADR-106)
//!
//! Unit tests for the [`CanvasSession`] aggregate root covering:
//!
//! - Tier-limit resolution for every [`ZaruTier`] (ADR-106 §Sub-Decision 5)
//! - Status transitions (`Initializing → Ready → Idle → Terminated`)
//! - Domain-event emission on every state transition / audit marker
//! - Cross-field invariants: `GitLinked ⇔ git_binding_id == Some(matching)`
//! - Persistent workspace requires a non-empty `volume_label`

use aegis_orchestrator_core::domain::canvas::{
    CanvasEvent, CanvasSession, CanvasSessionStatus, CanvasTierLimits, ConversationId,
    WorkspaceMode, WorkspaceModeKind,
};
use aegis_orchestrator_core::domain::git_repo::GitRepoBindingId;
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};

// ============================================================================
// Helpers
// ============================================================================

fn ephemeral_session() -> CanvasSession {
    CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::Ephemeral,
        None,
    )
    .expect("valid ephemeral session")
}

// ============================================================================
// Tier-limit resolution (ADR-106 §Sub-Decision 5)
// ============================================================================

#[test]
fn tier_limits_free_allows_ephemeral_only() {
    let limits = CanvasTierLimits::for_tier(ZaruTier::Free);
    assert_eq!(
        limits.allowed_workspace_modes,
        vec![WorkspaceModeKind::Ephemeral]
    );
    assert!(!limits.git_linked);
    assert!(limits.is_allowed(&WorkspaceMode::Ephemeral));
    assert!(!limits.is_allowed(&WorkspaceMode::Persistent {
        volume_label: "x".into()
    }));
    assert!(!limits.is_allowed(&WorkspaceMode::GitLinked {
        binding_id: GitRepoBindingId::new(),
    }));
}

#[test]
fn tier_limits_pro_allows_ephemeral_and_persistent_and_flags_git_linked_true() {
    let limits = CanvasTierLimits::for_tier(ZaruTier::Pro);
    assert_eq!(
        limits.allowed_workspace_modes,
        vec![WorkspaceModeKind::Ephemeral, WorkspaceModeKind::Persistent]
    );
    assert!(limits.git_linked);
    assert!(limits.is_allowed(&WorkspaceMode::Ephemeral));
    assert!(limits.is_allowed(&WorkspaceMode::Persistent {
        volume_label: "proj".into()
    }));
    // Pro gets `git_linked: true` per the tier table but GitLinked *mode* is
    // reserved to Enterprise in the allowed-modes list. `is_allowed` must
    // reject the combination.
    assert!(!limits.is_allowed(&WorkspaceMode::GitLinked {
        binding_id: GitRepoBindingId::new(),
    }));
}

#[test]
fn tier_limits_business_matches_pro_modes() {
    let limits = CanvasTierLimits::for_tier(ZaruTier::Business);
    assert_eq!(
        limits.allowed_workspace_modes,
        vec![WorkspaceModeKind::Ephemeral, WorkspaceModeKind::Persistent]
    );
    assert!(limits.git_linked);
    assert!(!limits.is_allowed(&WorkspaceMode::GitLinked {
        binding_id: GitRepoBindingId::new(),
    }));
}

#[test]
fn tier_limits_enterprise_allows_all_modes_and_git_linked() {
    let limits = CanvasTierLimits::for_tier(ZaruTier::Enterprise);
    assert_eq!(
        limits.allowed_workspace_modes,
        vec![
            WorkspaceModeKind::Ephemeral,
            WorkspaceModeKind::Persistent,
            WorkspaceModeKind::GitLinked,
        ]
    );
    assert!(limits.git_linked);
    assert!(limits.is_allowed(&WorkspaceMode::Ephemeral));
    assert!(limits.is_allowed(&WorkspaceMode::Persistent {
        volume_label: "ent".into()
    }));
    assert!(limits.is_allowed(&WorkspaceMode::GitLinked {
        binding_id: GitRepoBindingId::new(),
    }));
}

// ============================================================================
// `CanvasSession::new` — construction + SessionCreated event
// ============================================================================

#[test]
fn new_session_is_initializing_and_emits_session_created() {
    let tenant = TenantId::consumer();
    let convo = ConversationId::new();
    let volume = VolumeId::new();
    let mut session = CanvasSession::new(
        tenant.clone(),
        convo,
        volume,
        WorkspaceMode::Ephemeral,
        None,
    )
    .expect("valid session");

    assert_eq!(session.status, CanvasSessionStatus::Initializing);
    assert_eq!(session.conversation_id, convo);
    assert_eq!(session.workspace_volume_id, volume);
    assert_eq!(session.tenant_id, tenant);
    assert!(session.git_binding_id.is_none());

    let events = session.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        CanvasEvent::SessionCreated {
            session_id,
            conversation_id,
            workspace_mode,
            tenant_id,
            ..
        } => {
            assert_eq!(*session_id, session.id);
            assert_eq!(*conversation_id, convo);
            assert_eq!(*workspace_mode, WorkspaceMode::Ephemeral);
            assert_eq!(*tenant_id, tenant);
        }
        other => panic!("expected SessionCreated, got {other:?}"),
    }
}

#[test]
fn new_session_with_git_linked_mode_succeeds_when_binding_matches() {
    let binding = GitRepoBindingId::new();
    let session = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::GitLinked {
            binding_id: binding,
        },
        Some(binding),
    )
    .expect("valid git-linked session");
    assert_eq!(session.git_binding_id, Some(binding));
    assert!(matches!(
        session.workspace_mode,
        WorkspaceMode::GitLinked { .. }
    ));
}

#[test]
fn new_session_with_git_linked_mode_fails_when_binding_mismatched() {
    let binding = GitRepoBindingId::new();
    let other = GitRepoBindingId::new();
    let err = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::GitLinked {
            binding_id: binding,
        },
        Some(other),
    );
    assert!(err.is_err(), "mismatched git_binding_id must be rejected");
}

#[test]
fn new_session_with_git_linked_mode_fails_when_binding_none() {
    let binding = GitRepoBindingId::new();
    let err = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::GitLinked {
            binding_id: binding,
        },
        None,
    );
    assert!(err.is_err(), "missing git_binding_id must be rejected");
}

#[test]
fn new_session_with_persistent_empty_label_is_rejected() {
    let err = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::Persistent {
            volume_label: String::new(),
        },
        None,
    );
    assert!(err.is_err(), "empty volume_label must be rejected");
}

#[test]
fn new_session_with_persistent_whitespace_label_is_rejected() {
    let err = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::Persistent {
            volume_label: "   ".to_string(),
        },
        None,
    );
    assert!(
        err.is_err(),
        "whitespace-only volume_label must be rejected"
    );
}

#[test]
fn new_session_rejects_git_binding_id_on_non_git_mode() {
    let err = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::Ephemeral,
        Some(GitRepoBindingId::new()),
    );
    assert!(
        err.is_err(),
        "Ephemeral session with git_binding_id must be rejected"
    );
}

// ============================================================================
// Status transitions
// ============================================================================

#[test]
fn initializing_to_ready_to_idle_to_terminated() {
    let mut session = ephemeral_session();
    let _ = session.take_events(); // drain SessionCreated

    session.mark_ready();
    assert_eq!(session.status, CanvasSessionStatus::Ready);

    session.mark_idle();
    assert_eq!(session.status, CanvasSessionStatus::Idle);

    session.terminate();
    assert_eq!(session.status, CanvasSessionStatus::Terminated);
    let events = session.take_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], CanvasEvent::SessionTerminated { .. }));
}

#[test]
fn mark_ready_and_mark_idle_update_last_active_at() {
    let mut session = ephemeral_session();
    let before = session.last_active_at;
    // Sleep 1ms so the timestamp-touch is observable even at microsecond clocks.
    std::thread::sleep(std::time::Duration::from_millis(2));
    session.mark_ready();
    assert!(session.last_active_at > before);

    let after_ready = session.last_active_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    session.mark_idle();
    assert!(session.last_active_at > after_ready);
}

// ============================================================================
// Audit markers: FilesWrittenByAgent / GitCommitMade / GitPushed
// ============================================================================

#[test]
fn mark_files_written_emits_event_with_count_and_touches_last_active() {
    let mut session = ephemeral_session();
    let _ = session.take_events();
    let before = session.last_active_at;
    std::thread::sleep(std::time::Duration::from_millis(2));

    session.mark_files_written(3);
    assert!(session.last_active_at > before);

    let events = session.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        CanvasEvent::FilesWrittenByAgent {
            session_id,
            file_count,
            ..
        } => {
            assert_eq!(*session_id, session.id);
            assert_eq!(*file_count, 3);
        }
        other => panic!("expected FilesWrittenByAgent, got {other:?}"),
    }
}

#[test]
fn mark_commit_emits_git_commit_made() {
    let binding = GitRepoBindingId::new();
    let mut session = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::GitLinked {
            binding_id: binding,
        },
        Some(binding),
    )
    .expect("valid git-linked session");
    let _ = session.take_events();

    session.mark_commit("deadbeef".to_string());
    let events = session.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        CanvasEvent::GitCommitMade {
            session_id,
            commit_sha,
            ..
        } => {
            assert_eq!(*session_id, session.id);
            assert_eq!(commit_sha, "deadbeef");
        }
        other => panic!("expected GitCommitMade, got {other:?}"),
    }
}

#[test]
fn mark_push_emits_git_pushed() {
    let binding = GitRepoBindingId::new();
    let mut session = CanvasSession::new(
        TenantId::consumer(),
        ConversationId::new(),
        VolumeId::new(),
        WorkspaceMode::GitLinked {
            binding_id: binding,
        },
        Some(binding),
    )
    .expect("valid git-linked session");
    let _ = session.take_events();

    session.mark_push("origin".to_string(), "refs/heads/main".to_string());
    let events = session.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        CanvasEvent::GitPushed {
            session_id,
            remote,
            ref_name,
            ..
        } => {
            assert_eq!(*session_id, session.id);
            assert_eq!(remote, "origin");
            assert_eq!(ref_name, "refs/heads/main");
        }
        other => panic!("expected GitPushed, got {other:?}"),
    }
}

// ============================================================================
// Event buffer semantics
// ============================================================================

#[test]
fn take_events_drains_buffer() {
    let mut session = ephemeral_session();
    let first = session.take_events();
    assert_eq!(first.len(), 1);
    let second = session.take_events();
    assert!(second.is_empty());
}
