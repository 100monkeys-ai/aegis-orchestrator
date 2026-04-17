// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Script Domain Aggregate Tests (BC-7, ADR-110 §D7)
//!
//! Unit tests for the [`Script`] aggregate root covering:
//!
//! - Constructor semantics (version 1, private, emits `Created`)
//! - [`Script::update`] bumps the version and preserves immutable fields
//! - [`Script::soft_delete`] sets `deleted_at` and emits `Deleted`
//!   exactly once (idempotent)
//! - Validation guards: name, description, code, tags, visibility
//! - [`ScriptTierLimits::for_tier`] resolution per [`ZaruTier`]

use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::script::{
    validate_code, validate_description, validate_name, validate_tags, Script, ScriptError,
    ScriptEvent, Visibility, SCRIPT_CODE_MAX_BYTES, SCRIPT_DESCRIPTION_MAX_BYTES,
    SCRIPT_NAME_MAX_BYTES, SCRIPT_TAG_MAX_BYTES,
};
use aegis_orchestrator_core::domain::script_tier_limits::ScriptTierLimits;
use aegis_orchestrator_core::domain::shared_kernel::TenantId;

// ============================================================================
// Helpers
// ============================================================================

fn make_script() -> Script {
    Script::new(
        TenantId::consumer(),
        "user-1".to_string(),
        "demo".to_string(),
        "a demo script".to_string(),
        "return 1 + 2;".to_string(),
        vec!["alpha".to_string(), "beta".to_string()],
    )
    .expect("valid sample")
}

// ============================================================================
// Constructor + event emission
// ============================================================================

#[test]
fn new_script_initialises_to_version_one_private_with_created_event() {
    let mut s = make_script();
    assert_eq!(s.version, 1);
    assert_eq!(s.visibility, Visibility::Private);
    assert!(s.deleted_at.is_none());

    let events = s.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ScriptEvent::Created {
            id,
            name,
            version,
            tenant_id,
            ..
        } => {
            assert_eq!(*id, s.id);
            assert_eq!(name, "demo");
            assert_eq!(*version, 1);
            assert_eq!(*tenant_id, TenantId::consumer());
        }
        other => panic!("expected Created, got {other:?}"),
    }
}

#[test]
fn take_events_drains_buffer() {
    let mut s = make_script();
    let first = s.take_events();
    assert_eq!(first.len(), 1);
    assert!(s.take_events().is_empty());
}

// ============================================================================
// Update
// ============================================================================

#[test]
fn update_bumps_version_and_emits_event() {
    let mut s = make_script();
    let original_created_by = s.created_by.clone();
    let original_created_at = s.created_at;
    let original_id = s.id;
    let _ = s.take_events();

    // Force a measurable time gap so `updated_at` moves.
    std::thread::sleep(std::time::Duration::from_millis(5));

    s.update(
        "demo-v2".to_string(),
        "updated".to_string(),
        "return 42;".to_string(),
        vec!["gamma".to_string()],
    )
    .unwrap();

    assert_eq!(s.version, 2);
    assert_eq!(s.name, "demo-v2");
    assert_eq!(s.description, "updated");
    assert_eq!(s.code, "return 42;");
    assert_eq!(s.tags, vec!["gamma".to_string()]);

    // Immutable fields preserved.
    assert_eq!(s.id, original_id);
    assert_eq!(s.created_by, original_created_by);
    assert_eq!(s.created_at, original_created_at);
    // updated_at advances.
    assert!(s.updated_at > original_created_at);

    let events = s.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ScriptEvent::Updated {
            id,
            name,
            new_version,
            ..
        } => {
            assert_eq!(*id, s.id);
            assert_eq!(name, "demo-v2");
            assert_eq!(*new_version, 2);
        }
        other => panic!("expected Updated, got {other:?}"),
    }
}

#[test]
fn update_propagates_validation_error() {
    let mut s = make_script();
    let _ = s.take_events();
    let err = s.update("".to_string(), "".to_string(), "".to_string(), vec![]);
    assert_eq!(err, Err(ScriptError::EmptyName));
    // Version must NOT have bumped on a failed update.
    assert_eq!(s.version, 1);
}

// ============================================================================
// Soft-delete
// ============================================================================

#[test]
fn soft_delete_sets_deleted_at_and_emits_event() {
    let mut s = make_script();
    let _ = s.take_events();
    s.soft_delete();
    assert!(s.deleted_at.is_some());
    assert!(s.is_deleted());

    let events = s.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ScriptEvent::Deleted { id, .. } => assert_eq!(*id, s.id),
        other => panic!("expected Deleted, got {other:?}"),
    }
}

#[test]
fn soft_delete_is_idempotent() {
    let mut s = make_script();
    let _ = s.take_events();
    s.soft_delete();
    let first = s.take_events();
    assert_eq!(first.len(), 1);
    s.soft_delete();
    let second = s.take_events();
    assert!(second.is_empty(), "second soft_delete must be a no-op");
}

// ============================================================================
// Validation: name
// ============================================================================

#[test]
fn validate_name_rejects_empty() {
    assert_eq!(validate_name(""), Err(ScriptError::EmptyName));
}

#[test]
fn validate_name_rejects_over_max_bytes() {
    let too_long = "a".repeat(SCRIPT_NAME_MAX_BYTES + 1);
    assert_eq!(validate_name(&too_long), Err(ScriptError::NameTooLong));
}

