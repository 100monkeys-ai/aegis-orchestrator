// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # AEGIS Cluster Protocol Buffer Definitions
//!
//! Shim for `aegis_orchestrator_proto` crate types.

pub mod aegis {
    pub mod runtime {
        pub mod v1 {
            pub use aegis_orchestrator_proto::aegis::runtime::v1::*;
        }
    }

    pub mod cluster {
        pub mod v1 {
            pub use aegis_orchestrator_proto::aegis::cluster::v1::*;
        }
    }
}

pub use aegis::cluster::v1::*;
