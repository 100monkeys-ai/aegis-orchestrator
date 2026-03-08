// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Gateway Protocol Buffer Definitions
//!
//! Generated gRPC types from `smcp_gateway.proto` used by orchestrator clients.
//!
//! All types from the `aegis.smcp_gateway.v1` package are available directly
//! in this module (e.g. [`InvokeCliRequest`], [`InvokeWorkflowRequest`],
//! [`ListToolsRequest`], and the `gateway_invocation_service_client` submodule).

tonic::include_proto!("aegis.smcp_gateway.v1");
