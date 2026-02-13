// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Command implementations for AEGIS CLI

pub mod config;
pub mod daemon;
pub mod task;
pub mod agent;
pub mod workflow;

pub use self::config::ConfigCommand;
pub use self::daemon::DaemonCommand;
pub use self::task::TaskCommand;
pub use self::agent::AgentCommand;
pub use self::workflow::WorkflowCommand;
pub mod update;
pub use self::update::UpdateCommand;
