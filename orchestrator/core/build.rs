// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Build Script for aegis-core
//!
//! This build script compiles Protocol Buffer definitions for gRPC communication
//! with external services, primarily Temporal.io workflow engine integration.
//!
//! # Compilation Targets
//!
//! - **Temporal API**: Workflow service, common types, task queues, history
//! - **AEGIS Runtime**: Custom runtime protocol definitions
//!
//! # Code Generation
//!
//! Uses `tonic-build` to generate Rust code from `.proto` files located in:
//! - `../../aegis-proto/proto/aegis_runtime.proto` - AEGIS-specific protocols (git submodule: aegis-proto)
//! - `../../proto/temporal/api/**/*.proto` - Temporal API definitions
//!
//! Generated code is placed in `OUT_DIR` and included via `tonic::include_proto!`
//! in `src/infrastructure/temporal_proto.rs`.
//!
//! # Dependencies
//!
//! - **protoc**: Protocol buffer compiler (vendored via `protoc-bin-vendored`)
//! - **tonic-build**: Code generator for Rust gRPC stubs
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements internal responsibilities for build

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set PROTOC environment variable to point to the vendored protoc binary
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());

    // Get the manifest directory and construct paths
    // CARGO_MANIFEST_DIR = aegis-orchestrator/orchestrator/core
    // We need to go up 2 levels to aegis-orchestrator root, then access proto/
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let crate_root = std::path::Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .ok_or("Failed to find crate root")?;

    // Construct paths relative to aegis-orchestrator crate root
    let aegis_proto_dir = crate_root
        .join("aegis-proto/proto")
        .to_string_lossy()
        .to_string();
    let proto_dir = crate_root.join("proto").to_string_lossy().to_string();

    let mut protos = Vec::new();

    // Add aegis_runtime from the aegis-proto submodule
    protos.push(
        crate_root
            .join("aegis-proto/proto/aegis_runtime.proto")
            .to_string_lossy()
            .to_string(),
    );

    let temporal_protos = &[
        "workflowservice/v1/service.proto",
        "workflowservice/v1/request_response.proto",
        "common/v1/message.proto",
        "taskqueue/v1/message.proto",
        "enums/v1/workflow.proto",
        "enums/v1/namespace.proto",
        "enums/v1/task_queue.proto",
        "enums/v1/common.proto",
        "enums/v1/query.proto",
        "enums/v1/event_type.proto",
        "enums/v1/failed_cause.proto",
        "enums/v1/reset.proto",
        "enums/v1/schedule.proto",
        "enums/v1/update.proto",
        "enums/v1/batch_operation.proto",
        "enums/v1/deployment.proto",
        "enums/v1/activity.proto",
        "enums/v1/nexus.proto",
        "activity/v1/message.proto",
        "history/v1/message.proto",
        "command/v1/message.proto",
        "protocol/v1/message.proto",
        "rules/v1/message.proto",
        "batch/v1/message.proto",
        "worker/v1/message.proto",
        "sdk/v1/worker_config.proto",
        "sdk/v1/user_metadata.proto",
        "sdk/v1/task_complete_metadata.proto",
        "sdk/v1/workflow_metadata.proto",
        "sdk/v1/enhanced_stack_trace.proto",
        "failure/v1/message.proto",
        "filter/v1/message.proto",
        "namespace/v1/message.proto",
        "query/v1/message.proto",
        "replication/v1/message.proto",
        "schedule/v1/message.proto",
        "update/v1/message.proto",
        "version/v1/message.proto",
        "workflow/v1/message.proto",
        "nexus/v1/message.proto",
        "deployment/v1/message.proto",
    ];
    for proto_file in temporal_protos {
        let full_path = crate_root.join("proto/temporal/api").join(proto_file);
        if full_path.exists() {
            protos.push(full_path.to_string_lossy().to_string());
        }
    }

    // Only compile if we have proto files to compile
    if !protos.is_empty() {
        tonic_build::configure()
            .build_server(true)
            .build_client(true)
            .compile_protos(&protos, &[&proto_dir, &aegis_proto_dir])?;
    }

    let aegis_proto_file = crate_root.join("aegis-proto/proto/aegis_runtime.proto");
    if aegis_proto_file.exists() {
        println!(
            "cargo:rerun-if-changed={}",
            aegis_proto_file.to_string_lossy()
        );
    }

    Ok(())
}
