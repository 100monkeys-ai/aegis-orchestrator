// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Infrastructure (ADR-075)
//!
//! Cortex-backed event handler for maintaining the discovery index in sync
//! with agent and workflow lifecycle events.

pub mod event_handler;

pub use event_handler::DiscoveryIndexEventHandler;
