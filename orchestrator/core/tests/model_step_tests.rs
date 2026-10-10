// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Model steps: a `ContainerRun` whose `image` is `model:<class>/<name>`,
//! resolved through the runtime registry's `spec.models` (ADR-143 M3 to M6,
//! S1, S2, G1).
//!
//! - The registry refuses a model image named without a digest when it loads,
//!   and `resolve_model` answers a `model:` reference or refuses an unknown one.
//! - The parser checks a `model:` reference's form and gives a model step no
//!   network: `network_mode` absent reads `none`, any other value is refused.
//! - Resolution replaces the reference with the registry's pinned image.
//! - The built-in `transcribe-audio` parses and resolves; the whisper image's
//!   tag in the stack template is the one its Dockerfile's versions give.

use aegis_orchestrator_core::domain::runtime_registry::StandardRuntimeRegistry;
use aegis_orchestrator_core::domain::workflow::{StateKind, StateName, Workflow};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DIGEST: &str = "sha256:5f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn registry_yaml(models: &str) -> String {
    format!(
        r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    python:
      "3.11":
        image: "python:3.11-slim"
  models:
{models}
"#
    )
}

fn whisper_registry() -> StandardRuntimeRegistry {
    let models = format!(
        r#"    speech-to-text:
      whisper-base:
        image: "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base@{DIGEST}"
        description: "Speech to text"
        resources:
          cpu: 2000
          memory: "1Gi"
          timeout: "10m""#
    );
    StandardRuntimeRegistry::from_yaml_str(&registry_yaml(&models))
        .expect("a registry whose model is pinned by digest loads")
}

fn model_workflow(network_line: &str) -> String {
    format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: model-step-test
  version: "1.0.0"
spec:
  initial_state: TRANSCRIBE
  states:
    TRANSCRIBE:
      kind: ContainerRun
      name: "Transcribe"
      image: "model:speech-to-text/whisper-base"
      command: ["transcribe", "/input/a.wav", "--out", "/output/transcript.txt"]
{network_line}
      transitions:
        - condition: exit_code_zero
          target: DONE
        - condition: exit_code_non_zero
          target: FAILED
    DONE:
      kind: System
      command: "echo done"
      transitions: []
    FAILED:
      kind: System
      command: "echo failed"
      transitions: []
"#
    )
}

fn container_state(workflow: &Workflow, state: &str) -> StateKind {
    workflow.spec.states[&StateName::new(state).unwrap()]
        .kind
        .clone()
}

/// M4: a model image without an `@sha256:` digest is refused at load.
#[test]
fn a_model_image_without_a_digest_is_refused_at_load() {
    let models = r#"    speech-to-text:
      whisper-base:
        image: "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base""#;
    let result = StandardRuntimeRegistry::from_yaml_str(&registry_yaml(models));
    assert_eq!(
        result.map(|_| ()).map_err(|e| e.to_string()),
        Err("model 'speech-to-text/whisper-base' must name its image by digest".to_string())
    );
}

/// M4: a digest that is not 64 hexadecimal characters is no digest.
#[test]
fn a_model_image_with_a_malformed_digest_is_refused_at_load() {
    let models = r#"    speech-to-text:
      whisper-base:
        image: "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base@sha256:digest-filled-later""#;
    let result = StandardRuntimeRegistry::from_yaml_str(&registry_yaml(models));
    assert_eq!(
        result.map(|_| ()).map_err(|e| e.to_string()),
        Err("model 'speech-to-text/whisper-base' must name its image by digest".to_string())
    );
}

/// M4: a reference with a digest and no tag is refused: the image is named by
/// a tag and a digest.
#[test]
fn a_model_image_with_a_digest_and_no_tag_is_refused_at_load() {
    let models = format!(
        r#"    speech-to-text:
      whisper-base:
        image: "ghcr.io/100monkeys-ai/aegis-model-whisper@{DIGEST}""#
    );
    let result = StandardRuntimeRegistry::from_yaml_str(&registry_yaml(&models));
    assert_eq!(
        result.map(|_| ()).map_err(|e| e.to_string()),
        Err("model 'speech-to-text/whisper-base' must name its image by digest".to_string())
    );
}

