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
const SMCP_GATEWAY_CONFIG_TEMPLATE: &str =
    include_str!("../../../templates/stack/smcp-gateway-config.yaml");

/// Bundle of stack files used by `aegis init`.
pub struct StackFiles {
    pub docker_compose: String,
    pub init_db_script: String,
    pub runtime_registry: String,
    pub temporal_dynamic_config: String,
    pub smcp_gateway_config: String,
}

/// Load stack files from bundled templates.
pub async fn fetch_stack() -> Result<StackFiles> {
    println!();
    println!("{}", "Loading bundled stack templates...".bold());
    println!("  {} Stack files ready", "✓".green());

    Ok(StackFiles {
        docker_compose: DOCKER_COMPOSE_TEMPLATE.to_string(),
        init_db_script: INIT_DB_SCRIPT_TEMPLATE.to_string(),
        runtime_registry: RUNTIME_REGISTRY_TEMPLATE.to_string(),
        temporal_dynamic_config: TEMPORAL_DYNAMIC_CONFIG_TEMPLATE.to_string(),
        smcp_gateway_config: SMCP_GATEWAY_CONFIG_TEMPLATE.to_string(),
    })
}
