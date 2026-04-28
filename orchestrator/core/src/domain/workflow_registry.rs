// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # WorkflowRegistry Aggregate (BC-8 — ADR-021)
//!
//! Owns the `source_name → WorkflowId` direct routing table (deterministic path)
//! and an optional reference to the RouterAgent used for LLM-based fallback
//! classification.
//!
//! ## Invariants
//!
//! - Route keys are stored lowercase; lookups are case-insensitive.
//! - Route keys must be non-empty and contain no `/` or whitespace.
//! - `confidence_threshold` must be in `[0.0, 1.0]` (default: `0.7`).
//! - `router_agent_id`, if Some, must reference a valid deployed agent.
//!
//! ## DDD Placement
//!
//! `WorkflowRegistry` is the **Aggregate Root** for BC-8. External code holds
//! a reference to it (wrapped in `Arc<RwLock<WorkflowRegistry>>`); it is never
//! directly embedded inside other aggregates.
//!
//! This is **separate** from `WorkflowRepository` (which owns persistence of
//! `Workflow` manifests). `WorkflowRegistry` is a pure in-memory routing index.

use crate::domain::agent::AgentId;
use crate::domain::shared_kernel::TenantId;
use crate::domain::workflow::WorkflowId;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ──────────────────────────────────────────────────────────────────────────────
// WorkflowRegistryId
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowRegistryId(pub Uuid);

