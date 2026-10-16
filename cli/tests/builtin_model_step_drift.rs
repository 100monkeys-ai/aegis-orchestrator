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
const PUBLISHED_TDRZ: &str = "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-small.en-tdrz@sha256:bc770238d3b881d0ba9baa35ca9583241563c8677e566a0999b4bed4b6aba0c7";

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
    // ADR-143 S8b: the base step and the speaker-turn step, each on its
    // published image.
    let mut images: Vec<String> = stored
        .spec
        .states
        .values()
        .filter_map(|state| match &state.kind {
            StateKind::ContainerRun { image, .. } => Some(image.clone()),
            _ => None,
        })
        .collect();
    images.sort();
    assert_eq!(
        images,
        vec![PUBLISHED.to_string(), PUBLISHED_TDRZ.to_string()]
    );
}

/// The `TRANSCRIBE` state of a parsed `transcribe-audio`.
fn transcribe_state(workflow: &Workflow) -> &StateKind {
    &workflow
        .spec
        .states
        .iter()
        .find(|(name, _)| name.as_str() == "TRANSCRIBE")
        .expect("transcribe-audio has a TRANSCRIBE state")
        .1
        .kind
}

/// ADR-143 S4 and T4: `transcribe-audio` reads the file attached to its run,
/// through the run's `attachments` volume mounted read-only at `/input`; it
/// declares no shared volume, and its program writes nothing into the mount
/// (the transcript is the step's stdout).
#[test]
fn transcribe_audio_reads_the_runs_attachments_read_only() {
    let workflow = WorkflowParser::parse_yaml(&transcribe_audio()).expect("the template parses");
    assert!(
        workflow.spec.storage.shared_volumes.is_empty(),
        "transcribe-audio declares shared volumes: {:?}",
        workflow
            .spec
            .storage
            .shared_volumes
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>()
    );
    let StateKind::ContainerRun {
        command,
        volumes,
        workdir,
        ..
    } = transcribe_state(&workflow)
    else {
        panic!("TRANSCRIBE is not a ContainerRun");
    };
    let mounts: Vec<(&str, &str, bool)> = volumes
        .iter()
        .map(|m| (m.name.as_str(), m.mount_path.as_str(), m.read_only))
        .collect();
    assert_eq!(
        mounts,
        vec![("attachments", "/input", true)],
        "TRANSCRIBE does not mount only the run's attachments, read-only at /input"
    );
    assert_eq!(
        command,
        &vec![
            "transcribe".to_string(),
            "/input/{{input.audio_path}}".to_string(),
            "--out".to_string(),
            "/tmp/transcript.txt".to_string(),
        ],
        "TRANSCRIBE's command does not read /input and write outside it"
    );
    assert!(
        command.iter().skip(2).all(|arg| !arg.starts_with("/input")),
        "TRANSCRIBE writes into its read-only mount: {command:?}"
    );
    assert!(
        !workdir.as_deref().is_some_and(|w| w.starts_with("/input")),
        "TRANSCRIBE's working directory is inside its read-only mount: {workdir:?}"
    );
}

/// The startup deploy's case for this change: a `transcribe-audio` stored
/// from the template that read a declared `audio` volume reads as drift
/// against the template that reads the run's attachments, so the deploy
/// re-registers the built-in.
#[test]
fn a_stored_transcribe_audio_on_the_audio_volume_reads_as_drift() {
    let registry = registry();
    let template = transcribe_audio();
    let previous = template
        .replace(
            "  initial_state: TRANSCRIBE\n",
            "  initial_state: TRANSCRIBE\n  storage:\n    shared_volumes:\n      - name: audio\n        storage_class: persistent\n",
        )
        .replace(
            "        - name: attachments\n          mount_path: /input\n          read_only: true\n",
            "        - name: audio\n          mount_path: /data\n",
        );
    assert_ne!(
        previous, template,
        "the template reads the run's attachments"
    );
    let stored = stored(&previous, &registry);
    assert!(
        !workflow_matches(&stored, &template, Some(&registry)).unwrap(),
        "a transcribe-audio stored on the audio volume does not read as drift"
    );
}
