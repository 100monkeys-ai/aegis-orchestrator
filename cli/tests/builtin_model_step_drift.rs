// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A built-in workflow with a model step does not read as drift at every
//! daemon start (AEGIS known defect `model-step-builtin-drift`).
//!
//! Registration resolves a model step's `model:<class>/<name>` image through
//! the runtime registry before the workflow is stored, so the stored spec names
//! the pinned image, its no-network mode and the entry's resources. The startup
//! deploy compares that stored spec with the built-in template; the comparison
//! resolves the template through the same registry first, so the two sides are
//! the same function of the template and the registry.

use aegis_orchestrator::commands::builtins::{workflow_matches, BUILTIN_WORKFLOWS};
use aegis_orchestrator_core::domain::runtime_registry::StandardRuntimeRegistry;
use aegis_orchestrator_core::domain::workflow::{StateKind, Workflow};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use std::path::{Path, PathBuf};

fn templates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")
}

fn registry_yaml() -> String {
    std::fs::read_to_string(templates().join("stack/runtime-registry.yaml")).unwrap()
}

fn transcribe_audio() -> String {
    std::fs::read_to_string(templates().join("workflows/transcribe-audio.yaml")).unwrap()
}

/// The workflow as registration stores it: parsed, its model images resolved
/// through the registry, and read back from its stored JSON.
fn stored(template: &str, registry: &StandardRuntimeRegistry) -> Workflow {
    let mut workflow = WorkflowParser::parse_yaml(template).expect("the template parses");
    workflow
        .resolve_model_images(Some(registry))
        .expect("the template's model step resolves");
    serde_json::from_value(serde_json::to_value(&workflow).unwrap()).unwrap()
}

const PUBLISHED: &str = "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base@sha256:7ef552f84197b3c6a03efe059254e95517a05cf949e3454c04a9336a5090c88f";

fn registry() -> StandardRuntimeRegistry {
    StandardRuntimeRegistry::from_yaml_str(&registry_yaml()).expect("the stack registry loads")
}

/// The startup deploy's case: the stored `transcribe-audio` equals its
/// template once the template is resolved through the same registry.
#[test]
fn a_stored_model_step_builtin_matches_its_template() {
    let registry = registry();
    let template = transcribe_audio();
    let stored = stored(&template, &registry);
    assert!(
        workflow_matches(&stored, &template, Some(&registry)).unwrap(),
        "the stored transcribe-audio reads as drift against its own template"
    );
}

/// A changed template still reads as drift.
#[test]
fn a_changed_model_step_template_reads_as_drift() {
    let registry = registry();
    let template = transcribe_audio();
    let stored = stored(&template, &registry);
    let changed = template.replace("version: \"1.0.0\"", "version: \"1.0.1\"");
    assert_ne!(changed, template, "the template names version 1.0.0");
    assert!(
        !workflow_matches(&stored, &changed, Some(&registry)).unwrap(),
        "a changed template does not read as drift"
    );
}

/// A registry entry that moved to another digest reads as drift, so the
/// built-in is re-registered onto the new image.
#[test]
fn a_moved_model_digest_reads_as_drift() {
    let template = transcribe_audio();
    let stored = stored(&template, &registry());
    let moved = StandardRuntimeRegistry::from_yaml_str(&registry_yaml().replace(
        "7ef552f84197b3c6a03efe059254e95517a05cf949e3454c04a9336a5090c88f",
        "0000000000000000000000000000000000000000000000000000000000000000",
    ))
    .unwrap();
    assert!(
        !workflow_matches(&stored, &template, Some(&moved)).unwrap(),
        "a moved digest does not read as drift"
    );
}

/// ADR-133 D1: the startup deploy registers `transcribe-audio`, and its
/// stored step names the published image.
#[test]
fn transcribe_audio_is_a_builtin_workflow_on_the_published_image() {
    let (_, yaml) = BUILTIN_WORKFLOWS
        .iter()
        .find(|(name, _)| *name == "transcribe-audio")
        .expect("transcribe-audio is in BUILTIN_WORKFLOWS");
    assert_eq!(*yaml, transcribe_audio());
    let stored = stored(yaml, &registry());
    let images: Vec<String> = stored
        .spec
        .states
        .values()
        .filter_map(|state| match &state.kind {
            StateKind::ContainerRun { image, .. } => Some(image.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(images, vec![PUBLISHED.to_string()]);
}