/// M4: `resolve_model` answers a `model:` reference with the pinned entry.
#[test]
fn resolve_model_answers_the_pinned_entry() {
    let entry = whisper_registry()
        .resolve_model("model:speech-to-text/whisper-base")
        .expect("the model resolves");
    assert_eq!(
        entry.image,
        format!("ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base@{DIGEST}")
    );
    let resources = entry.resources.expect("the entry carries resources");
    assert_eq!(resources.timeout, Some(Duration::from_secs(600)));
}

/// M6: an unknown model is refused with the registry's list.
#[test]
fn resolve_model_refuses_an_unknown_model() {
    let refusal = whisper_registry()
        .resolve_model("model:speech-to-text/whisper-large")
        .map(|_| ())
        .map_err(|e| e.to_string());
    assert_eq!(
        refusal,
        Err(
            "unknown model 'speech-to-text/whisper-large'; the models are speech-to-text/whisper-base"
                .to_string()
        )
    );
}

/// M5: a model step with no `network_mode` runs with `none`.
#[test]
fn a_model_step_without_network_mode_runs_with_none() {
    let workflow = WorkflowParser::parse_yaml(&model_workflow("")).expect("the model step parses");
    match container_state(&workflow, "TRANSCRIBE") {
        StateKind::ContainerRun {
            network_mode,
            image,
            ..
        } => {
            assert_eq!(network_mode.as_deref(), Some("none"));
            assert_eq!(image, "model:speech-to-text/whisper-base");
        }
        other => panic!("expected a ContainerRun, got {other:?}"),
    }
}

/// M5: `network_mode: none` written out is accepted.
#[test]
fn a_model_step_with_network_mode_none_parses() {
    let workflow = WorkflowParser::parse_yaml(&model_workflow("      network_mode: none"))
        .expect("a model step with network_mode none parses");
    match container_state(&workflow, "TRANSCRIBE") {
        StateKind::ContainerRun { network_mode, .. } => {
            assert_eq!(network_mode.as_deref(), Some("none"))
        }
        other => panic!("expected a ContainerRun, got {other:?}"),
    }
}

/// M5: any other `network_mode` is refused at parse with M5's sentence.
#[test]
fn a_model_step_with_egress_is_refused() {
    let refusal = WorkflowParser::parse_yaml(&model_workflow("      network_mode: egress"))
        .map(|_| ())
        .map_err(|e| e.to_string());
    let message = refusal.expect_err("a model step with egress is refused");
    assert!(
        message
            .contains("a model step runs with no network; remove network_mode or set it to none"),
        "refusal was: {message}"
    );
}

/// A `model:` reference that is not `model:<class>/<name>` is refused at parse.
#[test]
fn a_malformed_model_reference_is_refused() {
    let yaml =
        model_workflow("").replace("model:speech-to-text/whisper-base", "model:whisper-base");
    let message = WorkflowParser::parse_yaml(&yaml)
        .map(|_| ())
        .expect_err("a malformed model reference is refused")
        .to_string();
    assert!(
        message.contains("a model image is named model:<class>/<name>, not 'model:whisper-base'"),
        "refusal was: {message}"
    );
}

/// M3, M4: resolution replaces the reference with the pinned image, and the
/// registry's resources apply to a state that sets none.
#[test]
fn resolution_gives_the_step_the_pinned_image_and_the_models_resources() {
    let mut workflow =
        WorkflowParser::parse_yaml(&model_workflow("")).expect("the model step parses");
    workflow
        .resolve_model_images(Some(&whisper_registry()))
        .expect("the model step resolves");
    match container_state(&workflow, "TRANSCRIBE") {
        StateKind::ContainerRun {
            image,
            resources,
            network_mode,
            ..
        } => {
            assert_eq!(
                image,
                format!(
                    "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base@{DIGEST}"
                )
            );
            let resources = resources.expect("the model's resources apply");
            assert_eq!(resources.cpu, Some(2000));
            assert_eq!(resources.memory.as_deref(), Some("1Gi"));
            assert_eq!(network_mode.as_deref(), Some("none"));
        }
        other => panic!("expected a ContainerRun, got {other:?}"),
    }
}