#[test]
fn validate_name_accepts_max_bytes() {
    let exactly = "a".repeat(SCRIPT_NAME_MAX_BYTES);
    assert!(validate_name(&exactly).is_ok());
}

#[test]
fn validate_name_rejects_path_separators() {
    assert_eq!(
        validate_name("path/to/file"),
        Err(ScriptError::NameContainsPathSeparator)
    );
    assert_eq!(
        validate_name("path\\to\\file"),
        Err(ScriptError::NameContainsPathSeparator)
    );
}

// ============================================================================
// Validation: description
// ============================================================================

#[test]
fn validate_description_rejects_over_max_bytes() {
    let too_long = "d".repeat(SCRIPT_DESCRIPTION_MAX_BYTES + 1);
    assert_eq!(
        validate_description(&too_long),
        Err(ScriptError::DescriptionTooLong)
    );
}

#[test]
fn validate_description_accepts_empty() {
    assert!(validate_description("").is_ok());
}

// ============================================================================
// Validation: code
// ============================================================================

#[test]
fn validate_code_rejects_over_max_bytes() {
    let too_large = "x".repeat(SCRIPT_CODE_MAX_BYTES + 1);
    assert_eq!(validate_code(&too_large), Err(ScriptError::CodeTooLarge));
}

#[test]
fn validate_code_accepts_max_bytes() {
    let exactly = "x".repeat(SCRIPT_CODE_MAX_BYTES);
    assert!(validate_code(&exactly).is_ok());
}

// ============================================================================
// Validation: tags
// ============================================================================

#[test]
fn validate_tags_rejects_over_max_count() {
    let too_many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    assert_eq!(validate_tags(&too_many), Err(ScriptError::TooManyTags));
}

#[test]
fn validate_tags_rejects_empty_tag() {
    let with_empty = vec!["ok".to_string(), "".to_string()];
    assert!(matches!(
        validate_tags(&with_empty),
        Err(ScriptError::TagInvalid(_))
    ));
}

#[test]
fn validate_tags_rejects_over_long_tag() {
    let long_tag = "a".repeat(SCRIPT_TAG_MAX_BYTES + 1);
    let tags = vec![long_tag];
    assert!(matches!(
        validate_tags(&tags),
        Err(ScriptError::TagInvalid(_))
    ));
}

#[test]
fn validate_tags_rejects_uppercase() {
    let tags = vec!["UPPER".to_string()];
    assert!(matches!(
        validate_tags(&tags),
        Err(ScriptError::TagInvalid(_))
    ));
}

#[test]
fn validate_tags_rejects_spaces() {
    let tags = vec!["hello world".to_string()];
    assert!(matches!(
        validate_tags(&tags),
        Err(ScriptError::TagInvalid(_))
    ));
}

#[test]
fn validate_tags_accepts_allowed_chars() {
    let tags = vec!["alpha-1".to_string(), "beta_2".to_string(), "x".to_string()];
    assert!(validate_tags(&tags).is_ok());
}

// ============================================================================
// Constructor validation cascade
// ============================================================================

#[test]
fn new_rejects_empty_name() {
    let err = Script::new(
        TenantId::consumer(),
        "u".to_string(),
        "".to_string(),
        "".to_string(),
        "x".to_string(),
        vec![],
    );
    assert_eq!(err.err(), Some(ScriptError::EmptyName));
}

#[test]
fn new_rejects_oversized_code() {
    let big = "x".repeat(SCRIPT_CODE_MAX_BYTES + 1);
    let err = Script::new(
        TenantId::consumer(),
        "u".to_string(),
        "ok".to_string(),
        "".to_string(),
        big,
        vec![],
    );
    assert_eq!(err.err(), Some(ScriptError::CodeTooLarge));
}

#[test]
fn new_rejects_too_many_tags() {
    let tags: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    let err = Script::new(
        TenantId::consumer(),
        "u".to_string(),
        "ok".to_string(),
        "".to_string(),
        "x".to_string(),
        tags,
    );
    assert_eq!(err.err(), Some(ScriptError::TooManyTags));
}

// ============================================================================
// Visibility
// ============================================================================

#[test]
fn visibility_defaults_to_private() {
    assert_eq!(Visibility::default(), Visibility::Private);
}

#[test]
fn visibility_text_roundtrip() {
    for v in [Visibility::Private, Visibility::Tenant, Visibility::Public] {
        assert_eq!(Visibility::from_str_ci(v.as_str()), Some(v));
    }
    assert_eq!(Visibility::from_str_ci("unknown"), None);
}

// ============================================================================
// Tier limits
// ============================================================================

#[test]
fn tier_limits_free() {
    let limits = ScriptTierLimits::for_tier(ZaruTier::Free);
    assert_eq!(limits.max_scripts, Some(5));
    assert_eq!(limits.max_code_bytes, SCRIPT_CODE_MAX_BYTES as u32);
}

#[test]
fn tier_limits_pro() {
    let limits = ScriptTierLimits::for_tier(ZaruTier::Pro);
    assert_eq!(limits.max_scripts, Some(50));
}

#[test]
fn tier_limits_business() {
    let limits = ScriptTierLimits::for_tier(ZaruTier::Business);
    assert_eq!(limits.max_scripts, Some(500));
}

#[test]
fn tier_limits_enterprise_unlimited() {
    let limits = ScriptTierLimits::for_tier(ZaruTier::Enterprise);
    assert_eq!(limits.max_scripts, None);
}
