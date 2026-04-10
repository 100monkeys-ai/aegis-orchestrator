// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # NFS FileHandle Serialization (BC-7, ADR-036)
//!
//! Provides bincode-based encoding/decoding of [`AegisFileHandle`] so that it
//! fits within the **64-byte hard limit** imposed by the NFSv3 wire protocol
//! (RFC 1813 §2.3.2, kernel NFS client enforces this via `NFS3_FHSIZE = 64`).
//!
//! ## Wire Format
//!
//! ```text
//! AegisFileHandle {
//!   execution_context: HandleExecutionContext  →  4B tag + 16B UUID  = 20B
//!   volume_id: Uuid                            →  16B (fixed)
//!   path_hash: u64                             →  8B
//!   created_at: i64                            →  8B
//! }
//!   └── bincode_handle_options().serialize  →  52 bytes total   OK: 52 ≤ 64  ✓
//! ```
//!
//! `bincode_handle_options()` uses `with_fixint_encoding()` to eliminate the
//! 8-byte varint length prefixes that the default bincode encoding adds to
//! `uuid::Uuid` fields (via its serde seq impl), which would push the total to
//! 68 bytes — 4 bytes over the NFSv3 hard limit.
//!
//! The 64-byte constraint means we **cannot** store the full path in the handle —
//! only a hash. The server performs a hash→inode lookup in the FSAL layer to
//! reconstruct the full path from the hash.
//!
//! ## Security
//!
//! The `execution_id` field is verified on every NFS operation by `AegisFSAL`:
//! if the execution making the request does not own the `volume_id`, the
//! operation is rejected with `UnauthorizedVolumeAccess`.
//!
//! See ADR-036 §3.1 (FileHandle Design), AGENTS.md §FileHandle.
//!
//! - **Layer:** Infrastructure Layer
//! - **Bounded Context:** BC-7 Storage Gateway

use crate::domain::fsal::{bincode_handle_options, AegisFileHandle};
use bincode::Options;
use thiserror::Error;

/// Errors arising from NFS FileHandle serialization/deserialization.
#[derive(Debug, Error)]
pub enum FileHandleError {
    /// bincode serialization failed (should not happen in practice).
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Serialized handle exceeds the 64-byte NFSv3 hard limit.
    /// This indicates `AegisFileHandle` grew beyond its size budget; reduce
    /// the path hash or switch to a shorter encoding.
    #[error("FileHandle too large: {size} bytes (max 64)")]
    TooLarge { size: usize },

    /// bincode deserialization failed; the bytes received from the NFS client
    /// are corrupt or were not produced by [`encode_file_handle`].
    #[error("Deserialization error: {0}")]
    Deserialization(String),
}

/// Encode an [`AegisFileHandle`] to bytes for the NFS wire protocol.
///
/// Uses bincode for compact binary encoding. Returns an error if the resulting
/// byte slice would exceed the 64-byte NFSv3 hard limit enforced by
/// [`FileHandleError::TooLarge`].
///
/// # Arguments
///
/// * `handle` — The Aegis file handle to encode.
///
/// # Returns
///
/// `Ok(Vec<u8>)` of ≤ 64 bytes ready to be passed as `nfs_fh3` on the wire.
///
/// # Errors
///
/// - [`FileHandleError::Serialization`] — bincode encountered an unexpected type.
/// - [`FileHandleError::TooLarge`] — encoded size exceeds 64 bytes.
pub fn encode_file_handle(handle: &AegisFileHandle) -> Result<Vec<u8>, FileHandleError> {
    let bytes = bincode_handle_options()
        .serialize(handle)
        .map_err(|e| FileHandleError::Serialization(e.to_string()))?;

    if bytes.len() > 64 {
        return Err(FileHandleError::TooLarge { size: bytes.len() });
    }

    Ok(bytes)
}

/// Decode bytes received from the NFS client into an [`AegisFileHandle`].
///
/// # Arguments
///
/// * `bytes` — Raw `nfs_fh3.data` bytes from the NFS client.
///
/// # Errors
///
/// [`FileHandleError::Deserialization`] if the bytes are corrupt or the type
/// layout has changed since the handle was created.
pub fn decode_file_handle(bytes: &[u8]) -> Result<AegisFileHandle, FileHandleError> {
    bincode_handle_options()
        .deserialize(bytes)
        .map_err(|e| FileHandleError::Deserialization(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{execution::ExecutionId, volume::VolumeId};

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/test/file.txt",
        );

        let bytes = encode_file_handle(&original).unwrap();
        let decoded = decode_file_handle(&bytes).unwrap();

        assert_eq!(original, decoded);
    }

    /// Regression test: default bincode encoding adds 8-byte varint length prefixes to
    /// `uuid::Uuid` fields, producing 68 bytes — 4 bytes over the NFSv3 64-byte limit.
    /// `with_fixint_encoding()` eliminates those prefixes, yielding exactly 52 bytes:
    ///   enum tag (4B) + UUID (16B) + volume_id UUID (16B) + path_hash u64 (8B) + created_at i64 (8B)
    #[test]
    fn test_encode_file_handle_serializes_to_52_bytes() {
        let handle =
            AegisFileHandle::new(ExecutionId::new(), VolumeId::new(), "/workspace/test.txt");

        let bytes = encode_file_handle(&handle).unwrap();
        assert_eq!(
            bytes.len(),
            52,
            "encode_file_handle must produce exactly 52 bytes with fixint encoding; got {} bytes",
            bytes.len()
        );
    }

    /// Regression test: two separate `Option<Uuid>` fields serialized to 66 bytes
    /// via bincode, exceeding the 64-byte NFSv3 hard limit. The `HandleExecutionContext`
    /// enum (1-byte tag + 16-byte UUID) must keep both agent and workflow handles ≤ 64 bytes.
    #[test]
    fn test_size_limit_agent_handle() {
        let handle =
            AegisFileHandle::new(ExecutionId::new(), VolumeId::new(), "/workspace/test.txt");

        let bytes = encode_file_handle(&handle).unwrap();
        assert!(
            bytes.len() <= 64,
            "Agent FileHandle exceeds 64-byte NFSv3 limit: {} bytes",
            bytes.len()
        );
    }

    /// Regression test: workflow handles must also fit within the 64-byte NFSv3 limit.
    #[test]
    fn test_size_limit_workflow_handle() {
        let handle = AegisFileHandle::new_for_workflow(
            uuid::Uuid::new_v4(),
            VolumeId::new(),
            "/workspace/test.txt",
        );

        let bytes = encode_file_handle(&handle).unwrap();
        assert!(
            bytes.len() <= 64,
            "Workflow FileHandle exceeds 64-byte NFSv3 limit: {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn test_encode_decode_workflow_roundtrip() {
        let original = AegisFileHandle::new_for_workflow(
            uuid::Uuid::new_v4(),
            VolumeId::new(),
            "/workspace/container/out.txt",
        );

        let bytes = encode_file_handle(&original).unwrap();
        let decoded = decode_file_handle(&bytes).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_multiple_handles_different_bytes() {
        let handle1 =
            AegisFileHandle::new(ExecutionId::new(), VolumeId::new(), "/workspace/file1.txt");

        let handle2 =
            AegisFileHandle::new(ExecutionId::new(), VolumeId::new(), "/workspace/file2.txt");

        let bytes1 = encode_file_handle(&handle1).unwrap();
        let bytes2 = encode_file_handle(&handle2).unwrap();

        assert_ne!(bytes1, bytes2);
    }
}
