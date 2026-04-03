// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Command implementations for AEGIS CLI
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for mod

pub mod agent;
pub mod auth;
pub mod builtins;
pub mod config;
pub mod daemon;
pub mod down;
pub mod init;
pub mod node;
pub mod restart;
pub mod status;
pub mod task;
pub mod uninstall;
pub mod up;
pub mod workflow;

pub use self::agent::AgentCommand;
pub use self::config::ConfigCommand;
pub use self::daemon::DaemonCommand;
pub use self::down::DownArgs;
pub use self::init::InitArgs;
pub use self::node::NodeCommand;
pub use self::restart::RestartArgs;
pub use self::status::StatusArgs;
pub use self::task::TaskCommand;
pub use self::uninstall::UninstallArgs;
pub use self::up::UpArgs;
pub use self::workflow::WorkflowCommand;
pub mod update;
pub use self::update::UpdateCommand;
