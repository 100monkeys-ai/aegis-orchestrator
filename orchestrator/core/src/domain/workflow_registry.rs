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
    /// Direct routing table. Keys are stored lowercase.
    routes: HashMap<String, WorkflowId>,
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

    /// Register a direct `source_name → WorkflowId` route.
    ///
    /// # Errors
    /// Returns `Err` if the key is empty, contains `/`, or contains whitespace.
    pub fn register_route(&mut self, source_name: &str, workflow_id: WorkflowId) -> Result<()> {
        let key = Self::validate_key(source_name)?;
        self.routes.insert(key, workflow_id);
        Ok(())
    }

    /// Remove a direct route by source name. Returns `true` if a route was removed.
    pub fn remove_route(&mut self, source_name: &str) -> bool {
        self.routes.remove(&source_name.to_lowercase()).is_some()
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

    /// Stage 1: Deterministic lookup by source name.
    ///
    /// Returns `Some(WorkflowId)` if a direct route is registered, `None` otherwise.
    /// Lookup is case-insensitive.
    pub fn lookup_direct(&self, source_name: &str) -> Option<WorkflowId> {
        self.routes.get(&source_name.to_lowercase()).copied()
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

    /// Returns all registered source names (sorted for deterministic output).
    pub fn registered_sources(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.routes.keys().cloned().collect();
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

    #[test]
    fn register_and_lookup_direct() {
        let mut reg = WorkflowRegistry::new(None);
        let wf = make_workflow_id();
        reg.register_route("github", wf).unwrap();
        assert_eq!(reg.lookup_direct("github"), Some(wf));
        assert_eq!(reg.lookup_direct("GitHub"), Some(wf)); // case-insensitive
    }

    #[test]
    fn lookup_missing_returns_none() {
        let reg = WorkflowRegistry::new(None);
        assert!(reg.lookup_direct("nonexistent").is_none());
    }

    #[test]
    fn remove_route() {
        let mut reg = WorkflowRegistry::new(None);
        let wf = make_workflow_id();
        reg.register_route("stripe", wf).unwrap();
        assert!(reg.remove_route("stripe"));
        assert!(reg.lookup_direct("stripe").is_none());
    }

    #[test]
    fn invalid_key_slash() {
        let mut reg = WorkflowRegistry::new(None);
        let err = reg
            .register_route("foo/bar", make_workflow_id())
            .unwrap_err();
        assert!(err.to_string().contains("'/'"));
    }

    #[test]
    fn invalid_key_empty() {
        let mut reg = WorkflowRegistry::new(None);
        let err = reg.register_route("", make_workflow_id()).unwrap_err();
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
        reg.register_route("stripe", make_workflow_id()).unwrap();
        reg.register_route("github", make_workflow_id()).unwrap();
        reg.register_route("aws-sns", make_workflow_id()).unwrap();
        assert_eq!(
            reg.registered_sources(),
            vec!["aws-sns", "github", "stripe"]
        );
    }
}
