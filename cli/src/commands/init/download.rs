// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! GitHub download step of `aegis init`.
//!
//! Loads bundled stack templates used by `aegis init`.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** stack asset load step inside the `aegis init` wizard

use anyhow::Result;
use colored::Colorize;
const DOCKER_COMPOSE_TEMPLATE: &str = include_str!("../../../templates/stack/docker-compose.yml");
const INIT_DB_SCRIPT_TEMPLATE: &str = include_str!("../../../templates/stack/init-multiple-dbs.sh");
const RUNTIME_REGISTRY_TEMPLATE: &str =
    include_str!("../../../templates/stack/runtime-registry.yaml");
const TEMPORAL_DYNAMIC_CONFIG_TEMPLATE: &str =
    include_str!("../../../templates/stack/development-sql.yaml");
const SEAL_GATEWAY_CONFIG_TEMPLATE: &str =
    include_str!("../../../templates/stack/seal-gateway-config.yaml");

/// Bundle of stack files used by `aegis init`.
pub struct StackFiles {
    pub docker_compose: String,
    pub init_db_script: String,
    pub runtime_registry: String,
    pub temporal_dynamic_config: String,
    pub seal_gateway_config: String,
}

/// Load stack files from bundled templates.
///
/// `tag` — Docker image tag substituted into all `{{AEGIS_IMAGE_TAG}}` placeholders
/// in the compose template (e.g. `"0.1.0-pre-alpha"` or `"latest"`).
pub async fn fetch_stack(tag: &str) -> Result<StackFiles> {
    println!();
    println!("{}", "Loading bundled stack templates...".bold());
    println!("  {} Using AEGIS image tag: {}", "✓".green(), tag);
    println!("  {} Stack files ready", "✓".green());

    Ok(StackFiles {
        docker_compose: DOCKER_COMPOSE_TEMPLATE.replace("{{AEGIS_IMAGE_TAG}}", tag),
        init_db_script: INIT_DB_SCRIPT_TEMPLATE.to_string(),
        runtime_registry: RUNTIME_REGISTRY_TEMPLATE.to_string(),
        temporal_dynamic_config: TEMPORAL_DYNAMIC_CONFIG_TEMPLATE.to_string(),
        seal_gateway_config: SEAL_GATEWAY_CONFIG_TEMPLATE.to_string(),
    })
}
