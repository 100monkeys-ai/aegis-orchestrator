// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis.document.render` (AEGIS ADR-135 D1, D6, and its Update of
//! 2026-10-07, D1d to D1f): a document rendered by the built-in agent
//! `aegis-document-renderer-agent`, whose program is fixed, and answered as
//! the file it produced: its path, size and format, and the execution that
//! holds it.

use super::*;

/// The built-in agent whose program renders the document.
pub(crate) const DOCUMENT_RENDERER_AGENT: &str = "aegis-document-renderer-agent";

/// The formats the renderer writes, each with its file extension and the text
/// the file starts with, where it has one.
const DOCUMENT_FORMATS: [(&str, &str, Option<&str>); 4] = [
    ("pdf", ".pdf", Some("%PDF")),
    ("docx", ".docx", Some("PK")),
    ("html", ".html", Some("<!DOCTYPE html>")),
    ("md", ".md", None),
];

/// How long the call waits for the render before it answers with the
/// execution to wait on: inside the minute an MCP client waits.
const RENDER_WAIT: std::time::Duration = std::time::Duration::from_secs(40);

/// How often the call reads the execution while it waits.
const RENDER_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// The refusal of a format outside the four, the renderer program's own
/// sentence.
fn format_refusal(format: &str) -> String {
    format!("format must be one of pdf, docx, html, md; got '{format}'")
}

/// The file's name without its extension, by the renderer program's rule:
/// the last path segment of `filename` (else of `title`), a format extension
/// dropped, every run of characters outside `A-Za-z0-9_-` made one `-`,
/// trimmed of `-`, at most 100 characters; `document` when nothing is left.
pub(crate) fn document_stem(filename: Option<&str>, title: Option<&str>) -> String {
    let name = filename.or(title).unwrap_or("").replace('\\', "/");
    let mut name = name.rsplit('/').next().unwrap_or("").to_string();
    for (_, extension, _) in DOCUMENT_FORMATS {
        if name.to_lowercase().ends_with(extension) {
            name.truncate(name.len() - extension.len());
            break;
        }
    }
    let mut stem = String::new();
    let mut in_run = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            stem.push(c);
            in_run = false;
        } else if !in_run {
            stem.push('-');
            in_run = true;
        }
    }
    let stem: String = stem.trim_matches('-').chars().take(100).collect();
    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "document".to_string()
    } else {
        stem.to_string()
    }
}

