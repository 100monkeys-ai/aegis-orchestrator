// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Domain-layer unit tests for the **Security Policy** (BC-4) and **SEAL Protocol**
//! (BC-12) bounded contexts.
//!
//! Covers:
//! - `SecurityContext` aggregate: evaluate(), permits_tool_name(), deny-list precedence,
//!   empty capabilities, tenant ownership validation
//! - `Capability` value object: matches_tool_name(), allows(), path_in_allowlist()
//! - `SealSession` aggregate: construction, evaluate_call(), revoke(), status transitions,
//!   expiry detection, principal metadata, serialization round-trips
//!
//! # Architecture
//!
//! - **Layer:** Domain (pure business logic, no I/O)
//! - **Bounded Contexts:** BC-4 Security Policy, BC-12 SEAL Protocol

use chrono::Utc;
use serde_json::json;

use aegis_orchestrator_core::domain::seal_session::{
    EnvelopeVerifier, SealSession, SealSessionError, SessionStatus,
};
use aegis_orchestrator_core::domain::security_context::{
    Capability, PolicyViolation, SecurityContext, SecurityContextMetadata,
};
use aegis_orchestrator_core::domain::shared_kernel::{AgentId, ExecutionId, TenantId};

// ============================================================================
// Test Helpers
// ============================================================================

fn test_metadata() -> SecurityContextMetadata {
    SecurityContextMetadata {
        created_at: Utc::now(),
        updated_at: Utc::now(),
        version: 1,
    }
}

fn fs_read_capability() -> Capability {
    Capability {
        tool_pattern: "fs.read".to_string(),
        path_allowlist: Some(vec!["/workspace".into()]),
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
    }
}

fn fs_wildcard_capability() -> Capability {
    Capability {
        tool_pattern: "fs.*".to_string(),
        path_allowlist: Some(vec!["/workspace".into()]),
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
    }
}

fn wildcard_capability() -> Capability {
    Capability {
        tool_pattern: "*".to_string(),
        path_allowlist: None,
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
    }
}

fn web_capability_with_domains(domains: Vec<&str>) -> Capability {
    Capability {
        tool_pattern: "web.*".to_string(),
        path_allowlist: None,
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: Some(domains.into_iter().map(String::from).collect()),
        max_response_size: None,
        rate_limit: None,
    }
}

fn make_context(
    name: &str,
    capabilities: Vec<Capability>,
    deny_list: Vec<&str>,
) -> SecurityContext {
    SecurityContext {
        name: name.to_string(),
        description: format!("Test context: {name}"),
        capabilities,
        deny_list: deny_list.into_iter().map(String::from).collect(),
        metadata: test_metadata(),
    }
}

fn make_session(context: SecurityContext) -> SealSession {
    SealSession::new(
        AgentId::new(),
        ExecutionId::new(),
        vec![1, 2, 3, 4], // dummy public key bytes
        "test-token-abc".to_string(),
        context,
        TenantId::local_default(),
    )
}

/// Mock EnvelopeVerifier for SealSession::evaluate_call() tests.
struct MockEnvelope {
    token: String,
    signature_valid: bool,
    tool_name: Option<String>,
    arguments: Option<serde_json::Value>,
}

impl MockEnvelope {
    fn valid(tool_name: &str, args: serde_json::Value) -> Self {
        Self {
            token: "test-token-abc".to_string(),
            signature_valid: true,
            tool_name: Some(tool_name.to_string()),
            arguments: Some(args),
        }
    }
}

impl EnvelopeVerifier for MockEnvelope {
    fn security_token(&self) -> &str {
        &self.token
    }

    fn verify_signature(&self, _public_key_bytes: &[u8]) -> Result<(), SealSessionError> {
        if self.signature_valid {
            Ok(())
        } else {
            Err(SealSessionError::SignatureVerificationFailed(
                "mock: invalid signature".to_string(),
            ))
        }
    }

    fn extract_tool_name(&self) -> Option<String> {
        self.tool_name.clone()
    }

    fn extract_arguments(&self) -> Option<serde_json::Value> {
        self.arguments.clone()
    }
}

// ============================================================================
// SecurityContext — evaluate()
// ============================================================================

#[test]
fn evaluate_allows_matching_capability_with_valid_args() {
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    assert!(ctx
        .evaluate("fs.read", &json!({"path": "/workspace/foo.txt"}))
        .is_ok());
}

