// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Lib
//!
//! Provides lib functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Learning & Memory Layer
//! - **Purpose:** Implements lib

pub mod domain;
pub mod application;
pub mod infrastructure;

pub use domain::*;
pub use infrastructure::*;