/// M6: resolution of an unknown model is refused with M6's sentence.
#[test]
fn resolution_refuses_an_unknown_model() {
    let yaml = model_workflow("").replace("whisper-base", "whisper-large");
    let mut workflow = WorkflowParser::parse_yaml(&yaml).expect("the form is right");
    let message = workflow
        .resolve_model_images(Some(&whisper_registry()))
        .expect_err("an unknown model is refused")
        .to_string();
    assert!(
        message.contains(
            "unknown model 'speech-to-text/whisper-large'; the models are speech-to-text/whisper-base"
        ),
        "refusal was: {message}"
    );
}

/// M6: with no registry, a model image is refused with M6's sentence and an
/// empty list.
#[test]
fn resolution_without_a_registry_refuses_a_model_image() {
    let mut workflow =
        WorkflowParser::parse_yaml(&model_workflow("")).expect("the model step parses");
    let message = workflow
        .resolve_model_images(None)
        .expect_err("no registry, no model")
        .to_string();
    assert!(
        message.contains("unknown model 'speech-to-text/whisper-base'; the models are none"),
        "refusal was: {message}"
    );
}

/// S2: the built-in `transcribe-audio` parses, is labelled built-in, and
/// resolves against a registry carrying the whisper model.
#[test]
fn transcribe_audio_template_parses_and_resolves() {
    let yaml =
        std::fs::read_to_string(repo_root().join("cli/templates/workflows/transcribe-audio.yaml"))
            .expect("the transcribe-audio template exists");
    let mut workflow = WorkflowParser::parse_yaml(&yaml).expect("the template parses");
    assert_eq!(workflow.metadata.name, "transcribe-audio");
    assert_eq!(
        workflow.metadata.labels.get("builtin").map(String::as_str),
        Some("true")
    );
    workflow
        .resolve_model_images(Some(&whisper_registry()))
        .expect("the template's model step resolves");
    let model_steps: Vec<String> = workflow
        .spec
        .states
        .values()
        .filter_map(|state| match &state.kind {
            StateKind::ContainerRun { image, .. } => Some(image.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        model_steps,
        vec![format!(
            "ghcr.io/100monkeys-ai/aegis-model-whisper:whisper-cpp-1.9.4-base@{DIGEST}"
        )]
    );
}

/// S1, M4: the stack template's whisper entry names the tag
/// `whisper-cpp-<WHISPER_CPP_VERSION>-<WHISPER_MODEL>` that
/// `docker/Dockerfile.model-whisper`'s versions give and `pipeline.yml`
/// publishes. Until the first publish gives its digest the entry is a comment.
#[test]
fn the_template_whisper_tag_is_the_dockerfiles_versions() {
    let dockerfile =
        std::fs::read_to_string(repo_root().join("docker/Dockerfile.model-whisper")).unwrap();
    let arg = |name: &str| {
        dockerfile
            .lines()
            .find_map(|line| line.strip_prefix(&format!("ARG {name}=")))
            .unwrap_or_else(|| panic!("the Dockerfile names no {name}"))
            .to_string()
    };
    let tag = format!(
        "whisper-cpp-{}-{}",
        arg("WHISPER_CPP_VERSION"),
        arg("WHISPER_MODEL")
    );
    let template =
        std::fs::read_to_string(repo_root().join("cli/templates/stack/runtime-registry.yaml"))
            .unwrap();
    let named = template
        .lines()
        .find_map(|line| {
            line.split("ghcr.io/100monkeys-ai/aegis-model-whisper:")
                .nth(1)
        })
        .expect("the template names the whisper image")
        .split(['@', '"'])
        .next()
        .unwrap()
        .to_string();
    assert_eq!(named, tag);
}

/// G1, M4: the whisper image is a CPU build whose base is pinned by digest and
/// whose engine source and weights are checked by `sha256sum -c`.
#[test]
fn the_whisper_dockerfile_is_pinned_and_cpu_only() {
    let dockerfile =
        std::fs::read_to_string(repo_root().join("docker/Dockerfile.model-whisper")).unwrap();
    let lower = dockerfile.to_lowercase();
    assert!(!lower.contains("cuda"), "the image names cuda");
    assert!(!lower.contains("nvidia"), "the image names nvidia");
    assert!(
        dockerfile
            .lines()
            .any(|line| line.starts_with("ARG BASE_IMAGE=") && line.contains("@sha256:")),
        "the base is not pinned by digest"
    );
    assert!(
        dockerfile.matches("sha256sum -c").count() >= 2,
        "the engine source and the weights are not both checked"
    );
}