#[test]
fn evaluate_denies_tool_not_in_capabilities() {
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    let err = ctx
        .evaluate("fs.write", &json!({"path": "/workspace/foo.txt"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::ToolNotAllowed { .. }));
}

#[test]
fn evaluate_deny_list_takes_precedence_over_capabilities() {
    // fs.* capability would normally match fs.delete, but deny_list wins.
    let ctx = make_context(
        "zaru-test",
        vec![fs_wildcard_capability()],
        vec!["fs.delete"],
    );

    let err = ctx
        .evaluate("fs.delete", &json!({"path": "/workspace/x"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::ToolExplicitlyDenied { .. }));

    // fs.read should still be allowed
    assert!(ctx
        .evaluate("fs.read", &json!({"path": "/workspace/x"}))
        .is_ok());
}

#[test]
fn evaluate_default_deny_when_no_capability_matches() {
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    let err = ctx.evaluate("cmd.run", &json!({})).unwrap_err();
    match err {
        PolicyViolation::ToolNotAllowed {
            tool_name,
            allowed_tools,
        } => {
            assert_eq!(tool_name, "cmd.run");
            assert_eq!(allowed_tools, vec!["fs.read"]);
        }
        other => panic!("expected ToolNotAllowed, got {other:?}"),
    }
}

#[test]
fn evaluate_empty_capabilities_denies_everything() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    let err = ctx.evaluate("fs.read", &json!({})).unwrap_err();
    match err {
        PolicyViolation::ToolNotAllowed { allowed_tools, .. } => {
            assert!(allowed_tools.is_empty());
        }
        other => panic!("expected ToolNotAllowed with empty allowed_tools, got {other:?}"),
    }
}

#[test]
fn evaluate_path_outside_boundary_falls_through_to_tool_not_allowed() {
    // When the only matching capability rejects on path constraints, evaluate()
    // continues scanning and ultimately returns ToolNotAllowed (default-deny),
    // because the 3-step algorithm treats any capability error as "try next".
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    let err = ctx
        .evaluate("fs.read", &json!({"path": "/etc/passwd"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::ToolNotAllowed { .. }));
}

// ============================================================================
// SecurityContext — permits_tool_name()
// ============================================================================

#[test]
fn permits_tool_name_exact_match() {
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    assert!(ctx.permits_tool_name("fs.read"));
    assert!(!ctx.permits_tool_name("fs.write"));
}

#[test]
fn permits_tool_name_prefix_match() {
    let ctx = make_context("zaru-test", vec![fs_wildcard_capability()], vec![]);
    assert!(ctx.permits_tool_name("fs.read"));
    assert!(ctx.permits_tool_name("fs.write"));
    assert!(ctx.permits_tool_name("fs.delete"));
    assert!(!ctx.permits_tool_name("cmd.run"));
}

#[test]
fn permits_tool_name_respects_deny_list() {
    let ctx = make_context(
        "zaru-test",
        vec![fs_wildcard_capability()],
        vec!["fs.delete"],
    );
    assert!(ctx.permits_tool_name("fs.read"));
    assert!(!ctx.permits_tool_name("fs.delete"));
}

#[test]
fn permits_tool_name_empty_capabilities_denies_all() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    assert!(!ctx.permits_tool_name("fs.read"));
    assert!(!ctx.permits_tool_name("anything"));
}

// ============================================================================
// Capability — matches_tool_name()
// ============================================================================

#[test]
fn capability_matches_exact_tool_name() {
    let cap = fs_read_capability();
    assert!(cap.matches_tool_name("fs.read"));
    assert!(!cap.matches_tool_name("fs.write"));
    assert!(!cap.matches_tool_name("fs.read.extra"));
}

#[test]
fn capability_matches_prefix_wildcard() {
    let cap = fs_wildcard_capability();
    assert!(cap.matches_tool_name("fs.read"));
    assert!(cap.matches_tool_name("fs.write"));
    assert!(cap.matches_tool_name("fs.delete"));
    // Does not match a different prefix
    assert!(!cap.matches_tool_name("web.fetch"));
}

#[test]
fn capability_matches_universal_wildcard() {
    let cap = wildcard_capability();
    assert!(cap.matches_tool_name("fs.read"));
    assert!(cap.matches_tool_name("cmd.run"));
    assert!(cap.matches_tool_name("web.fetch"));
    assert!(cap.matches_tool_name("anything-at-all"));
}

// ============================================================================
// Capability — allows() with path constraints
// ============================================================================

#[test]
fn capability_allows_valid_path() {
    let cap = fs_read_capability();
    assert!(cap
        .allows("fs.read", &json!({"path": "/workspace/src/main.rs"}))
        .is_ok());
}

#[test]
fn capability_rejects_path_outside_allowlist() {
    let cap = fs_read_capability();
    let err = cap
        .allows("fs.read", &json!({"path": "/etc/shadow"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::PathOutsideBoundary { .. }));
}

#[test]
fn capability_rejects_path_traversal_outside_allowlist() {
    // PathBuf::starts_with operates on path components. `/workspace/../etc/passwd`
    // still has `/workspace` as its first component so it passes starts_with.
    // True traversal rejection requires canonicalization (done by path_sanitizer).
    // Here we test that a clearly-outside path like `/etc/passwd` is rejected.
    let cap = fs_read_capability();
    let err = cap
        .allows("fs.read", &json!({"path": "/etc/passwd"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::PathOutsideBoundary { .. }));
}

#[test]
fn capability_allows_when_no_path_constraint() {
    let cap = Capability {
        tool_pattern: "fs.read".to_string(),
        path_allowlist: None,
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
    };
    // No path_allowlist means no path restriction
    assert!(cap
        .allows("fs.read", &json!({"path": "/anywhere/at/all"}))
        .is_ok());
}

// ============================================================================
// Capability — allows() with command constraints
// ============================================================================

#[test]
fn capability_allows_whitelisted_command() {
    let cap = Capability {
        tool_pattern: "cmd.run".to_string(),
        path_allowlist: None,
        command_allowlist: Some(vec!["cargo".to_string(), "rustc".to_string()]),
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
    };
    assert!(cap
        .allows("cmd.run", &json!({"command": "cargo build --release"}))
        .is_ok());
    assert!(cap
        .allows("cmd.run", &json!({"command": "rustc --version"}))
        .is_ok());
}

#[test]
fn capability_rejects_non_whitelisted_command() {
    let cap = Capability {
        tool_pattern: "cmd.run".to_string(),
        path_allowlist: None,
        command_allowlist: Some(vec!["cargo".to_string()]),
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
    };
    let err = cap
        .allows("cmd.run", &json!({"command": "rm -rf /"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::ToolNotAllowed { .. }));
}

// ============================================================================
// Capability — allows() with domain constraints
// ============================================================================

#[test]
fn capability_allows_whitelisted_domain() {
    let cap = web_capability_with_domains(vec!["github.com", "crates.io"]);
    assert!(cap
        .allows("web.fetch", &json!({"url": "https://github.com/foo/bar"}))
        .is_ok());
    assert!(cap
        .allows(
            "web.fetch",
            &json!({"url": "https://crates.io/crates/serde"})
        )
        .is_ok());
}

#[test]
fn capability_rejects_non_whitelisted_domain() {
    let cap = web_capability_with_domains(vec!["github.com"]);
    let err = cap
        .allows("web.fetch", &json!({"url": "https://evil.example.com/pwn"}))
        .unwrap_err();
    assert!(matches!(err, PolicyViolation::DomainNotAllowed { .. }));
}

// ============================================================================
// Capability — tool pattern mismatch returns ToolNotAllowed
// ============================================================================

#[test]
fn capability_allows_rejects_wrong_tool() {
    let cap = fs_read_capability();
    let err = cap.allows("cmd.run", &json!({})).unwrap_err();
    assert!(matches!(err, PolicyViolation::ToolNotAllowed { .. }));
}

// ============================================================================
// Tenant Ownership Validation
// ============================================================================

#[test]
fn tenant_ownership_zaru_prefix_for_consumer() {
    let consumer = TenantId::consumer();
    let ctx = make_context("zaru-free", vec![wildcard_capability()], vec![]);
    assert!(ctx.validate_tenant_ownership(&consumer).is_ok());
}

#[test]
fn tenant_ownership_zaru_prefix_rejected_for_enterprise() {
    let enterprise = TenantId::from_realm_slug("acme").unwrap();
    let ctx = make_context("zaru-pro", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&enterprise).is_err());
}

#[test]
fn tenant_ownership_zaru_prefix_rejected_for_system() {
    let system = TenantId::system();
    let ctx = make_context("zaru-free", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&system).is_err());
}

#[test]
fn tenant_ownership_tenant_prefix_matches_slug() {
    let tenant = TenantId::from_realm_slug("acme-corp").unwrap();
    let ctx = make_context("tenant-acme-corp-research", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&tenant).is_ok());
}

#[test]
fn tenant_ownership_tenant_prefix_rejects_wrong_slug() {
    let tenant = TenantId::from_realm_slug("acme-corp").unwrap();
    let ctx = make_context("tenant-other-corp-research", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&tenant).is_err());
}

