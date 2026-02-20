// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Context Attachment Loader
//!
//! This module provides infrastructure for loading context attachments
//! from various sources (text, files, directories, URLs) and preparing
//! them for injection into agent execution context.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Load and concatenate context from multiple sources
//! - **Integration:** Agent spec.context → Agent runtime environment
//!
//! # Supported Attachment Types
//!
//! - **Text**: Inline content  
//! - **File**: Local file path
//! - **Directory**: Recursive directory read
//! - **URL**: HTTP GET request
//!
//! # Usage
//!
//! ```ignore
//! use context_loader::ContextLoader;
//! use domain::agent::ContextItem;
//!
//! let loader = ContextLoader::new();
//! let attachments = vec![
//!     ContextItem::Text { content: "Instructions".to_string(), description: None },
//!     ContextItem::File { path: "/config/rules.json".to_string(), description: None },
//! ];
//!
//! let context = loader.load_attachments(&attachments).await?;
//! ```

use crate::domain::agent::ContextItem;
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

// ============================================================================
// Context Loader
// ============================================================================

pub struct ContextLoader {
    /// HTTP client for URL attachments
    client: Client,
    
    /// Maximum total context size (bytes)
    max_size: usize,
    
    /// Maximum file size (bytes)
    max_file_size: usize,
}

impl ContextLoader {
    /// Create new context loader with default limits
    ///
    /// - Max total size: 10 MB
    /// - Max single file size: 5 MB
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            max_size: 10 * 1024 * 1024, // 10 MB
            max_file_size: 5 * 1024 * 1024, // 5 MB
        }
    }
    
    /// Create loader with custom limits
    pub fn with_limits(max_size: usize, max_file_size: usize) -> Self {
        Self {
            client: Client::new(),
            max_size,
            max_file_size,
        }
    }
    
    /// Load all attachments and concatenate into single string
    ///
    /// Returns formatted context string with separators between attachments.
    pub async fn load_attachments(&self, attachments: &[ContextItem]) -> Result<String> {
        let mut parts = Vec::new();
        let mut total_size = 0;
        
        for attachment in attachments {
            let content = self.load_attachment(attachment).await
                .with_context(|| format!("Failed to load attachment: {:?}", attachment))?;
            
            total_size += content.len();
            if total_size > self.max_size {
                return Err(anyhow!(
                    "Total context size ({} bytes) exceeds limit ({} bytes)",
                    total_size,
                    self.max_size
                ));
            }
            
            // Format with description if available
            let description = self.get_description(attachment);
            let formatted = if let Some(desc) = description {
                format!("# {}\n\n{}\n\n---\n\n", desc, content)
            } else {
                format!("{}\n\n---\n\n", content)
            };
            
            parts.push(formatted);
        }
        
        Ok(parts.join(""))
    }
    
    /// Load a single attachment
    async fn load_attachment(&self, attachment: &ContextItem) -> Result<String> {
        match attachment {
            ContextItem::Text { content, .. } => Ok(content.clone()),
            ContextItem::File { path, .. } => self.load_file(Path::new(path)),
            ContextItem::Directory { path, .. } => self.load_directory(Path::new(path)),
            ContextItem::Url { url, .. } => self.load_url(url).await,
        }
    }
    
    /// Load content from a local file
    fn load_file(&self, path: &Path) -> Result<String> {
        if !path.exists() {
            return Err(anyhow!("File not found: {:?}", path));
        }
        
        let metadata = fs::metadata(path)
            .with_context(|| format!("Failed to get file metadata: {:?}", path))?;
        
        if metadata.len() > self.max_file_size as u64 {
            return Err(anyhow!(
                "File size ({} bytes) exceeds limit ({} bytes): {:?}",
                metadata.len(),
                self.max_file_size,
                path
            ));
        }
        
        fs::read_to_string(path)
            .with_context(|| format!("Failed to read file: {:?}", path))
    }
    
    /// Load and concatenate all files in a directory
    fn load_directory(&self, path: &Path) -> Result<String> {
        if !path.exists() || !path.is_dir() {
            return Err(anyhow!("Directory not found: {:?}", path));
        }
        
        let mut files = Vec::new();
        
        // Walk directory recursively
        for entry in WalkDir::new(path).follow_links(false) {
            let entry = entry.context("Failed to read directory entry")?;
            
            if entry.file_type().is_file() {
                let file_path = entry.path();
                
                // Skip hidden files and common ignorable files
                if self.should_skip_file(file_path) {
                    continue;
                }
                
                let content = self.load_file(file_path)
                    .with_context(|| format!("Failed to load file in directory: {:?}", file_path))?;
                
                let relative_path = file_path.strip_prefix(path)
                    .unwrap_or(file_path)
                    .to_string_lossy();
                
                files.push(format!("## File: {}\n\n{}\n\n", relative_path, content));
            }
        }
        
        if files.is_empty() {
            return Ok(String::new());
        }
        
        Ok(files.join(""))
    }
    
    /// Load content from a URL
    async fn load_url(&self, url: &str) -> Result<String> {
        let response = self.client
            .get(url)
            .header("User-Agent", "AEGIS/1.0")
            .send()
            .await
            .with_context(|| format!("Failed to fetch URL: {}", url))?;
        
        if !response.status().is_success() {
            return Err(anyhow!("HTTP {} fetching URL: {}", response.status(), url));
        }
        
        // Check content length if available
        if let Some(content_length) = response.content_length() {
            if content_length > self.max_file_size as u64 {
                return Err(anyhow!(
                    "URL content size ({} bytes) exceeds limit ({} bytes): {}",
                    content_length,
                    self.max_file_size,
                    url
                ));
            }
        }
        
        let text = response.text().await
            .with_context(|| format!("Failed to read URL content: {}", url))?;
        
        if text.len() > self.max_file_size {
            return Err(anyhow!(
                "URL content size ({} bytes) exceeds limit ({} bytes): {}",
                text.len(),
                self.max_file_size,
                url
            ));
        }
        
        Ok(text)
    }
    
    /// Get description from attachment
    fn get_description(&self, attachment: &ContextItem) -> Option<String> {
        match attachment {
            ContextItem::Text { description, .. } => description.clone(),
            ContextItem::File { description, .. } => description.clone(),
            ContextItem::Directory { description, .. } => description.clone(),
            ContextItem::Url { description, .. } => description.clone(),
        }
    }
    
    /// Check if file should be skipped
    fn should_skip_file(&self, path: &Path) -> bool {
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        
        // Skip hidden files
        if filename.starts_with('.') {
            return true;
        }
        
        // Skip common non-text files
        let skip_extensions = [
            ".pyc", ".so", ".dll", ".exe", ".bin",
            ".jpg", ".png", ".gif", ".ico", ".svg",
            ".mp4", ".mp3", ".wav", ".avi",
            ".zip", ".tar", ".gz", ".7z",
            ".db", ".sqlite", ".log"
        ];
        
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_with_dot = format!(".{}", ext);
            if skip_extensions.contains(&ext_with_dot.as_str()) {
                return true;
            }
        }
        
        false
    }
}

