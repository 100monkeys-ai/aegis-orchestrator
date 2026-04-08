// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! FUSE-based FSAL Transport (ADR-107)
//!
//! Provides a user-space FUSE filesystem that maps POSIX operations to FSAL calls,
//! enabling bind-mount-based volume access for agent containers. This replaces the
//! NFS volume driver approach with host-local FUSE mounts, which works correctly
//! in rootless container runtimes (Podman) where NFS volume drivers are unsupported.
//!
//! ## Architecture
//! ```text
//! Agent Container (bind mount) → FUSE mountpoint (host path)
//!   → FuseFsalDaemon (implements fuser::Filesystem)
//!   → AegisFSAL (domain entity, security boundary)
//!   → StorageProvider trait (SeaweedFS/Local/S3)
//! ```
//!
//! ## Key Features
//! - **Zero Agent Privileges**: Agent containers use bind mounts (no CAP_SYS_ADMIN)
//! - **Rootless Compatible**: Works with both Docker and Podman rootless
//! - **Same Security Model**: All authorization via AegisFSAL (same as NFS transport)
//! - **Per-Volume Mountpoints**: Each volume gets an isolated FUSE mount
//!
//! ## Integration
//! - Shares the same `AegisFSAL` instance as the NFS transport
//! - Used by `ContainerStepRunner` and `ContainerRuntime` as an alternative to NFS
//! - Mount lifecycle tied to container lifecycle via `FuseMountHandle`
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** FUSE transport frontend for the FSAL domain entity

pub mod daemon;
mod inode_table;
