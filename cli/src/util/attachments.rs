// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! CLI helpers for parsing and resolving the `--attachments` /
//! `--attachment` flags on `aegis task execute` and `aegis agent run`
//! (ADR-113, Surface 1).
//!
//! Two flag shapes are supported, mutually exclusive at the clap layer:
//!
//! * `--attachments <json-or-@file>` — precise form. Mirrors the existing
//!   `--input` parser: accepts either a JSON array literal or a `@path.json`
//!   reference. The parsed value is `Vec<AttachmentRef>`.
//!
//! * `--attachment <volume_id:path>` — convenient shorthand, repeatable.
//!   Each invocation resolves to a full `AttachmentRef` by issuing a
//!   `GET /v1/volumes/{id}/files/stat` against the orchestrator daemon and
//!   joining the returned metadata with the user-supplied `(volume_id, path)`.
//!
//! Path-traversal characters (`..`), control characters, and `\` separators
//! are rejected at parse time so a hostile shorthand never reaches the daemon.

use anyhow::{Context, Result};
use uuid::Uuid;

use aegis_orchestrator_core::domain::execution::AttachmentRef;
use aegis_orchestrator_core::domain::volume::VolumeId;

use crate::daemon::DaemonClient;

/// Parse a single `--attachment volume_id:path` shorthand into its
/// `(VolumeId, path)` parts. Splits on the first `:` so paths may contain
/// further colons (e.g. on Windows-mounted shares passed through unchanged
/// as POSIX paths). Rejects path-traversal and other unsafe characters at
/// the edge — matches the daemon's `validate_dest_path` policy so the same
/// rules apply on both sides.
pub fn parse_attachment_shorthand(raw: &str) -> Result<(VolumeId, String)> {
    let (volume_str, path) = raw.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("--attachment must be of the form 'volume_id:path' (got '{raw}')")
    })?;

    if volume_str.is_empty() {
        anyhow::bail!("--attachment volume_id is empty in '{raw}'");
    }
    if path.is_empty() {
        anyhow::bail!("--attachment path is empty in '{raw}'");
    }

    let volume_uuid = Uuid::parse_str(volume_str)
        .with_context(|| format!("--attachment volume_id '{volume_str}' is not a valid UUID"))?;

    // Path-traversal hardening: reject characters / segments that the
    // orchestrator's path sanitizer would otherwise have to scrub.
    if path.contains('\\') {
        anyhow::bail!("--attachment path may not contain '\\' (got '{path}')");
    }
    if path.chars().any(|c| c.is_control()) {
        anyhow::bail!("--attachment path may not contain control characters");
    }
    if path.split('/').any(|segment| segment == "..") {
        anyhow::bail!("--attachment path may not contain '..' segments (got '{path}')");
    }

    Ok((VolumeId(volume_uuid), path.to_string()))
}

/// Parse a `--attachments` payload — either a JSON array literal or a
/// `@path.json` reference — into `Vec<AttachmentRef>`. Mirrors the
/// `parse_input` shape used by `--input`: both inline JSON and file
/// references must yield the same Rust value.
pub async fn parse_attachments_json(raw: &str) -> Result<Vec<AttachmentRef>> {
    let body = if let Some(path) = raw.strip_prefix('@') {
        tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read attachments file: {path}"))?
    } else {
        raw.to_string()
    };

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str::<Vec<AttachmentRef>>(trimmed)
        .context("--attachments must be a JSON array of AttachmentRef objects")
}

/// Resolve a list of `(volume_id, path)` shorthand pairs into full
/// `AttachmentRef` records by stat-ing each file via the daemon's
/// `GET /v1/volumes/{id}/files/stat` endpoint. Stats are issued sequentially
/// to keep error reporting deterministic; the per-stat call is fast enough
/// that parallelism would obscure rather than help when one path is wrong.
pub async fn resolve_attachment_refs(
    client: &DaemonClient,
    refs: Vec<(VolumeId, String)>,
) -> Result<Vec<AttachmentRef>> {
    let mut out = Vec::with_capacity(refs.len());
    for (volume_id, path) in refs {
        let stat = client
            .stat_attachment_file(volume_id.0, &path)
            .await
            .with_context(|| format!("Failed to stat attachment '{}:{}'", volume_id.0, path))?;
        out.push(AttachmentRef {
            volume_id,
            path,
            name: stat.name,
            mime_type: stat.mime_type,
            size: stat.size,
            sha256: stat.sha256,
        });
    }
    Ok(out)
}