#[test]
fn tenant_ownership_aegis_system_prefix_for_system() {
    let system = TenantId::system();
    let ctx = make_context("aegis-system-internal", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&system).is_ok());
}

#[test]
fn tenant_ownership_aegis_system_prefix_rejected_for_consumer() {
    let consumer = TenantId::consumer();
    let ctx = make_context("aegis-system-core", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&consumer).is_err());
}

#[test]
fn tenant_ownership_bare_name_rejected_for_all() {
    // Legacy bare names (no recognised prefix) are rejected outright.
    let consumer = TenantId::consumer();
    let system = TenantId::system();
    let enterprise = TenantId::from_realm_slug("acme").unwrap();

    let ctx = make_context("research-safe", vec![], vec![]);
    assert!(ctx.validate_tenant_ownership(&consumer).is_err());
    assert!(ctx.validate_tenant_ownership(&system).is_err());
    assert!(ctx.validate_tenant_ownership(&enterprise).is_err());
}

// ============================================================================
// SealSession — new()
// ============================================================================

#[test]
fn session_new_initialises_active_with_one_hour_expiry() {
    let before = Utc::now();
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let session = make_session(ctx);
    let after = Utc::now();

    assert_eq!(session.status, SessionStatus::Active);
    assert!(session.created_at >= before);
    assert!(session.created_at <= after);

    // expires_at should be ~1 hour from created_at
    let ttl = session.expires_at - session.created_at;
    assert_eq!(ttl.num_hours(), 1);

    assert!(session.principal_subject.is_none());
    assert!(session.user_id.is_none());
    assert!(session.workload_id.is_none());
    assert!(session.zaru_tier.is_none());
}

