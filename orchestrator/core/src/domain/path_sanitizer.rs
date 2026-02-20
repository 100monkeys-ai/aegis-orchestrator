// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Path Sanitizer Domain Service
//!
//! Provides path canonicalization and traversal attack prevention per ADR-036.
//! This is a domain service (not infrastructure) because path validation is
//! a core business rule for security, not a technical concern.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements internal responsibilities for path sanitizer

use std::path::{PathBuf, Component};
use thiserror::Error;

/// Path sanitization errors
#[derive(Debug, Error)]
pub enum PathSanitizerError {
    #[error("Path traversal attempt detected: {0}")]
    PathTraversal(String),

    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("Path outside volume boundary: {0}")]
    OutsideBoundary(String),

    #[error("Path too long: {0}")]
    PathTooLong(String),
}

/// Path sanitizer domain service
///
/// Validates and canonicalizes file paths to prevent directory traversal
/// attacks and ensure paths stay within volume boundaries.
///
/// # Security Guarantees
/// - Rejects paths containing `..` components
/// - Normalizes separators (`\` → `/` on Windows)
/// - Ensures absolute paths start from volume root
/// - Prevents symlink attacks (future: requires storage backend support)
pub struct PathSanitizer {
    /// Maximum allowed path length (default: 4096)
    max_path_len: usize,
}

impl PathSanitizer {
    /// Create a new path sanitizer with default settings
    pub fn new() -> Self {
        Self {
            max_path_len: 4096,
        }
    }

    /// Create a path sanitizer with custom max length
    pub fn with_max_length(max_path_len: usize) -> Self {
        Self { max_path_len }
    }

    /// Canonicalize and validate a file path
    ///
    /// # Arguments
    /// * `path` - The path to sanitize (relative or absolute)
    /// * `volume_root` - Optional volume root to enforce boundary (e.g., "/workspace")
    ///
    /// # Returns
    /// * `Ok(PathBuf)` - Canonicalized, safe path
    /// * `Err(PathSanitizerError)` - Path is unsafe or invalid
    ///
    /// # Examples
    /// ```
    /// use aegis_core::domain::path_sanitizer::PathSanitizer;
    /// use std::path::PathBuf;
    ///
    /// let sanitizer = PathSanitizer::new();
    ///
    /// // Valid path
    /// let safe = sanitizer.canonicalize("/workspace/file.txt", Some("/workspace")).unwrap();
    /// assert_eq!(safe, PathBuf::from("/workspace/file.txt"));
    ///
    /// // Reject traversal
    /// let bad = sanitizer.canonicalize("/workspace/../etc/passwd", Some("/workspace"));
    /// assert!(bad.is_err());
    /// ```
    pub fn canonicalize(
        &self,
        path: &str,
        volume_root: Option<&str>,
    ) -> Result<PathBuf, PathSanitizerError> {
        // Check path length
        if path.len() > self.max_path_len {
            return Err(PathSanitizerError::PathTooLong(path.to_string()));
        }

        // Convert to Path
        let path_buf = PathBuf::from(path);

        // Check for path traversal components
        for component in path_buf.components() {
            if component == Component::ParentDir {
                tracing::warn!(
                    path = %path,
                    "Path traversal attempt detected: contains '..' component"
                );
                return Err(PathSanitizerError::PathTraversal(path.to_string()));
            }
        }

        // Normalize the path (remove redundant . and /)
        let mut normalized = PathBuf::new();
        for component in path_buf.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    normalized.push(component);
                }
                Component::CurDir => {
                    // Skip "." components
                }
                Component::Normal(part) => {
                    normalized.push(part);
                }
                Component::ParentDir => {
                    // Already rejected above, but double-check
                    return Err(PathSanitizerError::PathTraversal(path.to_string()));
                }
            }
        }

        // If volume root specified, ensure path is within boundary
        if let Some(root) = volume_root {
            let root_path = PathBuf::from(root);
            
            // Make absolute if root is absolute
            let absolute_path = if root_path.is_absolute() && !normalized.is_absolute() {
                root_path.join(&normalized)
            } else {
                normalized.clone()
            };

            // Check if path starts with root
            if !absolute_path.starts_with(&root_path) {
                tracing::warn!(
                    path = %path,
                    root = %root,
                    "Path outside volume boundary detected"
                );
                return Err(PathSanitizerError::OutsideBoundary(path.to_string()));
            }

            return Ok(absolute_path);
        }

        Ok(normalized)
    }

    /// Validate a path without canonicalizing (lightweight check)
    ///
    /// Useful for quick validation before full canonicalization.
    pub fn validate(&self, path: &str) -> Result<(), PathSanitizerError> {
        // Check length
        if path.len() > self.max_path_len {
            return Err(PathSanitizerError::PathTooLong(path.to_string()));
        }

        // Check for obvious traversal patterns
        if path.contains("..") {
            tracing::warn!(
                path = %path,
                "Lightweight validation detected path traversal pattern"
            );
            return Err(PathSanitizerError::PathTraversal(path.to_string()));
        }

        // Check for null bytes (security)
        if path.contains('\0') {
            tracing::warn!(
                path = %path,
                "Path contains null byte (potential security issue)"
            );
            return Err(PathSanitizerError::InvalidPath(
                "Path contains null byte".to_string(),
            ));
        }

        Ok(())
    }

    /// Extract the volume-relative path from an absolute path
    ///
    /// # Arguments
    /// * `absolute_path` - Absolute path (e.g., "/workspace/subdir/file.txt")
    /// * `volume_root` - Volume root (e.g., "/workspace")
    ///
    /// # Returns
    /// * `Ok(PathBuf)` - Relative path (e.g., "subdir/file.txt")
    /// * `Err(PathSanitizerError)` - Path is not under volume root
    pub fn strip_volume_root(
        &self,
        absolute_path: &str,
        volume_root: &str,
    ) -> Result<PathBuf, PathSanitizerError> {
        let abs = PathBuf::from(absolute_path);
        let root = PathBuf::from(volume_root);

        abs.strip_prefix(&root)
            .map(|p| p.to_path_buf())
            .map_err(|_| PathSanitizerError::OutsideBoundary(absolute_path.to_string()))
    }
}

