// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §D — fleet (multi-target) operations.

pub mod cancel;
pub mod dispatcher;
pub mod registry;
pub mod resolver;

pub use cancel::CancelFleetService;
pub use dispatcher::{FleetDispatcher, FleetEvent};
pub use registry::{FleetCommandHandle, FleetRegistry};
pub use resolver::EdgeFleetResolver;
