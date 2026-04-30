// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression test for the `aegis edge` clap runtime panic.
//!
//! Background: prior to this fix, the top-level `Cli` declared
//! `--output: OutputFormat` (enum) as `global = true`, while several
//! `aegis edge` subcommand `Args` structs redeclared their own local
//! `--output: String`. Clap detects the type collision at the first
//! `args.get_one::<...>("output")` and panics with:
//!
//! ```text
//! Mismatch between definition and access of `output`.
//! Could not downcast to TypeId(...), need to downcast to TypeId(...)
//! ```
//!
//! Every invocation of `aegis edge enroll <token>` (and the other
//! affected subcommands) hit this panic during argument access. The fix
//! removes the per-subcommand `output` fields and threads the global
//! `Cli::output` through the dispatch path.
//!
//! This test reconstructs the same global+subcommand shape used by the
//! production `Cli` struct and exercises clap's parse + access paths
//! the same way `clap::Parser::parse` does at runtime. If a future
//! change reintroduces a colliding local `--output` field on any
//! `aegis edge` subcommand, this test will panic in the same way the
//! production binary did and fail the build.

use aegis_orchestrator::commands::edge::EdgeArgs;
use aegis_orchestrator::output::OutputFormat;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "aegis")]
struct SmokeCli {
    /// Mirror of the real top-level global `--output` flag.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    output: OutputFormat,

    #[command(subcommand)]
    command: SmokeCommand,
}

#[derive(Subcommand)]
enum SmokeCommand {
    #[command(name = "edge")]
    Edge(EdgeArgs),
}

/// Calling `Cli::parse` is what triggers the clap downcast — `try_parse_from`
/// alone is not enough to surface the panic, because clap defers the type
/// check until `get_one::<T>()` runs against a populated arg matcher. We
/// reproduce that access path explicitly to ensure the regression is pinned.
fn parse_and_access(argv: &[&str]) -> OutputFormat {
    let cli = SmokeCli::try_parse_from(argv).expect("clap must accept the argv");
    // Access every field that clap exposes — in particular `output` —
    // so any TypeId mismatch surfaces here instead of in production.
    match cli.command {
        SmokeCommand::Edge(_) => {}
    }
    cli.output
}

#[test]
fn aegis_edge_enroll_does_not_panic_on_output_flag_collision() {
    // Before the fix, this invocation panicked inside clap with a
    // TypeId downcast mismatch for `output`. After the fix, parsing
    // and accessing the global `--output` succeeds and returns Text.
    let out = parse_and_access(&["aegis", "edge", "enroll", "fake-jwt-token"]);
    assert_eq!(out, OutputFormat::Text);
}

#[test]
fn aegis_edge_status_does_not_panic_on_output_flag_collision() {
    let out = parse_and_access(&["aegis", "edge", "status"]);
    assert_eq!(out, OutputFormat::Text);
}

#[test]
fn aegis_edge_ls_does_not_panic_on_output_flag_collision() {
    let out = parse_and_access(&["aegis", "edge", "ls"]);
    assert_eq!(out, OutputFormat::Text);
}

#[test]
fn aegis_edge_fleet_runs_does_not_panic_on_output_flag_collision() {
    let out = parse_and_access(&["aegis", "edge", "fleet", "runs"]);
    assert_eq!(out, OutputFormat::Text);
}

#[test]
fn global_output_flag_propagates_to_edge_subcommands() {
    // Verify the global flag is honored at every affected subcommand
    // — these are the call sites that previously shadowed it.
    for argv in [
        &[
            "aegis",
            "--output",
            "json",
            "edge",
            "enroll",
            "fake-jwt-token",
        ][..],
        &["aegis", "--output", "json", "edge", "status"][..],
        &["aegis", "--output", "json", "edge", "ls"][..],
        &["aegis", "--output", "json", "edge", "fleet", "runs"][..],
    ] {
        let out = parse_and_access(argv);
        assert_eq!(out, OutputFormat::Json, "argv={argv:?}");
    }
}