impl Default for PathSanitizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_path() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.canonicalize("/workspace/file.txt", Some("/workspace"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/workspace/file.txt"));
    }

    #[test]
    fn test_reject_parent_dir() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.canonicalize("/workspace/../etc/passwd", Some("/workspace"));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PathSanitizerError::PathTraversal(_)));
    }

    #[test]
    fn test_reject_double_dot() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.canonicalize("../../../etc/passwd", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_current_dir() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.canonicalize("/workspace/./subdir/./file.txt", Some("/workspace"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/workspace/subdir/file.txt"));
    }

    #[test]
    fn test_path_too_long() {
        let sanitizer = PathSanitizer::with_max_length(10);
        let long_path = "/very/long/path/that/exceeds/limit";
        let result = sanitizer.canonicalize(long_path, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PathSanitizerError::PathTooLong(_)));
    }

    #[test]
    fn test_outside_boundary() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.canonicalize("/etc/passwd", Some("/workspace"));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PathSanitizerError::OutsideBoundary(_)));
    }

    #[test]
    fn test_validate_quick_check() {
        let sanitizer = PathSanitizer::new();
        
        assert!(sanitizer.validate("/workspace/file.txt").is_ok());
        assert!(sanitizer.validate("/workspace/../etc").is_err());
        assert!(sanitizer.validate("/path\0/with/null").is_err());
    }

    #[test]
    fn test_strip_volume_root() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.strip_volume_root("/workspace/subdir/file.txt", "/workspace");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("subdir/file.txt"));
    }

    #[test]
    fn test_strip_volume_root_outside() {
        let sanitizer = PathSanitizer::new();
        let result = sanitizer.strip_volume_root("/etc/passwd", "/workspace");
        assert!(result.is_err());
    }
}
