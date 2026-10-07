// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The StandardRuntime registry the stack template ships
//! (`cli/templates/stack/runtime-registry.yaml`, ADR-043) against the built-in
//! agent templates (`cli/templates/agents/`).
//!
//! The production failure it pins (2026-10-01): `builtin-intent-to-execution`
//! advertises language `bash` and routes it to `aegis-bash-executor-agent`,
//! whose manifest asks for `bash` `"5"`, but no registry had a bash runtime, so
//! every bash intent failed before any container with "Unsupported Standard
//! Runtime: bash 5. Unsupported language 'bash'".

use aegis_orchestrator_core::domain::runtime_registry::StandardRuntimeRegistry;
use std::path::{Path, PathBuf};

fn templates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/templates")
}

fn registry() -> StandardRuntimeRegistry {
    StandardRuntimeRegistry::from_file(templates().join("stack/runtime-registry.yaml"))
        .expect("the stack template's registry loads")
}

/// T8.
#[test]
fn template_registry_resolves_bash_5() {
    assert_eq!(registry().resolve("bash", "5").unwrap(), "python:3.11-slim");
}

/// Every built-in agent template that names a StandardRuntime resolves in the
/// registry the stack template ships.
#[test]
fn every_builtin_agent_runtime_resolves_in_the_template_registry() {
    let registry = registry();
    let mut checked = 0;
    for entry in std::fs::read_dir(templates().join("agents")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let manifest: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let runtime = &manifest["spec"]["runtime"];
        if runtime["image"].as_str().is_some() {
            continue;
        }
        let (Some(language), Some(version)) =
            (runtime["language"].as_str(), runtime["version"].as_str())
        else {
            continue;
        };
        if let Err(e) = registry.resolve(language, version) {
            panic!("{}: {language} {version}: {e}", path.display());
        }
        checked += 1;
    }
    assert!(checked > 0, "no built-in agent template was read");
}

/// The document renderer's runtime: the built-in agent
/// `aegis-document-renderer-agent` names `document` `"1"`, which resolves to
/// the image `pipeline.yml` publishes from `docker/Dockerfile.document-renderer`,
/// by its version tag.
#[test]
fn template_registry_resolves_document_1_to_the_renderer_image() {
    let resolved = registry()
        .resolve("document", "1")
        .map_err(|e| e.to_string());
    assert_eq!(
        resolved,
        Ok("ghcr.io/100monkeys-ai/aegis-document-renderer:pandoc-3.12-typst-0.15.1".to_string()),
        "the stack template's registry does not give document 1 the renderer image"
    );
}

/// The registry's tag for the renderer is the one `docker/Dockerfile.document-renderer`'s
/// versions give, `pandoc-<PANDOC_VERSION>-typst-<TYPST_VERSION>`, which
/// `pipeline.yml` reads from the same lines, so a version change moves both.
#[test]
fn renderer_tag_is_the_dockerfiles_versions() {
    let dockerfile = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/Dockerfile.document-renderer"),
    )
    .unwrap_or_default();
    let arg = |name: &str| {
        dockerfile
            .lines()
            .find_map(|line| line.strip_prefix(&format!("ARG {name}=")))
            .unwrap_or("<missing>")
            .to_string()
    };
    let tag = format!(
        "ghcr.io/100monkeys-ai/aegis-document-renderer:pandoc-{}-typst-{}",
        arg("PANDOC_VERSION"),
        arg("TYPST_VERSION")
    );
    let resolved = registry()
        .resolve("document", "1")
        .map_err(|e| e.to_string());
    assert_eq!(
        resolved,
        Ok(tag),
        "the registry's renderer image is not the tag the Dockerfile's versions give"
    );
}