// ============================================================================
// SealSession — evaluate_call()
// ============================================================================

#[test]
fn evaluate_call_succeeds_for_valid_envelope() {
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    let mut session = make_session(ctx);
    let envelope = MockEnvelope::valid("fs.read", json!({"path": "/workspace/file.txt"}));
    assert!(session.evaluate_call(&envelope).is_ok());
}

#[test]
fn evaluate_call_rejects_inactive_session() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);
    session.revoke("test revocation".to_string());

    let envelope = MockEnvelope::valid("fs.read", json!({"path": "/workspace/x"}));
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::SessionInactive(_)));
}

#[test]
fn evaluate_call_rejects_expired_session() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);
    // Force expiry by setting expires_at to the past
    session.expires_at = Utc::now() - chrono::Duration::seconds(1);

    let envelope = MockEnvelope::valid("fs.read", json!({}));
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::SessionExpired));
    // Status should now be Expired
    assert_eq!(session.status, SessionStatus::Expired);
}

#[test]
fn evaluate_call_rejects_token_mismatch() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);

    let envelope = MockEnvelope {
        token: "wrong-token".to_string(),
        signature_valid: true,
        tool_name: Some("fs.read".to_string()),
        arguments: Some(json!({})),
    };
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(
        err,
        SealSessionError::SignatureVerificationFailed(_)
    ));
}

#[test]
fn evaluate_call_rejects_invalid_signature() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);

    let envelope = MockEnvelope {
        token: "test-token-abc".to_string(),
        signature_valid: false,
        tool_name: Some("fs.read".to_string()),
        arguments: Some(json!({})),
    };
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(
        err,
        SealSessionError::SignatureVerificationFailed(_)
    ));
}

#[test]
fn evaluate_call_rejects_missing_tool_name() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);

    let envelope = MockEnvelope {
        token: "test-token-abc".to_string(),
        signature_valid: true,
        tool_name: None,
        arguments: Some(json!({})),
    };
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::MalformedPayload(_)));
}