impl ToolInvocationService {
    /// `aegis.document.render`: refuse what the renderer would refuse, start
    /// the renderer agent on the document with the file it must leave
    /// declared, wait for it within [`RENDER_WAIT`], and answer the produced
    /// file.
    pub(super) async fn invoke_aegis_document_render_tool(
        &self,
        args: &mut Value,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        const TOOL: &str = "aegis.document.render";
        let refused = |sentence: String| {
            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": TOOL,
                "error": sentence,
            })))
        };

        let content = match args.get("content") {
            Some(Value::String(content)) if !content.trim().is_empty() => content.clone(),
            _ => return refused("content is empty".to_string()),
        };
        let format = match args.get("format") {
            Some(Value::String(format)) => format.clone(),
            Some(other) => return refused(format_refusal(&other.to_string())),
            None => return refused(format_refusal("")),
        };
        let Some((_, extension, magic)) = DOCUMENT_FORMATS
            .iter()
            .find(|(name, _, _)| *name == format)
            .copied()
        else {
            return refused(format_refusal(&format));
        };
        let text = |key: &str| match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!("{key} must be text")),
        };
        let title = match text("title") {
            Ok(title) => title,
            Err(sentence) => return refused(sentence),
        };
        let filename = match text("filename") {
            Ok(filename) => filename,
            Err(sentence) => return refused(sentence),
        };

        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let stem = document_stem(filename.as_deref(), title.as_deref());
        let path = format!("/workspace/{stem}{extension}");

        // The file the renderer must leave, declared for this execution so
        // the supervisor reads it from the volume and records it as a
        // produced file (ADR-005 O1 to O3).
        let mut output = serde_json::json!({ "path": path, "min_bytes": 1 });
        if let Some(magic) = magic {
            output["magic"] = Value::String(magic.to_string());
        }
        let mut input = serde_json::json!({
            "content": content,
            "format": format,
            "filename": stem,
            // The date the document carries, so the same input at the same
            // time renders the same bytes.
            "created_at": chrono::Utc::now().timestamp().max(0),
            "tenant_id": tenant_id.to_string(),
            "outputs": [output],
        });
        if let Some(title) = &title {
            input["title"] = Value::String(title.clone());
        }

        let agent_id = match self
            .agent_lifecycle
            .lookup_agent_visible_for_tenant(&tenant_id, DOCUMENT_RENDERER_AGENT)
            .await
        {
            Ok(Some(id)) => id,
            _ => {
                return refused(format!(
                    "the document renderer agent '{DOCUMENT_RENDERER_AGENT}' is not deployed on this node"
                ))
            }
        };

        let execution_id = match self
            .execution_service
            .start_execution(
                agent_id,
                crate::domain::execution::ExecutionInput {
                    intent: None,
                    input,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                "aegis-system-agent-runtime".to_string(),
                caller_identity,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                let error = match e.downcast_ref::<crate::domain::execution::ExecutionError>() {
                    Some(refused @ crate::domain::execution::ExecutionError::Refused(_)) => {
                        refused.to_string()
                    }
                    _ => format!("Failed to start the document render: {e}"),
                };
                return refused(error);
            }
        };

        let deadline = std::time::Instant::now() + RENDER_WAIT;
        loop {
            let execution = match self
                .execution_service
                .get_execution_for_tenant(&tenant_id, execution_id)
                .await
            {
                Ok(execution) => execution,
                Err(e) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": TOOL,
                        "execution_id": execution_id.to_string(),
                        "error": format!("Failed to read the document render: {e}"),
                    })))
                }
            };
            use crate::domain::execution::ExecutionStatus;
            match execution.status {
                ExecutionStatus::Completed => {
                    let answer = match execution
                        .produced_files()
                        .iter()
                        .find(|file| file.path == path)
                    {
                        Some(file) => serde_json::json!({
                            "tool": TOOL,
                            "execution_id": execution_id.to_string(),
                            "path": file.path,
                            "size_bytes": file.size_bytes,
                            "format": format,
                        }),
                        None => serde_json::json!({
                            "tool": TOOL,
                            "execution_id": execution_id.to_string(),
                            "error": format!("the render completed without its file {path}"),
                        }),
                    };
                    return Ok(ToolInvocationResult::Direct(answer));
                }
                ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                    let reason = execution
                        .error
                        .clone()
                        .or_else(|| {
                            execution
                                .iterations()
                                .last()
                                .and_then(|i| i.error.as_ref().map(|e| format!("{e:?}")))
                        })
                        .unwrap_or_else(|| format!("{:?}", execution.status).to_lowercase());
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": TOOL,
                        "execution_id": execution_id.to_string(),
                        "error": format!("the document render ended without its file: {reason}"),
                    })));
                }
                _ if std::time::Instant::now() >= deadline => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": TOOL,
                        "execution_id": execution_id.to_string(),
                        "status": "running",
                        "path": path,
                        "format": format,
                        "message": "The document is still rendering; wait for its execution with aegis.task.wait, then read the file with aegis.execution.file.",
                    })));
                }
                _ => tokio::time::sleep(RENDER_POLL).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::document_stem;

    #[test]
    fn a_document_is_named_by_the_renderer_programs_rule() {
        let cases = [
            ((None, Some("Quarterly report")), "Quarterly-report"),
            ((Some("report.pdf"), Some("Ignored")), "report"),
            ((Some("../../etc/passwd"), None), "passwd"),
            ((Some("a.b  c!.DOCX"), None), "a-b-c"),
            ((None, Some("日本語")), "document"),
            ((None, None), "document"),
        ];
        let mut complaints = Vec::new();
        for ((filename, title), expected) in cases {
            let stem = document_stem(filename, title);
            if stem != expected {
                complaints.push(format!(
                    "{filename:?} {title:?} named {stem:?}, not {expected:?}"
                ));
            }
            if document_stem(Some(&stem), None) != stem {
                complaints.push(format!("{stem:?} is renamed by its own rule"));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
