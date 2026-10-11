// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Inode Table — bidirectional mapping between FUSE inodes and AegisFileHandles
//!
//! FUSE uses 64-bit inode numbers to identify files and directories. This table
//! provides a thread-safe bidirectional mapping between inode numbers and the
//! FSAL's `AegisFileHandle` + canonical path pairs.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Inode ↔ AegisFileHandle mapping for FUSE transport

use crate::domain::fsal::AegisFileHandle;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::debug;

/// Root inode number (FUSE convention: inode 1 is the root directory).
pub const ROOT_INODE: u64 = 1;

/// Thread-safe bidirectional mapping between FUSE inodes (u64) and AegisFileHandles.
///
/// Mirrors the `FileHandleTable` pattern used by the NFS server, adapted for
/// FUSE's inode-based addressing.
pub struct InodeTable {
    /// Counter for generating unique inode numbers.
    /// Starts at 2 because inode 1 is reserved for root.
    next_inode: AtomicU64,
    /// Forward mapping: inode → (AegisFileHandle, canonical_path)
    forward: RwLock<HashMap<u64, (AegisFileHandle, String)>>,
    /// Reverse mapping: path_hash → inode (for consistent lookup by handle identity)
    reverse: RwLock<HashMap<u64, u64>>,
}

impl InodeTable {
    /// Create a new empty inode table.
    pub fn new() -> Self {
        Self {
            next_inode: AtomicU64::new(2), // Start at 2 (1 is reserved for root)
            forward: RwLock::new(HashMap::new()),
            reverse: RwLock::new(HashMap::new()),
        }
    }

    /// Allocate an inode for the given handle and path, or return an existing
    /// inode if this handle (identified by `path_hash`) was already registered.
    pub fn allocate(&self, handle: AegisFileHandle, path: String) -> u64 {
        // Check if handle already registered via path_hash
        let reverse = self.reverse.read();
        if let Some(&existing_inode) = reverse.get(&handle.path_hash) {
            return existing_inode;
        }
        drop(reverse);

        // Allocate new inode
        let inode = self.next_inode.fetch_add(1, Ordering::SeqCst);

        // Store bidirectional mapping
        self.forward
            .write()
            .insert(inode, (handle.clone(), path.clone()));
        self.reverse.write().insert(handle.path_hash, inode);

        debug!(inode = inode, path = %path, "Allocated FUSE inode");
        inode
    }

    /// Look up the AegisFileHandle and canonical path for an inode.
    pub fn lookup(&self, inode: u64) -> Option<(AegisFileHandle, String)> {
        self.forward.read().get(&inode).cloned()
    }

    /// Re-key the inodes of `from`, and of every path under it, to the same
    /// place under `to`, after the file system has moved `from` there.
    ///
    /// The kernel keeps a moved entry's inode under its new name, so the
    /// inode must name the new path from then on: git writes `config.lock`
    /// and renames it over `config`, then reads `config` through the inode
    /// `config.lock` was created with. An inode `to` held before is
    /// forgotten, as the file the move replaced. `handle_for` gives the
    /// handle a path is allocated with.
    pub fn rename(&self, from: &str, to: &str, handle_for: impl Fn(&str) -> AegisFileHandle) {
        self.forget(to);

        let mut forward = self.forward.write();
        let mut reverse = self.reverse.write();
        for (inode, (handle, path)) in forward.iter_mut() {
            let Some(moved) = moved_path(path, from, to) else {
                continue;
            };
            reverse.remove(&handle.path_hash);
            *handle = handle_for(&moved);
            reverse.insert(handle.path_hash, *inode);
            debug!(inode = *inode, from = %path, to = %moved, "Re-keyed FUSE inode");
            *path = moved;
        }
    }

    /// Forget the inodes of `path` and of every path under it, after the file
    /// system has removed it: a file made there again is a new file, with an
    /// inode of its own.
    pub fn forget(&self, path: &str) {
        let mut forward = self.forward.write();
        let mut reverse = self.reverse.write();
        forward.retain(|inode, (handle, held)| {
            if moved_path(held, path, path).is_none() {
                return true;
            }
            reverse.remove(&handle.path_hash);
            debug!(inode = *inode, path = %held, "Forgot FUSE inode");
            false
        });
    }

    /// Reverse lookup: find the inode for a given path hash, if previously registered.
    #[expect(
        dead_code,
        reason = "public API reserved for future inode reuse optimisation"
    )]
    pub fn find_by_path_hash(&self, path_hash: u64) -> Option<u64> {
        self.reverse.read().get(&path_hash).copied()
    }
}

/// Where `path` is once `from` is moved to `to`: `to` for `from` itself, the
/// same place under `to` for a path under `from`, and `None` for any other.
fn moved_path(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        return Some(to.to_string());
    }
    let rest = path.strip_prefix(from.trim_end_matches('/'))?;
    rest.starts_with('/')
        .then(|| format!("{}{rest}", to.trim_end_matches('/')))
}