impl Default for ContextLoader {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    
    #[test]
    fn test_load_text_attachment() {
        let loader = ContextLoader::new();
        let attachments = vec![
            ContextItem::Text {
                content: "Test content".to_string(),
                description: Some("Test description".to_string()),
            }
        ];
        
        let result = tokio_test::block_on(loader.load_attachments(&attachments)).unwrap();
        
        assert!(result.contains("Test description"));
        assert!(result.contains("Test content"));
        assert!(result.contains("---"));
    }
    
    #[test]
    fn test_load_file_attachment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_file.txt");
        
        // Create test file
        fs::write(&file_path, "File content").unwrap();
        
        let loader = ContextLoader::new();
        let attachments = vec![
            ContextItem::File {
                path: file_path.to_string_lossy().to_string(),
                description: Some("Test file".to_string()),
            }
        ];
        
        let result = tokio_test::block_on(loader.load_attachments(&attachments)).unwrap();
        
        assert!(result.contains("Test file"));
        assert!(result.contains("File content"));
    }
    
    #[test]
    fn test_load_directory_attachment() {
        let temp_dir = tempfile::tempdir().unwrap();
        
        // Create test files
        fs::write(temp_dir.path().join("file1.txt"), "Content 1").unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "Content 2").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();
        fs::write(temp_dir.path().join("subdir").join("file3.txt"), "Content 3").unwrap();
        
        let loader = ContextLoader::new();
        let attachments = vec![
            ContextItem::Directory {
                path: temp_dir.path().to_string_lossy().to_string(),
                description: Some("Test directory".to_string()),
            }
        ];
        
        let result = tokio_test::block_on(loader.load_attachments(&attachments)).unwrap();
        
        assert!(result.contains("Content 1"));
        assert!(result.contains("Content 2"));
        assert!(result.contains("Content 3"));
        assert!(result.contains("file1.txt"));
    }
    
    #[test]
    fn test_file_size_limit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("large_file.txt");
        
        // Create file larger than limit
        let large_content = "x".repeat(6 * 1024 * 1024); // 6 MB
        fs::write(&file_path, large_content).unwrap();
        
        let loader = ContextLoader::with_limits(10 * 1024 * 1024, 5 * 1024 * 1024);
        let attachments = vec![
            ContextItem::File {
                path: file_path.to_string_lossy().to_string(),
                description: None,
            }
        ];
        
        let result = tokio_test::block_on(loader.load_attachments(&attachments));
        assert!(result.is_err(), "Expected an error but got Ok");
        let err = result.unwrap_err();
        let err_msg = format!("{:?}", err); // Use Debug format to see the full chain
        assert!(err_msg.contains("exceeds"), "Error message should contain 'exceeds', got: {}", err_msg);
    }
    
    #[test]
    fn test_skip_hidden_files() {
        let loader = ContextLoader::new();
        
        assert!(loader.should_skip_file(Path::new(".hidden")));
        assert!(!loader.should_skip_file(Path::new(".git/config"))); // config is not hidden, only .git is
        assert!(!loader.should_skip_file(Path::new("visible.txt")));
    }
    
    #[test]
    fn test_skip_binary_files() {
        let loader = ContextLoader::new();
        
        assert!(loader.should_skip_file(Path::new("image.png")));
        assert!(loader.should_skip_file(Path::new("binary.pyc")));
        assert!(loader.should_skip_file(Path::new("archive.zip")));
        assert!(!loader.should_skip_file(Path::new("script.py")));
    }
}
