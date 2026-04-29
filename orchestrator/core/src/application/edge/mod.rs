// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Edge Mode Application Services (ADR-117)
//!
//! Use-case-oriented application layer for the AEGIS Edge Daemon. Each
//! submodule corresponds to one operator-facing or daemon-facing capability:
//!
//! | Module | Purpose |
//! | --- | --- |
//! | [`issue_enrollment_token`] | Mint a short-lived enrollment JWT. |
//! | [`enroll_edge`]            | Validate JWT, redeem `jti`, persist EdgeDaemon. |
//! | [`connect_edge`]           | Drive the bidi gRPC stream once `Hello` arrives. |
//! | [`dispatch_to_edge`]       | Single-target reverse-RPC tool dispatch. |
//! | [`revoke_edge`]            | Operator-initiated revocation. |
//! | [`manage_tags`]            | Operator-managed tag mutation. |
//! | [`manage_groups`]          | Edge group CRUD with tenant scope. |
//! | [`rotate_edge_key`]        | Atomic dual-signature key rotation. |
//! | [`fleet`]                  | Multi-target fan-out (resolver/dispatcher/registry). |

pub mod connect_edge;
pub mod dispatch_to_edge;
pub mod enroll_edge;
pub mod fleet;
pub mod issue_enrollment_token;
pub mod manage_groups;
pub mod manage_tags;
pub mod revoke_edge;
pub mod rotate_edge_key;