/// Combine the `--attachments` and repeated `--attachment` flags into a
/// single `Vec<AttachmentRef>`. The two forms are mutually exclusive at the
/// clap layer (`conflicts_with`), so at most one branch fires per invocation.
pub async fn collect_attachments(
    client: &DaemonClient,
    attachments_json: Option<String>,
    shorthand: Vec<String>,
) -> Result<Vec<AttachmentRef>> {
    if let Some(raw) = attachments_json {
        return parse_attachments_json(&raw).await;
    }
    if shorthand.is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Vec<(VolumeId, String)> = shorthand
        .iter()
        .map(|s| parse_attachment_shorthand(s))
        .collect::<Result<_>>()?;
    resolve_attachment_refs(client, parsed).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_parses_volume_uuid_and_path() {
        let raw = "11111111-2222-3333-4444-555555555555:/foo/bar.pdf";
        let (vol, path) = parse_attachment_shorthand(raw).unwrap();
        assert_eq!(
            vol.0,
            Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap()
        );
        assert_eq!(path, "/foo/bar.pdf");
    }

    #[test]
    fn shorthand_rejects_missing_colon() {
        let err = parse_attachment_shorthand("not-a-shorthand").unwrap_err();
        assert!(err.to_string().contains("volume_id:path"));
    }

    #[test]
    fn shorthand_rejects_path_traversal() {
        let raw = "11111111-2222-3333-4444-555555555555:../etc/passwd";
        let err = parse_attachment_shorthand(raw).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn shorthand_rejects_backslash() {
        let raw = "11111111-2222-3333-4444-555555555555:foo\\bar";
        let err = parse_attachment_shorthand(raw).unwrap_err();
        assert!(err.to_string().contains('\\'.to_string().as_str()));
    }

    #[test]
    fn shorthand_rejects_control_chars() {
        let raw = "11111111-2222-3333-4444-555555555555:foo\nbar";
        let err = parse_attachment_shorthand(raw).unwrap_err();
        assert!(err.to_string().contains("control"));
    }

    #[test]
    fn shorthand_rejects_invalid_uuid() {
        let err = parse_attachment_shorthand("not-a-uuid:/foo").unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn shorthand_keeps_colon_in_path_after_first_split() {
        let raw = "11111111-2222-3333-4444-555555555555:/a:b:c";
        let (_, path) = parse_attachment_shorthand(raw).unwrap();
        assert_eq!(path, "/a:b:c");
    }

    #[tokio::test]
    async fn attachments_json_parses_inline_array() {
        let json = r#"[{"volume_id":"11111111-2222-3333-4444-555555555555","path":"/x.pdf","name":"x.pdf","mime_type":"application/pdf","size":42}]"#;
        let parsed = parse_attachments_json(json).await.unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "x.pdf");
        assert_eq!(parsed[0].size, 42);
    }

    #[tokio::test]
    async fn attachments_json_parses_file_reference() {
        let path = std::env::temp_dir().join(format!("aegis-attachments-{}.json", Uuid::new_v4()));
        let json = r#"[{"volume_id":"11111111-2222-3333-4444-555555555555","path":"/y.txt","name":"y.txt","mime_type":"text/plain","size":7}]"#;
        tokio::fs::write(&path, json).await.unwrap();

        let arg = format!("@{}", path.display());
        let parsed = parse_attachments_json(&arg).await.unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "y.txt");
        assert_eq!(parsed[0].mime_type, "text/plain");

        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn attachments_json_inline_and_file_yield_same_value() {
        let json = r#"[{"volume_id":"11111111-2222-3333-4444-555555555555","path":"/z.bin","name":"z.bin","mime_type":"application/octet-stream","size":99,"sha256":"deadbeef"}]"#;
        let inline = parse_attachments_json(json).await.unwrap();

        let path =
            std::env::temp_dir().join(format!("aegis-attachments-eq-{}.json", Uuid::new_v4()));
        tokio::fs::write(&path, json).await.unwrap();
        let from_file = parse_attachments_json(&format!("@{}", path.display()))
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(path).await;

        assert_eq!(inline, from_file);
    }
}