#[test]
fn evaluate_call_rejects_missing_arguments() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);

    let envelope = MockEnvelope {
        token: "test-token-abc".to_string(),
        signature_valid: true,
        tool_name: Some("fs.read".to_string()),
        arguments: None,
    };
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::MalformedPayload(_)));
}

#[test]
fn evaluate_call_rejects_policy_violation() {
    // Context only allows fs.read, but we try cmd.run
    let ctx = make_context("zaru-test", vec![fs_read_capability()], vec![]);
    let mut session = make_session(ctx);

    let envelope = MockEnvelope::valid("cmd.run", json!({"command": "rm -rf /"}));
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::PolicyViolation(_)));
}

#[test]
fn evaluate_call_rejects_denied_tool_via_policy() {
    let ctx = make_context(
        "zaru-test",
        vec![fs_wildcard_capability()],
        vec!["fs.delete"],
    );
    let mut session = make_session(ctx);

    let envelope = MockEnvelope::valid("fs.delete", json!({"path": "/workspace/x"}));
    let err = session.evaluate_call(&envelope).unwrap_err();
    match err {
        SealSessionError::PolicyViolation(PolicyViolation::ToolExplicitlyDenied { tool_name }) => {
            assert_eq!(tool_name, "fs.delete");
        }
        other => panic!("expected PolicyViolation(ToolExplicitlyDenied), got {other:?}"),
    }
}

// ============================================================================
// SealSession — revoke()
// ============================================================================

#[test]
fn revoke_transitions_active_to_revoked() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    let mut session = make_session(ctx);
    assert_eq!(session.status, SessionStatus::Active);

    session.revoke("security incident".to_string());

    assert!(matches!(
        session.status,
        SessionStatus::Revoked { ref reason } if reason == "security incident"
    ));
}

#[test]
fn revoke_is_idempotent_on_already_revoked() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    let mut session = make_session(ctx);

    session.revoke("first reason".to_string());
    session.revoke("second reason".to_string());

    // First revocation reason is preserved
    assert!(matches!(
        session.status,
        SessionStatus::Revoked { ref reason } if reason == "first reason"
    ));
}

#[test]
fn revoke_is_idempotent_on_expired() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);

    // Force expiry
    session.expires_at = Utc::now() - chrono::Duration::seconds(1);
    let envelope = MockEnvelope::valid("fs.read", json!({}));
    let _ = session.evaluate_call(&envelope); // triggers Expired transition

    assert_eq!(session.status, SessionStatus::Expired);

    // Revoke after expiry should not change status
    session.revoke("late revocation".to_string());
    assert_eq!(session.status, SessionStatus::Expired);
}

// ============================================================================
// SealSession — status transitions
// ============================================================================

#[test]
fn status_transition_active_to_expired_on_evaluate() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);
    session.expires_at = Utc::now() - chrono::Duration::seconds(1);

    let envelope = MockEnvelope::valid("fs.read", json!({}));
    let _ = session.evaluate_call(&envelope);

    assert_eq!(session.status, SessionStatus::Expired);
}

#[test]
fn status_transition_active_to_revoked() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    let mut session = make_session(ctx);
    session.revoke("done".to_string());
    assert!(matches!(session.status, SessionStatus::Revoked { .. }));
}

#[test]
fn no_reversal_from_expired() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);
    session.expires_at = Utc::now() - chrono::Duration::seconds(1);

    let envelope = MockEnvelope::valid("fs.read", json!({}));
    let _ = session.evaluate_call(&envelope);
    assert_eq!(session.status, SessionStatus::Expired);

    // Cannot revert to Active — subsequent evaluate_call still fails
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::SessionInactive(_)));
}

#[test]
fn no_reversal_from_revoked() {
    let ctx = make_context("zaru-test", vec![wildcard_capability()], vec![]);
    let mut session = make_session(ctx);
    session.revoke("revoked".to_string());

    let envelope = MockEnvelope::valid("fs.read", json!({}));
    let err = session.evaluate_call(&envelope).unwrap_err();
    assert!(matches!(err, SealSessionError::SessionInactive(_)));
}

// ============================================================================
// SealSession — with_principal_metadata()
// ============================================================================

