// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Tool Handlers (ADR-075)
//!
//! MCP tool invocation handlers for `aegis.agent.search` and `aegis.workflow.search`.
//! These tools provide semantic search over deployed agents and workflows using
//! the `DiscoveryService` application port. Enterprise feature — returns a clear
//! error when discovery is not configured.

use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_agent_search_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let discovery = self.discovery_service.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(
                "Discovery service not configured — this is an enterprise feature. \
                 Configure spec.discovery in aegis-config.yaml to enable semantic search."
                    .into(),
            )
        })?;

        let tenant_id = Self::resolve_tenant_arg(args)?;

        let query_text = args.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.agent.search requires 'query' parameter".into(),
            )
        })?;

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
        let min_score = args
            .get("min_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.3);
        let include_templates = args
            .get("include_platform_templates")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let label_filters = args
            .get("labels")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let status_filter = args
            .get("status")
            .and_then(|v| v.as_str())
            .map(String::from);

        let query = crate::domain::discovery::DiscoveryQuery {
            query: query_text.to_string(),
            limit,
            min_score,
            label_filters,
            status_filter,
            include_platform_templates: include_templates,
        };

        // Derive tier from the caller's SecurityContext name.
        // Non-Zaru contexts (operators, service accounts) get Enterprise-level results.
        let tier = crate::domain::iam::ZaruTier::from_security_context_name(&security_context.name)
            .unwrap_or(crate::domain::iam::ZaruTier::Enterprise);

        let response = discovery
            .search_agents(&tenant_id, &tier, query)
            .await
            .map_err(|e| SealSessionError::InternalError(format!("Agent search failed: {e}")))?;

        let results: Vec<serde_json::Value> = response
            .results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "agent_id": r.resource_id,
                    "name": r.name,
                    "version": r.version,
                    "description": r.description,
                    "labels": r.labels,
                    "similarity_score": r.similarity_score,
                    "relevance_score": r.relevance_score,
                    "tenant_id": r.tenant_id,
                    "is_platform_template": r.is_platform_template,
                    "updated_at": r.updated_at.to_rfc3339(),
                    "input_schema": r.input_schema.as_ref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.agent.search",
            "search_mode": "semantic",
            "count": results.len(),
            "total_indexed": response.total_indexed,
            "query_time_ms": response.query_time_ms,
            "results": results,
        })))
    }

    pub(super) async fn invoke_aegis_workflow_search_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let discovery = self.discovery_service.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(
                "Discovery service not configured — this is an enterprise feature. \
                 Configure spec.discovery in aegis-config.yaml to enable semantic search."
                    .into(),
            )
        })?;

        let tenant_id = Self::resolve_tenant_arg(args)?;

        let query_text = args.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.workflow.search requires 'query' parameter".into(),
            )
        })?;

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
        let min_score = args
            .get("min_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.3);
        let include_templates = args
            .get("include_platform_templates")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let label_filters = args
            .get("labels")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Workflows do not have a status filter
        let query = crate::domain::discovery::DiscoveryQuery {
            query: query_text.to_string(),
            limit,
            min_score,
            label_filters,
            status_filter: None,
            include_platform_templates: include_templates,
        };

        // Derive tier from the caller's SecurityContext name.
        // Non-Zaru contexts (operators, service accounts) get Enterprise-level results.
        let tier = crate::domain::iam::ZaruTier::from_security_context_name(&security_context.name)
            .unwrap_or(crate::domain::iam::ZaruTier::Enterprise);

        let response = discovery
            .search_workflows(&tenant_id, &tier, query)
            .await
            .map_err(|e| SealSessionError::InternalError(format!("Workflow search failed: {e}")))?;

        let results: Vec<serde_json::Value> = response
            .results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "workflow_id": r.resource_id,
                    "name": r.name,
                    "version": r.version,
                    "description": r.description,
                    "labels": r.labels,
                    "similarity_score": r.similarity_score,
                    "relevance_score": r.relevance_score,
                    "tenant_id": r.tenant_id,
                    "is_platform_template": r.is_platform_template,
                    "updated_at": r.updated_at.to_rfc3339(),
                    "input_schema": r.input_schema.as_ref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.search",
            "search_mode": "semantic",
            "count": results.len(),
            "total_indexed": response.total_indexed,
            "query_time_ms": response.query_time_ms,
            "results": results,
        })))
    }
}
