// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! NFS FileHandle Serialization
//!
//! Provides bincode-based serialization for AegisFileHandle to fit within
//! NFSv3's 64-byte file handle limit.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for file handle

use crate::domain::fsal::AegisFileHandle;
use thiserror::Error;

/// FileHandle serialization errors
#[derive(Debug, Error)]
pub enum FileHandleError {
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("FileHandle too large: {size} bytes (max 64)")]
    TooLarge { size: usize },

    #[error("Deserialization error: {0}")]
    Deserialization(String),
}

/// Encode AegisFileHandle to bytes for NFS wire protocol
///
/// # Arguments
/// * `handle` - The Aegis file handle to encode
///
/// # Returns
/// * `Ok(Vec<u8>)` - Serialized handle (≤64 bytes)
/// * `Err(FileHandleError)` - Serialization failed or size exceeded
pub fn encode_file_handle(handle: &AegisFileHandle) -> Result<Vec<u8>, FileHandleError> {
    let bytes = bincode::serialize(handle)
        .map_err(|e| FileHandleError::Serialization(e.to_string()))?;
    
    if bytes.len() > 64 {
        return Err(FileHandleError::TooLarge { size: bytes.len() });
    }
    
    Ok(bytes)
}

/// Decode bytes to AegisFileHandle from NFS wire protocol
///
/// # Arguments
/// * `bytes` - Serialized file handle bytes
///
/// # Returns
/// * `Ok(AegisFileHandle)` - Decoded handle
/// * `Err(FileHandleError)` - Deserialization failed
pub fn decode_file_handle(bytes: &[u8]) -> Result<AegisFileHandle, FileHandleError> {
    bincode::deserialize(bytes)
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

    #[test]
    fn test_size_limit() {
        let handle = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/test.txt",
        );

        let bytes = encode_file_handle(&handle).unwrap();
        assert!(bytes.len() <= 64, "FileHandle exceeds 64-byte NFSv3 limit: {} bytes", bytes.len());
    }

    #[test]
    fn test_multiple_handles_different_bytes() {
        let handle1 = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/file1.txt",
        );

        let handle2 = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/file2.txt",
        );

        let bytes1 = encode_file_handle(&handle1).unwrap();
        let bytes2 = encode_file_handle(&handle2).unwrap();

        assert_ne!(bytes1, bytes2);
    }
}