impl WorkflowRegistryId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WorkflowRegistryId {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// WorkflowRegistry — Aggregate Root
// ──────────────────────────────────────────────────────────────────────────────

/// Aggregate Root for BC-8 (Stimulus-Response Context).
///
/// Holds:
/// - A direct routing table: `source_name → WorkflowId` (Stage 1)
/// - An optional RouterAgent reference for LLM classification (Stage 2)
/// - The confidence threshold applied to LLM classification results
pub struct WorkflowRegistry {
    pub id: WorkflowRegistryId,
    /// Direct routing table. Keys are `(TenantId, lowercase_source_name)` to prevent
    /// source_name collisions across tenants (gap 056-13).
    routes: HashMap<(TenantId, String), WorkflowId>,
    /// RouterAgent invoked when no direct route matches.
    /// A `None` value means Stage 2 is unavailable; stimuli with no direct route
    /// are rejected with `StimulusError::NoRouterConfigured`.
    router_agent_id: Option<AgentId>,
    /// LLM classification confidence threshold.
    /// Stimuli classified below this value are rejected with 422.
    /// Default: 0.7.
    confidence_threshold: f64,
}

impl WorkflowRegistry {
    /// Create a new empty registry. Optionally wire in a RouterAgent for LLM fallback.
    pub fn new(router_agent_id: Option<AgentId>) -> Self {
        Self {
            id: WorkflowRegistryId::new(),
            routes: HashMap::new(),
            router_agent_id,
            confidence_threshold: 0.7,
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────────

    /// Register a direct `(tenant_id, source_name) → WorkflowId` route.
    ///
    /// Route keys are scoped to a tenant so the same `source_name` can be
    /// registered independently by different tenants without collision (gap 056-13).
    ///
    /// # Errors
    /// Returns `Err` if the source_name key is empty, contains `/`, or contains whitespace.
    pub fn register_route(
        &mut self,
        tenant_id: &TenantId,
        source_name: &str,
        workflow_id: WorkflowId,
    ) -> Result<()> {
        let key = Self::validate_key(source_name)?;
        self.routes.insert((tenant_id.clone(), key), workflow_id);
        Ok(())
    }

    /// Remove a direct route for a specific tenant. Returns `true` if a route was removed.
    pub fn remove_route(&mut self, tenant_id: &TenantId, source_name: &str) -> bool {
        self.routes
            .remove(&(tenant_id.clone(), source_name.to_lowercase()))
            .is_some()
    }

    /// Replace the RouterAgent used for LLM fallback classification.
    pub fn set_router_agent(&mut self, agent_id: Option<AgentId>) {
        self.router_agent_id = agent_id;
    }

    /// Set the minimum confidence required to accept an LLM classification.
    ///
    /// # Errors
    /// Returns `Err` if `threshold` is outside `[0.0, 1.0]`.
    pub fn set_confidence_threshold(&mut self, threshold: f64) -> Result<()> {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(anyhow!(
                "confidence_threshold must be in [0.0, 1.0], got {threshold}"
            ));
        }
        self.confidence_threshold = threshold;
        Ok(())
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    /// Stage 1: Deterministic lookup by tenant and source name.
    ///
    /// Returns `Some(WorkflowId)` if a direct route is registered for the given
    /// tenant and source name, `None` otherwise. Lookup is case-insensitive.
    pub fn lookup_direct(&self, tenant_id: &TenantId, source_name: &str) -> Option<WorkflowId> {
        self.routes
            .get(&(tenant_id.clone(), source_name.to_lowercase()))
            .copied()
    }

    /// The RouterAgent to invoke in Stage 2 if no direct route matched.
    pub fn router_agent(&self) -> Option<AgentId> {
        self.router_agent_id
    }

    /// The LLM confidence threshold (default `0.7`).
    pub fn confidence_threshold(&self) -> f64 {
        self.confidence_threshold
    }

    /// Number of registered direct routes.
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Find every `(tenant_id, workflow_id)` pair that has registered the
    /// given `source_name`.
    ///
    /// Used by the unauthenticated webhook ingestion path to discover the
    /// owning tenant of an incoming stimulus without trusting caller-supplied
    /// tenant headers. Tenant-scoped registrations are the source of truth
    /// (ADR-097 / ADR-021): if exactly one tenant has registered a given
    /// `source_name`, it owns the webhook; if multiple have, the webhook
    /// route is ambiguous and MUST be rejected; if zero have, there is no
    /// route and the request MUST be rejected.
    pub fn find_routes_by_source(&self, source_name: &str) -> Vec<(TenantId, WorkflowId)> {
        let needle = source_name.to_lowercase();
        self.routes
            .iter()
            .filter(|((_, src), _)| src == &needle)
            .map(|((tenant, _), wf)| (tenant.clone(), *wf))
            .collect()
    }

    /// Returns all registered source names for a tenant (sorted for deterministic output).
    pub fn registered_sources(&self, tenant_id: &TenantId) -> Vec<String> {
        let mut keys: Vec<String> = self
            .routes
            .keys()
            .filter(|(tid, _)| tid == tenant_id)
            .map(|(_, src)| src.clone())
            .collect();
        keys.sort();
        keys
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn validate_key(key: &str) -> Result<String> {
        let lowered = key.to_lowercase();
        if lowered.is_empty() {
            return Err(anyhow!("Route key must not be empty"));
        }
        if lowered.contains('/') {
            return Err(anyhow!("Route key '{key}' must not contain '/'"));
        }
        if lowered.contains(char::is_whitespace) {
            return Err(anyhow!("Route key '{key}' must not contain whitespace"));
        }
        Ok(lowered)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_workflow_id() -> WorkflowId {
        WorkflowId(Uuid::new_v4())
    }

    fn tenant_a() -> TenantId {
        TenantId::from_realm_slug("tenant-acme").unwrap()
    }

    fn tenant_b() -> TenantId {
        TenantId::from_realm_slug("tenant-globex").unwrap()
    }

    #[test]
    fn register_and_lookup_direct() {
        let mut reg = WorkflowRegistry::new(None);
        let wf = make_workflow_id();
        let tid = tenant_a();
        reg.register_route(&tid, "github", wf).unwrap();
        assert_eq!(reg.lookup_direct(&tid, "github"), Some(wf));
        assert_eq!(reg.lookup_direct(&tid, "GitHub"), Some(wf)); // case-insensitive
    }

    #[test]
    fn lookup_missing_returns_none() {
        let reg = WorkflowRegistry::new(None);
        assert!(reg.lookup_direct(&tenant_a(), "nonexistent").is_none());
    }

    #[test]
    fn remove_route() {
        let mut reg = WorkflowRegistry::new(None);
        let wf = make_workflow_id();
        let tid = tenant_a();
        reg.register_route(&tid, "stripe", wf).unwrap();
        assert!(reg.remove_route(&tid, "stripe"));
        assert!(reg.lookup_direct(&tid, "stripe").is_none());
    }

    #[test]
    fn invalid_key_slash() {
        let mut reg = WorkflowRegistry::new(None);
        let err = reg
            .register_route(&tenant_a(), "foo/bar", make_workflow_id())
            .unwrap_err();
        assert!(err.to_string().contains("'/'"));
    }

    #[test]
    fn invalid_key_empty() {
        let mut reg = WorkflowRegistry::new(None);
        let err = reg
            .register_route(&tenant_a(), "", make_workflow_id())
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn set_valid_threshold() {
        let mut reg = WorkflowRegistry::new(None);
        reg.set_confidence_threshold(0.85).unwrap();
        assert_eq!(reg.confidence_threshold(), 0.85);
    }

    #[test]
    fn set_invalid_threshold() {
        let mut reg = WorkflowRegistry::new(None);
        assert!(reg.set_confidence_threshold(1.5).is_err());
    }

    #[test]
    fn registered_sources_sorted() {
        let mut reg = WorkflowRegistry::new(None);
        let tid = tenant_a();
        reg.register_route(&tid, "stripe", make_workflow_id())
            .unwrap();
        reg.register_route(&tid, "github", make_workflow_id())
            .unwrap();
        reg.register_route(&tid, "aws-sns", make_workflow_id())
            .unwrap();
        assert_eq!(
            reg.registered_sources(&tid),
            vec!["aws-sns", "github", "stripe"]
        );
    }

    #[test]
    fn tenant_isolation_no_cross_tenant_collision() {
        let mut reg = WorkflowRegistry::new(None);
        let wf_a = make_workflow_id();
        let wf_b = make_workflow_id();
        let tid_a = tenant_a();
        let tid_b = tenant_b();

        reg.register_route(&tid_a, "deploy", wf_a).unwrap();
        reg.register_route(&tid_b, "deploy", wf_b).unwrap();

        // Each tenant gets its own route — no collision.
        assert_eq!(reg.lookup_direct(&tid_a, "deploy"), Some(wf_a));
        assert_eq!(reg.lookup_direct(&tid_b, "deploy"), Some(wf_b));
        assert_ne!(wf_a, wf_b);
    }
}
