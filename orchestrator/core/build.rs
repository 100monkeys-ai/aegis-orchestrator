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
//! - `../../proto/aegis_runtime.proto` - AEGIS-specific protocols
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

    // Gather all .proto files in temporal/api
    let proto_root = "../../proto/temporal/api";
    let mut protos = Vec::new();
    
    // Add aegis_runtime manually
    protos.push("../../proto/aegis_runtime.proto".to_string());
    
    // Walk directory to find all .proto files
    if let Ok(_) = std::fs::read_dir(proto_root) {
        fn visit_dirs(dir: &std::path::Path, protos: &mut Vec<String>) -> std::io::Result<()> {
            if dir.is_dir() {
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_dir() {
                        visit_dirs(&path, protos)?;
                    } else if path.extension().map(|s| s == "proto").unwrap_or(false) {
                         protos.push(path.to_string_lossy().replace("\\", "/"));
                    }
                }
            }
            Ok(())
        }
        
        let root_path = std::path::Path::new(proto_root);
        let _ = visit_dirs(root_path, &mut protos);
    }
    
     let temporal_protos = &[
        "../../proto/temporal/api/workflowservice/v1/service.proto",
        "../../proto/temporal/api/workflowservice/v1/request_response.proto",
        "../../proto/temporal/api/common/v1/message.proto",
        "../../proto/temporal/api/taskqueue/v1/message.proto",
        "../../proto/temporal/api/enums/v1/workflow.proto",
        "../../proto/temporal/api/enums/v1/namespace.proto",
        "../../proto/temporal/api/enums/v1/task_queue.proto",
        "../../proto/temporal/api/enums/v1/common.proto",
        "../../proto/temporal/api/enums/v1/query.proto",
        "../../proto/temporal/api/enums/v1/event_type.proto",
        "../../proto/temporal/api/enums/v1/failed_cause.proto",
        "../../proto/temporal/api/enums/v1/reset.proto",
        "../../proto/temporal/api/enums/v1/schedule.proto",
        "../../proto/temporal/api/enums/v1/update.proto",
        "../../proto/temporal/api/enums/v1/batch_operation.proto",
        "../../proto/temporal/api/enums/v1/deployment.proto",
        "../../proto/temporal/api/enums/v1/activity.proto",
        "../../proto/temporal/api/enums/v1/nexus.proto",
        "../../proto/temporal/api/activity/v1/message.proto",
        "../../proto/temporal/api/history/v1/message.proto",
        "../../proto/temporal/api/command/v1/message.proto",
        "../../proto/temporal/api/protocol/v1/message.proto",
        "../../proto/temporal/api/rules/v1/message.proto",
        "../../proto/temporal/api/batch/v1/message.proto",
        "../../proto/temporal/api/worker/v1/message.proto",
        "../../proto/temporal/api/sdk/v1/worker_config.proto",
        "../../proto/temporal/api/sdk/v1/user_metadata.proto",
        "../../proto/temporal/api/sdk/v1/task_complete_metadata.proto",
        "../../proto/temporal/api/sdk/v1/workflow_metadata.proto",
        "../../proto/temporal/api/sdk/v1/enhanced_stack_trace.proto",
        "../../proto/temporal/api/failure/v1/message.proto",
        "../../proto/temporal/api/filter/v1/message.proto",
        "../../proto/temporal/api/namespace/v1/message.proto",
        "../../proto/temporal/api/query/v1/message.proto",
        "../../proto/temporal/api/replication/v1/message.proto",
        "../../proto/temporal/api/schedule/v1/message.proto",
        "../../proto/temporal/api/update/v1/message.proto",
        "../../proto/temporal/api/version/v1/message.proto",
        "../../proto/temporal/api/workflow/v1/message.proto",
        "../../proto/temporal/api/nexus/v1/message.proto",
        "../../proto/temporal/api/deployment/v1/message.proto",
    ];
    protos.extend(temporal_protos.iter().map(|s| s.to_string()));



    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &protos,
            &["../../proto"], 
        )?;

    println!("cargo:rerun-if-changed=../../proto/aegis_runtime.proto");

    Ok(())
}
