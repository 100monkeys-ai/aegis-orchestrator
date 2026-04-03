// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_runtime_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let registry = match &self.runtime_registry {
            Some(r) => r,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.runtime.list",
                    "error": "Runtime registry not configured on this node"
                })));
            }
        };

        let language_filter = args.get("language").and_then(|v| v.as_str());

        let languages: Vec<String> = match language_filter {
            Some(lang) => {
                // Return only the requested language; empty runtimes array if unknown (not an error).
                vec![lang.to_string()]
            }
            None => registry.supported_languages(),
        };

        let mut runtimes: Vec<serde_json::Value> = Vec::new();

        for lang in &languages {
            let versions = registry.supported_versions(lang);
            for version in versions {
                match registry.resolve_entry(lang, &version) {
                    Ok(entry) => {
                        runtimes.push(serde_json::json!({
                            "language": lang,
                            "version": version,
                            "image": entry.image,
                            "description": entry.metadata.description,
                            "deprecated": false
                        }));
                    }
                    Err(_) => {
                        // resolve_entry can only fail if the lang/version doesn't exist;
                        // since we got `version` from supported_versions, this is unreachable
                        // in practice but we skip silently rather than failing the entire response.
                    }
                }
            }
        }

        // Sort deterministically: language name ascending, then version segments numerically.
        runtimes.sort_by(|a, b| {
            let lang_a = a["language"].as_str().unwrap_or("");
            let lang_b = b["language"].as_str().unwrap_or("");
            let ver_a = a["version"].as_str().unwrap_or("");
            let ver_b = b["version"].as_str().unwrap_or("");

            let lang_cmp = lang_a.cmp(lang_b);
            if lang_cmp != std::cmp::Ordering::Equal {
                return lang_cmp;
            }

            // Numeric segment comparison so "3.9" < "3.10".
            let a_parts: Vec<u64> = ver_a.split('.').filter_map(|s| s.parse().ok()).collect();
            let b_parts: Vec<u64> = ver_b.split('.').filter_map(|s| s.parse().ok()).collect();
            a_parts.cmp(&b_parts)
        });

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.runtime.list",
            "runtimes": runtimes,
            "custom_runtime_supported": true
        })))
    }
}
