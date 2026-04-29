// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 `aegis edge` subcommand family.

use clap::{Args, Subcommand};

pub mod bootstrap;
pub mod enroll;
pub mod fleet;
pub mod group;
pub mod keys;
pub mod logout;
pub mod ls;
pub mod selector;
pub mod status;
pub mod tag;
pub mod token;

#[derive(Debug, Args)]
pub struct EdgeArgs {
    #[command(subcommand)]
    pub command: EdgeCommand,
}

#[derive(Debug, Subcommand)]
pub enum EdgeCommand {
    /// Redeem an enrollment token and bootstrap local edge state.
    Enroll(enroll::EnrollArgs),
    /// Show local daemon status, tenant binding and capabilities.
    Status(status::StatusArgs),
    /// Local revocation: delete node.token and key from disk.
    Logout,
    /// List edges visible to the current operator (operator-side).
    Ls(ls::LsArgs),
    /// Operator-managed tag mutation.
    #[command(subcommand)]
    Tag(tag::TagCommand),
    /// Edge group operations.
    #[command(subcommand)]
    Group(group::GroupCommand),
    /// Fleet (multi-target) operations.
    #[command(subcommand)]
    Fleet(fleet::FleetCommand),
    /// Local key rotation operations.
    #[command(subcommand)]
    Keys(keys::KeysCommand),
    /// NodeSecurityToken refresh.
    #[command(subcommand)]
    Token(token::TokenCommand),
}

pub async fn run(args: EdgeArgs) -> anyhow::Result<()> {
    match args.command {
        EdgeCommand::Enroll(a) => enroll::run(a).await,
        EdgeCommand::Status(a) => status::run(a).await,
        EdgeCommand::Logout => logout::run().await,
        EdgeCommand::Ls(a) => ls::run(a).await,
        EdgeCommand::Tag(c) => tag::run(c).await,
        EdgeCommand::Group(c) => group::run(c).await,
        EdgeCommand::Fleet(c) => fleet::run(c).await,
        EdgeCommand::Keys(c) => keys::run(c).await,
        EdgeCommand::Token(c) => token::run(c).await,
    }
}
