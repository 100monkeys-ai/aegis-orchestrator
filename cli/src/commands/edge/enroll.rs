// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge enroll <token>` — redeem an enrollment token, bootstrap local
//! state, attest+challenge against the controller, persist the
//! NodeSecurityToken, then exit. The long-lived bidi stream is started
//! separately by the daemon process (see `cli::daemon::edge_lifecycle`).

use anyhow::{Context, Result};
use clap::Args;
use std::path::Path;

use super::bootstrap::{
    run_bootstrap, BootstrapPlan, BootstrapPolicy, OutputFormat as BootstrapOutputFormat,
};
use super::grpc;
use super::handshake::{run_attest_and_challenge, HandshakeOutcome};
use crate::output::OutputFormat;

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
}

pub async fn run(args: EnrollArgs, output: OutputFormat) -> anyhow::Result<()> {
    let policy = BootstrapPolicy {
        non_interactive: args.non_interactive,
        force: args.force,
        keep_existing: args.keep_existing,
        dry_run: args.dry_run,
        minimal_config: args.minimal,
    };
    // Convert from the global OutputFormat (Text|Table|Json|Yaml) to the
    // bootstrap-local OutputFormat (Text|Json). Table/Yaml are not
    // meaningful renderers for the bootstrap progress / plan output, so
    // they collapse to Text.
    let bootstrap_output = match output {
        OutputFormat::Json => BootstrapOutputFormat::Json,
        _ => BootstrapOutputFormat::Text,
    };

    // Step 1 (local FS): generate keypair, persist enrollment.jwt, write
    // aegis-config.yaml. Returns Ok before any network I/O.
    let plan = BootstrapPlan::detect(args.state_dir.clone(), &args.token, &policy)?;
    let resolved_state_dir = plan.state_dir.clone();
    run_bootstrap(plan, &policy, bootstrap_output, &args.token).await?;

    // --dry-run terminates before the wire flow per BootstrapPolicy contract.
    if policy.dry_run {
        return Ok(());
    }

    // --keep-existing short-circuit: if a valid node.token is already on
    // disk, the bootstrap step has already verified the existing identity is
    // safe to reuse (config endpoint matches, node.key is a valid 32-byte
    // seed). Calling AttestNode + ChallengeNode again would burn the
    // enrollment JWT for no reason and the server would refuse the redeemed
    // jti. Treat enrollment as already complete.
    let node_token_path = resolved_state_dir.join("node.token");
    if policy.keep_existing && node_token_path.exists() {
        let outcome = build_outcome_from_existing(&resolved_state_dir, &args.token)?;
        emit_output(&outcome, &node_token_path, output, /*reused=*/ true)?;
        return Ok(());
    }

    // Steps 2-4 (wire): AttestNode → ChallengeNode → persist NodeSecurityToken.
    let signing_key = grpc::load_signing_key(&resolved_state_dir)
        .context("load freshly-bootstrapped node.key")?;

    // The cep claim of the enrollment JWT pins which controller to dial.
    // The bootstrap step already validated the JWT is well-formed; we
    // re-decode here rather than threading the value through to keep the
    // bootstrap and handshake stages independent.
    let claims = super::handshake::decode_enrollment_claims(&args.token)?;

    // The daemon's node_id is a UUID minted client-side at bootstrap and
    // persisted in `aegis-config.yaml` (`spec.node.id`). It is decoupled
    // from the enrollment JWT's `sub` claim (which is operator display
    // metadata, e.g. "BEASTLY1"). Loading from config also pins the
    // persistence contract: a re-enroll on the same host re-uses the same
    // node_id rather than minting a new one.
    let node_id = grpc::load_node_id_from_config(&resolved_state_dir)
        .context("read minted node_id from aegis-config.yaml after bootstrap")?;

    let outcome =
        run_attest_and_challenge(&claims.cep, &node_id, &signing_key, &args.token).await?;

    // Persist node.token AFTER the handshake succeeds. Atomic-write +
    // mode-0600 mirrors how `bootstrap.rs` and `keys.rs` treat secrets — a
    // partial write would corrupt the daemon's identity.
    grpc::atomic_write_secret(&node_token_path, outcome.node_security_token.as_bytes())
        .context("persist NodeSecurityToken to node.token")?;

    emit_output(&outcome, &node_token_path, output, /*reused=*/ false)?;
    Ok(())
}

/// When `--keep-existing` short-circuits the wire flow, synthesise a
/// HandshakeOutcome from the on-disk artifacts so the operator still sees
/// a structured success block.
fn build_outcome_from_existing(state_dir: &Path, enrollment_jwt: &str) -> Result<HandshakeOutcome> {
    let token = grpc::load_node_security_token(state_dir)?;
    let claims = super::handshake::decode_enrollment_claims(enrollment_jwt)?;
    // The persisted node_id is the `sub` claim of the issued NodeSecurityToken
    // — the daemon's UUID. The enrollment JWT's `sub` is unrelated operator
    // display metadata and must not be confused with the node identity.
    let node_id = grpc::node_id_from_token(&token)?;
    Ok(HandshakeOutcome {
        node_id,
        tenant_id: claims.tid,
        controller_endpoint: claims.cep,
        node_security_token: token,
        // We deliberately do not parse the existing token's `exp` here — the
        // caller path emits this only as informational metadata, and the
        // daemon's `token refresh` command is the authoritative expiry source.
        expires_at: None,
    })
}

fn emit_output(
    outcome: &HandshakeOutcome,
    node_token_path: &Path,
    output: OutputFormat,
    reused: bool,
) -> Result<()> {
    let path_str = node_token_path.display().to_string();
    let expires_str = outcome.expires_at.map(|t| t.to_rfc3339());
    match output {
        OutputFormat::Json => {
            let body = serde_json::json!({
                "ok": true,
                "reused_existing_token": reused,
                "node_id": outcome.node_id,
                "tenant_id": outcome.tenant_id,
                "controller_endpoint": outcome.controller_endpoint,
                "node_token_path": path_str,
                "expires_at": expires_str,
            });
            println!("{}", serde_json::to_string(&body)?);
        }
        _ => {
            if reused {
                println!("aegis edge: enrollment already complete (reused existing node.token)");
            } else {
                println!("aegis edge: enrollment complete");
            }
            println!("  node_id:             {}", outcome.node_id);
            println!("  tenant_id:           {}", outcome.tenant_id);
            println!("  controller_endpoint: {}", outcome.controller_endpoint);
            println!("  node_token_path:     {path_str}");
            if let Some(exp) = expires_str {
                println!("  expires_at:          {exp}");
            }
        }
    }
    Ok(())
}
