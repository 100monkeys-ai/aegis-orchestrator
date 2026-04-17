// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Shared SSH Key Materialization Helper (BC-7 Storage Gateway, ADR-081)
//!
//! Both `git_clone_executor::blocking_clone` / `blocking_fetch_and_checkout`
//! (read path) and `git_repo_service::blocking_push` (write path) need to
//! hand libgit2 an on-disk SSH private key via `Cred::ssh_key`. The
//! Keymaster Pattern (ADR-034) requires that key material is:
//!
//! 1. Written to a mode-`0600` tempfile before libgit2 reads it.
//! 2. Zero-filled and removed from disk as soon as the git operation
//!    returns — on success, failure, *and* panic paths.
//! 3. Never logged, never returned to callers, never serialized.
//!
//! This module owns that pattern once so the clone and push code paths
//! can't drift. Duplicating the zeroize-on-drop logic between them was
//! the landmine that motivated the extraction.
//!
//! ## Usage
//!
//! ```ignore
//! use git2::RemoteCallbacks;
//! let mut callbacks = RemoteCallbacks::new();
//! // Keep `guard` alive for the entire lifetime of the libgit2 call —
//! // dropping it zeros + removes the tempfile.
//! let guard = attach_ssh_credentials(
//!     &mut callbacks,
//!     private_key_pem,
//!     passphrase.as_deref(),
//! )?;
//! // ... perform clone / fetch / push with `callbacks` ...
//! drop(guard);
//! ```
//!
//! The guard is returned rather than absorbed into the closure because
//! libgit2's `RemoteCallbacks` borrows the closure for `'cb` and we
//! cannot tie the tempfile's lifetime to an opaque callback without
//! gymnastics. The explicit guard makes the "key is live for exactly
//! this scope" contract visible to reviewers.

use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use git2::{Cred, RemoteCallbacks};

/// Dropper guard owning an on-disk SSH key tempfile. On drop it zero-
/// fills the file and removes it — regardless of whether the libgit2
/// operation succeeded or panicked.
///
/// Public within the crate so callers can hold it for the duration of
/// their libgit2 operation; the field is private so nothing outside this
/// module can peek at the materialised key path.
pub(crate) struct SshKeyTempFile {
    path: PathBuf,
    key_len: usize,
}

impl SshKeyTempFile {
    /// Path of the tempfile. Only used by tests — production code should
    /// never read the file contents back.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for SshKeyTempFile {
    fn drop(&mut self) {
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            use std::io::Write;
            let zeros = vec![0u8; self.key_len];
            let _ = f.write_all(&zeros);
            let _ = f.sync_all();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Materialize `private_key_pem` into a mode-`0600` tempfile under the
/// system temp dir (path pattern: `aegis-git-<uuid>`), attach a libgit2
/// credentials callback to `callbacks` that hands back
/// `Cred::ssh_key(username_from_url.unwrap_or("git"), None, &path,
/// passphrase)`, and return the drop-guard that will zero + remove the
/// tempfile on scope exit.
///
/// Callers MUST keep the returned guard alive for the entire duration of
/// the libgit2 operation. Dropping the guard before libgit2 has finished
/// reading the key will cause an authentication failure.
pub(crate) fn attach_ssh_credentials<'cb>(
    callbacks: &mut RemoteCallbacks<'cb>,
    private_key_pem: &str,
    passphrase: Option<&str>,
) -> io::Result<SshKeyTempFile> {
    let tmp_uuid = uuid::Uuid::new_v4();
    let key_path = std::env::temp_dir().join(format!("aegis-git-{tmp_uuid}"));
    let key_len = private_key_pem.len();

    {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&key_path)?;
        f.write_all(private_key_pem.as_bytes())?;
        f.sync_all()?;
    }
    // Belt-and-braces: some filesystems ignore `mode()` on create; re-
    // assert 0600 explicitly before libgit2 opens it.
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;

    let guard = SshKeyTempFile {
        path: key_path.clone(),
        key_len,
    };

    let passphrase_string = passphrase.map(|p| p.to_string());
    let key_path_cb = key_path;
    callbacks.credentials(move |_url, username_from_url, _allowed| {
        Cred::ssh_key(
            username_from_url.unwrap_or("git"),
            None,
            &key_path_cb,
            passphrase_string.as_deref(),
        )
    });

    Ok(guard)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const FAKE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
                            AAAA-not-actually-a-key-just-test-bytes\n\
                            -----END OPENSSH PRIVATE KEY-----\n";

    #[test]
    fn tempfile_is_mode_0600() {
        let mut cb = RemoteCallbacks::new();
        let guard = attach_ssh_credentials(&mut cb, FAKE_KEY, None).unwrap();

        let meta = std::fs::metadata(guard.path()).expect("tempfile must exist");
        // Low 9 bits are rwxrwxrwx; we require rw------- = 0o600.
        assert_eq!(meta.mode() & 0o777, 0o600, "key tempfile must be mode 0600");
    }

    #[test]
    fn tempfile_contains_key_material_while_guard_alive() {
        let mut cb = RemoteCallbacks::new();
        let guard = attach_ssh_credentials(&mut cb, FAKE_KEY, None).unwrap();

        let contents = std::fs::read_to_string(guard.path()).expect("tempfile readable");
        assert_eq!(contents, FAKE_KEY);
    }

    #[test]
    fn tempfile_removed_after_guard_drops() {
        let path = {
            let mut cb = RemoteCallbacks::new();
            let guard = attach_ssh_credentials(&mut cb, FAKE_KEY, None).unwrap();
            guard.path().to_path_buf()
            // guard drops here
        };
        assert!(
            !path.exists(),
            "tempfile must be removed when the guard drops"
        );
    }

    #[test]
    fn tempfile_path_is_under_system_temp_dir() {
        let mut cb = RemoteCallbacks::new();
        let guard = attach_ssh_credentials(&mut cb, FAKE_KEY, None).unwrap();
        let path = guard.path();
        assert!(
            path.starts_with(std::env::temp_dir()),
            "tempfile must live under std::env::temp_dir()"
        );
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("aegis-git-"),
            "tempfile name must follow aegis-git-<uuid> pattern, got {name}"
        );
    }

    #[test]
    fn passphrase_threaded_through_without_panic() {
        // We can't actually exercise the callback without a live libgit2
        // transport, but we can construct it with and without a
        // passphrase and confirm `attach_ssh_credentials` succeeds both
        // ways. This pins that a `Some(passphrase)` input doesn't blow
        // up on tempfile creation or closure capture.
        let mut cb1 = RemoteCallbacks::new();
        let _g1 = attach_ssh_credentials(&mut cb1, FAKE_KEY, None).unwrap();

        let mut cb2 = RemoteCallbacks::new();
        let _g2 = attach_ssh_credentials(&mut cb2, FAKE_KEY, Some("hunter2")).unwrap();
    }

    #[test]
    fn each_call_creates_a_distinct_tempfile() {
        let mut cb1 = RemoteCallbacks::new();
        let g1 = attach_ssh_credentials(&mut cb1, FAKE_KEY, None).unwrap();
        let mut cb2 = RemoteCallbacks::new();
        let g2 = attach_ssh_credentials(&mut cb2, FAKE_KEY, None).unwrap();
        assert_ne!(
            g1.path(),
            g2.path(),
            "each materialisation must use a fresh uuid-scoped path"
        );
    }
}
