// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge enroll <token>` — redeem an enrollment token, bootstrap local
//! state, attest+challenge against the controller, persist the
//! NodeSecurityToken, then exit. The long-lived bidi stream is started
//! separately by the daemon process (see `cli::daemon::edge_lifecycle`).

use clap::Args;

use super::bootstrap::{run_bootstrap, BootstrapPlan, BootstrapPolicy, OutputFormat};

#[derive(Debug, Args)]
pub struct EnrollArgs {
    /// Enrollment token issued by `POST /v1/edge/enrollment-tokens`.
    pub token: String,
    /// Override the local edge state directory (default: `~/.aegis/edge`).
    #[arg(long)]
    pub state_dir: Option<std::path::PathBuf>,
    /// Disable interactive prompts (CI / config-management runs).
    #[arg(long)]
    pub non_interactive: bool,
    /// Assume Overwrite for every conflict.
    #[arg(long)]
    pub force: bool,
    /// Assume Reuse for every conflict (errors if a Reuse decision is unsafe).
    #[arg(long)]
    pub keep_existing: bool,
    /// Print the BootstrapPlan without writing or contacting the server.
    #[arg(long)]
    pub dry_run: bool,
    /// Emit a minimal config rather than the annotated default.
    #[arg(long)]
    pub minimal: bool,
    /// Output format (`text` | `json`).
    #[arg(long, default_value = "text")]
    pub output: String,
}

pub async fn run(args: EnrollArgs) -> anyhow::Result<()> {
    let policy = BootstrapPolicy {
        non_interactive: args.non_interactive,
        force: args.force,
        keep_existing: args.keep_existing,
        dry_run: args.dry_run,
        minimal_config: args.minimal,
    };
    let output = match args.output.as_str() {
        "json" => OutputFormat::Json,
        _ => OutputFormat::Text,
    };
    let state_dir = args.state_dir.clone();
    let plan = BootstrapPlan::detect(state_dir, &args.token, &policy)?;
    run_bootstrap(plan, &policy, output, &args.token).await
}