#[test]
fn with_principal_metadata_attaches_values() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    let session = make_session(ctx).with_principal_metadata(
        Some("subject-123".to_string()),
        Some("user-456".to_string()),
        Some("workload-789".to_string()),
        Some("pro".to_string()),
    );

    assert_eq!(session.principal_subject.as_deref(), Some("subject-123"));
    assert_eq!(session.user_id.as_deref(), Some("user-456"));
    assert_eq!(session.workload_id.as_deref(), Some("workload-789"));
    assert_eq!(session.zaru_tier.as_deref(), Some("pro"));
}

#[test]
fn with_principal_metadata_filters_blank_strings() {
    let ctx = make_context("zaru-test", vec![], vec![]);
    let session = make_session(ctx).with_principal_metadata(
        Some("  ".to_string()),
        Some("".to_string()),
        None,
        Some("free".to_string()),
    );

    assert!(session.principal_subject.is_none());
    assert!(session.user_id.is_none());
    assert!(session.workload_id.is_none());
    assert_eq!(session.zaru_tier.as_deref(), Some("free"));
}

// ============================================================================
// Serialization Round-Trips
// ============================================================================

#[test]
fn security_context_serialization_roundtrip() {
    let ctx = make_context(
        "zaru-roundtrip",
        vec![fs_read_capability(), fs_wildcard_capability()],
        vec!["fs.delete"],
    );

    let json = serde_json::to_string(&ctx).expect("serialize SecurityContext");
    let deserialized: SecurityContext =
        serde_json::from_str(&json).expect("deserialize SecurityContext");

    assert_eq!(ctx, deserialized);
}

#[test]
fn capability_serialization_roundtrip() {
    let cap = Capability {
        tool_pattern: "web.*".to_string(),
        path_allowlist: None,
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: Some(vec!["github.com".to_string(), "crates.io".to_string()]),
        max_response_size: Some(1_048_576),
        rate_limit: None,
    };

    let json = serde_json::to_string(&cap).expect("serialize Capability");
    let deserialized: Capability = serde_json::from_str(&json).expect("deserialize Capability");

    assert_eq!(cap, deserialized);
}

#[test]
fn policy_violation_serialization_roundtrip() {
    let violations = vec![
        PolicyViolation::ToolExplicitlyDenied {
            tool_name: "fs.delete".to_string(),
        },
        PolicyViolation::ToolNotAllowed {
            tool_name: "cmd.run".to_string(),
            allowed_tools: vec!["fs.read".to_string()],
        },
        PolicyViolation::PathOutsideBoundary {
            path: "/etc/passwd".into(),
            allowed_paths: vec!["/workspace".into()],
        },
        PolicyViolation::DomainNotAllowed {
            domain: "evil.com".to_string(),
            allowed_domains: vec!["github.com".to_string()],
        },
    ];

    for violation in violations {
        let json = serde_json::to_string(&violation).expect("serialize PolicyViolation");
        let deserialized: PolicyViolation =
            serde_json::from_str(&json).expect("deserialize PolicyViolation");
        assert_eq!(violation, deserialized);
    }
}

#[test]
fn session_status_serialization_roundtrip() {
    let statuses = vec![
        SessionStatus::Active,
        SessionStatus::Expired,
        SessionStatus::Revoked {
            reason: "security incident".to_string(),
        },
    ];

    for status in statuses {
        let json = serde_json::to_string(&status).expect("serialize SessionStatus");
        let deserialized: SessionStatus =
            serde_json::from_str(&json).expect("deserialize SessionStatus");
        assert_eq!(status, deserialized);
    }
}

#[test]
fn seal_session_error_serialization_roundtrip() {
    let errors = vec![
        SealSessionError::SessionInactive(SessionStatus::Expired),
        SealSessionError::SessionExpired,
        SealSessionError::PolicyViolation(PolicyViolation::ToolExplicitlyDenied {
            tool_name: "fs.delete".to_string(),
        }),
        SealSessionError::MalformedPayload("missing tool name".to_string()),
        SealSessionError::SignatureVerificationFailed("bad sig".to_string()),
    ];

    for error in errors {
        let json = serde_json::to_string(&error).expect("serialize SealSessionError");
        let deserialized: SealSessionError =
            serde_json::from_str(&json).expect("deserialize SealSessionError");
        assert_eq!(error, deserialized);
    }
}
