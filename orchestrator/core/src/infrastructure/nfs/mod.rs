// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! NFS Server Infrastructure (ADR-036)
//!
//! Provides user-space NFSv3 server implementation using the nfsserve crate.
//! Acts as the transport layer for AegisFSAL (File System Abstraction Layer),
//! enabling transparent POSIX filesystem access for agent containers.
//!
//! ## Architecture
//! ```
//! Agent Container (NFS client) → NFSv3 Protocol (port 2049)
//!   → AegisFsalAdapter (implements nfsserve::NFSFileSystem)
//!   → AegisFSAL (domain entity, security boundary)
//!   → StorageProvider trait (SeaweedFS/Local/S3)
//! ```
//!
//! ## Key Features (ADR-036)
//! - **Zero Agent Privileges**: Agent containers require no elevated capabilities (no CAP_SYS_ADMIN)
//! - **File-Level Authorization**: Every operation validates execution owns volume
//! - **Path Sanitization**: Server-side canonicalization prevents `../` traversal attacks
//! - **UID/GID Squashing**: File metadata returns container UID/GID, eliminating kernel permission checks
//! - **Full Audit Trail**: Every file operation generates `StorageEvent` domain events
//! - **Policy Enforcement**: Manifest filesystem policies (read/write allowlists) enforced at FSAL layer
//!
//! ## NFSv3 Protocol Details
//! - **nolock mount option**: Phase 1 has no NLM (Network Lock Manager) support
//! - **soft mount with 10s timeout**: Graceful failure on network issues
//! - **TCP protocol**: More reliable than UDP for container networks
//! - **FileHandle encoding**: AegisFileHandle serialized with bincode (<64 bytes for NFSv3)
//!
//! ## Transport Abstraction
//! This NFS implementation is **Phase 1 (Docker)**. The same `AegisFSAL` core will be reused
//! for **Phase 2+ (Firecracker)** via virtio-fs transport, with zero security logic duplication.
//!
//! ## Integration with Other Contexts
//! - **Volume Aggregate**: Operates on volumes created by `VolumeManager` service
//! - **Security Policy Context**: Enforces `FilesystemPolicy` from agent manifests
//! - **Execution Context**: Stores container UID/GID in execution metadata for permission squashing
//! - **Event Bus**: Publishes `StorageEvent` for audit trail and Cortex learning
//! - **MCP Tool Routing**: Follows same Orchestrator Proxy Pattern (ADR-033)

pub mod server;
pub mod file_handle;

pub use server::NfsServer;
pub use file_handle::{encode_file_handle, decode_file_handle};
