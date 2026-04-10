// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Domain-layer unit tests for BC-8 (Stimulus-Response) and BC-16 (Infrastructure & Hosting).
//!
//! Covers value objects, aggregate invariants, and serialization round-trips for:
//! - `Stimulus`, `StimulusSource`, `RoutingDecision`, `RejectionReason`
//! - `WorkflowRegistry` aggregate root
//! - `NodeCluster` aggregate, `NodePeer`, `NodeCapabilityAdvertisement`,
//!   `NodeSecurityToken`, `NodeChallenge`, `ConfigScope`, `RegisteredNode`,
//!   `ExecutionRoute`, `ClusterSummaryStatus`
//!
//! # Architecture
//!
//! - **Layer:** Domain
//! - **Purpose:** Validate business invariants for BC-8 and BC-16 domain types

use std::collections::HashMap;

use base64::Engine;
use chrono::{Duration, Utc};
use uuid::Uuid;

use aegis_orchestrator_core::domain::cluster::{
    ClusterSummaryStatus, ConfigScope, ExecutionRoute, NodeCapabilityAdvertisement, NodeChallenge,
    NodeCluster, NodeId, NodePeer, NodePeerStatus, NodeSecurityToken, NodeTokenClaims,
    RegisteredNode, RegistryStatus, ResourceSnapshot,
};
use aegis_orchestrator_core::domain::node_config::NodeRole;
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::domain::stimulus::{
    RejectionReason, RoutingDecision, RoutingMode, Stimulus, StimulusId, StimulusSource,
};
use aegis_orchestrator_core::domain::workflow::WorkflowId;
use aegis_orchestrator_core::domain::workflow_registry::WorkflowRegistry;

// ============================================================================
// Helpers
// ============================================================================

fn make_workflow_id() -> WorkflowId {
    WorkflowId(Uuid::new_v4())
}

fn make_node_id() -> NodeId {
    NodeId::new()
}

fn make_capabilities() -> NodeCapabilityAdvertisement {
    NodeCapabilityAdvertisement {
        gpu_count: 2,
        vram_gb: 16,
        cpu_cores: 8,
        available_memory_gb: 32,
        supported_runtimes: vec!["docker".to_string(), "firecracker".to_string()],
        tags: vec!["gpu".to_string()],
    }
}

fn make_peer(role: NodeRole) -> NodePeer {
    let now = Utc::now();
    NodePeer {
        node_id: make_node_id(),
        role,
        public_key: vec![1, 2, 3],
        capabilities: make_capabilities(),
        grpc_address: "https://worker-1:9090".to_string(),
        status: NodePeerStatus::Active,
        last_heartbeat_at: now,
        registered_at: now,
    }
}

/// Build a minimal JWT-shaped string with the given claims in the payload.
fn make_jwt(claims: &NodeTokenClaims) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"alg":"RS256","typ":"JWT"}"#.as_bytes());
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"fake-signature");
    format!("{header}.{payload}.{sig}")
}

fn make_token_claims(exp_offset_secs: i64) -> NodeTokenClaims {
    let now = Utc::now().timestamp();
    NodeTokenClaims {
        node_id: make_node_id(),
        role: NodeRole::Worker,
        capabilities_hash: "abc123".to_string(),
        iat: now,
        exp: now + exp_offset_secs,
    }
}

// ============================================================================
// BC-8 — Stimulus
// ============================================================================

#[test]
fn stimulus_new_sets_source_and_content() {
    let stim = Stimulus::new(
        StimulusSource::Webhook {
            source_name: "github".to_string(),
        },
        "push event payload",
    );
    assert_eq!(stim.content, "push event payload");
    assert!(matches!(stim.source, StimulusSource::Webhook { .. }));
    assert!(stim.idempotency_key.is_none());
    assert!(stim.headers.is_empty());
}

#[test]
fn stimulus_new_generates_unique_ids() {
    let a = Stimulus::new(StimulusSource::Stdin, "a");
    let b = Stimulus::new(StimulusSource::Stdin, "b");
    assert_ne!(a.id.0, b.id.0);
}

#[test]
fn stimulus_with_idempotency_key_non_empty() {
    let stim = Stimulus::new(StimulusSource::HttpApi, "hi").with_idempotency_key("req-abc-123");
    assert_eq!(stim.idempotency_key, Some("req-abc-123".to_string()));
}

#[test]
fn stimulus_with_idempotency_key_empty_stays_none() {
    let stim = Stimulus::new(StimulusSource::HttpApi, "hi").with_idempotency_key("");
    assert!(stim.idempotency_key.is_none());
}

#[test]
fn stimulus_with_headers() {
    let mut headers = HashMap::new();
    headers.insert("X-GitHub-Event".to_string(), "push".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    let stim = Stimulus::new(
        StimulusSource::Webhook {
            source_name: "github".to_string(),
        },
        "{}",
    )
    .with_headers(headers);
    assert_eq!(stim.headers.len(), 2);
    assert_eq!(stim.headers.get("X-GitHub-Event").unwrap(), "push");
}

// ── StimulusSource variants & name() ────────────────────────────────────────

#[test]
fn stimulus_source_webhook_name() {
    let src = StimulusSource::Webhook {
        source_name: "Stripe".to_string(),
    };
    assert_eq!(src.name(), "Stripe");
}

#[test]
fn stimulus_source_http_api_name() {
    assert_eq!(StimulusSource::HttpApi.name(), "http_api");
}

#[test]
fn stimulus_source_stdin_name() {
    assert_eq!(StimulusSource::Stdin.name(), "stdin");
}

#[test]
fn stimulus_source_temporal_signal_name() {
    assert_eq!(StimulusSource::TemporalSignal.name(), "temporal_signal");
}

// ── RoutingDecision ─────────────────────────────────────────────────────────

#[test]
fn routing_decision_deterministic() {
    let wf = make_workflow_id();
    let decision = RoutingDecision {
        workflow_id: wf,
        confidence: 1.0,
        mode: RoutingMode::Deterministic,
    };
    assert_eq!(decision.workflow_id, wf);
    assert_eq!(decision.confidence, 1.0);
    assert!(matches!(decision.mode, RoutingMode::Deterministic));
}

#[test]
fn routing_decision_llm_classified() {
    let decision = RoutingDecision {
        workflow_id: make_workflow_id(),
        confidence: 0.82,
        mode: RoutingMode::LlmClassified,
    };
    assert!(matches!(decision.mode, RoutingMode::LlmClassified));
    assert!(decision.confidence >= 0.7 && decision.confidence <= 1.0);
}

// ── RejectionReason ─────────────────────────────────────────────────────────

#[test]
fn rejection_reason_low_confidence() {
    let reason = RejectionReason::LowConfidence {
        confidence: 0.45,
        threshold: 0.7,
    };
    match reason {
        RejectionReason::LowConfidence {
            confidence,
            threshold,
        } => {
            assert!(confidence < threshold);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn rejection_reason_no_router_configured() {
    let reason = RejectionReason::NoRouterConfigured {
        source: "slack".to_string(),
    };
    assert!(matches!(reason, RejectionReason::NoRouterConfigured { .. }));
}

#[test]
fn rejection_reason_hmac_invalid() {
    let reason = RejectionReason::HmacInvalid;
    assert!(matches!(reason, RejectionReason::HmacInvalid));
}

#[test]
fn rejection_reason_idempotent_duplicate() {
    let original = StimulusId::new();
    let reason = RejectionReason::IdempotentDuplicate {
        original_stimulus_id: original,
    };
    match reason {
        RejectionReason::IdempotentDuplicate {
            original_stimulus_id,
        } => {
            assert_eq!(original_stimulus_id, original);
        }
        _ => panic!("wrong variant"),
    }
}

// ============================================================================
// BC-8 — WorkflowRegistry
// ============================================================================

#[test]
fn registry_new_without_router_agent() {
    let reg = WorkflowRegistry::new(None);
    assert!(reg.router_agent().is_none());
    assert_eq!(reg.route_count(), 0);
    assert_eq!(reg.confidence_threshold(), 0.7);
}

#[test]
fn registry_new_with_router_agent() {
    use aegis_orchestrator_core::domain::shared_kernel::AgentId;
    let agent_id = AgentId::new();
    let reg = WorkflowRegistry::new(Some(agent_id));
    assert_eq!(reg.router_agent(), Some(agent_id));
}

#[test]
fn registry_register_route_case_insensitive() {
    let mut reg = WorkflowRegistry::new(None);
    let wf = make_workflow_id();
    reg.register_route(&TenantId::system(), "GitHub", wf)
        .unwrap();
    // Lookup with different casing
    assert_eq!(reg.lookup_direct(&TenantId::system(), "github"), Some(wf));
    assert_eq!(reg.lookup_direct(&TenantId::system(), "GITHUB"), Some(wf));
    assert_eq!(reg.lookup_direct(&TenantId::system(), "GitHub"), Some(wf));
}

#[test]
fn registry_register_route_rejects_empty_key() {
    let mut reg = WorkflowRegistry::new(None);
    assert!(reg
        .register_route(&TenantId::system(), "", make_workflow_id())
        .is_err());
}

#[test]
fn registry_register_route_rejects_slash() {
    let mut reg = WorkflowRegistry::new(None);
    let err = reg
        .register_route(&TenantId::system(), "foo/bar", make_workflow_id())
        .unwrap_err();
    assert!(err.to_string().contains("'/'"));
}

#[test]
fn registry_register_route_rejects_whitespace() {
    let mut reg = WorkflowRegistry::new(None);
    assert!(reg
        .register_route(&TenantId::system(), "foo bar", make_workflow_id())
        .is_err());
}

#[test]
fn registry_remove_route_returns_true_if_existed() {
    let mut reg = WorkflowRegistry::new(None);
    reg.register_route(&TenantId::system(), "stripe", make_workflow_id())
        .unwrap();
    assert!(reg.remove_route(&TenantId::system(), "stripe"));
    assert!(reg.lookup_direct(&TenantId::system(), "stripe").is_none());
}

#[test]
fn registry_remove_route_returns_false_if_absent() {
    let mut reg = WorkflowRegistry::new(None);
    assert!(!reg.remove_route(&TenantId::system(), "nonexistent"));
}

#[test]
fn registry_lookup_direct_returns_none_for_unknown() {
    let reg = WorkflowRegistry::new(None);
    assert!(reg.lookup_direct(&TenantId::system(), "unknown").is_none());
}

#[test]
fn registry_set_router_agent() {
    use aegis_orchestrator_core::domain::shared_kernel::AgentId;
    let mut reg = WorkflowRegistry::new(None);
    assert!(reg.router_agent().is_none());
    let agent_id = AgentId::new();
    reg.set_router_agent(Some(agent_id));
    assert_eq!(reg.router_agent(), Some(agent_id));
    reg.set_router_agent(None);
    assert!(reg.router_agent().is_none());
}

#[test]
fn registry_set_confidence_threshold_valid() {
    let mut reg = WorkflowRegistry::new(None);
    reg.set_confidence_threshold(0.0).unwrap();
    assert_eq!(reg.confidence_threshold(), 0.0);
    reg.set_confidence_threshold(1.0).unwrap();
    assert_eq!(reg.confidence_threshold(), 1.0);
    reg.set_confidence_threshold(0.85).unwrap();
    assert_eq!(reg.confidence_threshold(), 0.85);
}

#[test]
fn registry_set_confidence_threshold_rejects_out_of_range() {
    let mut reg = WorkflowRegistry::new(None);
    assert!(reg.set_confidence_threshold(-0.1).is_err());
    assert!(reg.set_confidence_threshold(1.01).is_err());
    assert!(reg.set_confidence_threshold(2.0).is_err());
}

#[test]
fn registry_registered_sources_sorted() {
    let mut reg = WorkflowRegistry::new(None);
    reg.register_route(&TenantId::system(), "zebra", make_workflow_id())
        .unwrap();
    reg.register_route(&TenantId::system(), "apple", make_workflow_id())
        .unwrap();
    reg.register_route(&TenantId::system(), "mango", make_workflow_id())
        .unwrap();
    assert_eq!(
        reg.registered_sources(&TenantId::system()),
        vec!["apple", "mango", "zebra"]
    );
}

#[test]
fn registry_registered_sources_empty() {
    let reg = WorkflowRegistry::new(None);
    assert!(reg.registered_sources(&TenantId::system()).is_empty());
}

// ============================================================================
// BC-16 — NodeCluster
// ============================================================================

#[test]
fn node_cluster_new() {
    let controller = make_node_id();
    let cluster = NodeCluster::new(controller);
    assert_eq!(cluster.controller_node_id, controller);
    assert!(cluster.peers.is_empty());
}

#[test]
fn node_cluster_register_peer() {
    let mut cluster = NodeCluster::new(make_node_id());
    let peer = make_peer(NodeRole::Worker);
    let peer_id = peer.node_id;
    cluster.register_peer(peer).unwrap();
    assert!(cluster.peers.contains_key(&peer_id));
    assert_eq!(cluster.peers.len(), 1);
}

#[test]
fn node_cluster_register_duplicate_peer_overwrites() {
    let mut cluster = NodeCluster::new(make_node_id());
    let peer = make_peer(NodeRole::Worker);
    let peer_id = peer.node_id;
    // Register same node_id twice with a clone that has a different address
    cluster.register_peer(peer.clone()).unwrap();
    let mut peer2 = peer;
    peer2.grpc_address = "https://worker-2:9090".to_string();
    cluster.register_peer(peer2).unwrap();
    assert_eq!(cluster.peers.len(), 1);
    assert_eq!(
        cluster.peers.get(&peer_id).unwrap().grpc_address,
        "https://worker-2:9090"
    );
}

#[test]
fn node_cluster_deregister_peer() {
    let mut cluster = NodeCluster::new(make_node_id());
    let peer = make_peer(NodeRole::Worker);
    let peer_id = peer.node_id;
    cluster.register_peer(peer).unwrap();
    cluster.deregister_peer(&peer_id, "test removal").unwrap();
    assert!(cluster.peers.is_empty());
}

#[test]
fn node_cluster_deregister_absent_peer_errors() {
    let mut cluster = NodeCluster::new(make_node_id());
    let result = cluster.deregister_peer(&make_node_id(), "not found");
    assert!(result.is_err());
}

#[test]
fn node_cluster_record_heartbeat_updates_timestamp() {
    let mut cluster = NodeCluster::new(make_node_id());
    let mut peer = make_peer(NodeRole::Worker);
    let peer_id = peer.node_id;
    // Set an old heartbeat
    peer.last_heartbeat_at = Utc::now() - Duration::seconds(120);
    let old_hb = peer.last_heartbeat_at;
    cluster.register_peer(peer).unwrap();

    let snapshot = ResourceSnapshot {
        cpu_utilization: 0.5,
        gpu_utilization: 0.3,
        active_executions: 2,
    };
    cluster.record_heartbeat(&peer_id, snapshot).unwrap();
    let updated_hb = cluster.peers.get(&peer_id).unwrap().last_heartbeat_at;
    assert!(updated_hb > old_hb);
}

#[test]
fn node_cluster_record_heartbeat_absent_peer_errors() {
    let mut cluster = NodeCluster::new(make_node_id());
    let snapshot = ResourceSnapshot {
        cpu_utilization: 0.0,
        gpu_utilization: 0.0,
        active_executions: 0,
    };
    assert!(cluster.record_heartbeat(&make_node_id(), snapshot).is_err());
}

#[test]
fn node_cluster_mark_unhealthy() {
    let mut cluster = NodeCluster::new(make_node_id());
    let peer = make_peer(NodeRole::Worker);
    let peer_id = peer.node_id;
    cluster.register_peer(peer).unwrap();

    let updated = cluster.mark_unhealthy(&peer_id).unwrap();
    assert_eq!(updated.status, NodePeerStatus::Unhealthy);
    assert_eq!(
        cluster.peers.get(&peer_id).unwrap().status,
        NodePeerStatus::Unhealthy
    );
}

#[test]
fn node_cluster_mark_unhealthy_absent_errors() {
    let mut cluster = NodeCluster::new(make_node_id());
    assert!(cluster.mark_unhealthy(&make_node_id()).is_err());
}

#[test]
fn node_cluster_healthy_workers_filters_correctly() {
    let mut cluster = NodeCluster::new(make_node_id());

    // Active Worker — should be included
    let worker = make_peer(NodeRole::Worker);
    let worker_id = worker.node_id;
    cluster.register_peer(worker).unwrap();

    // Active Hybrid — should be included
    let hybrid = make_peer(NodeRole::Hybrid);
    let hybrid_id = hybrid.node_id;
    cluster.register_peer(hybrid).unwrap();

    // Active Controller — should NOT be included
    cluster
        .register_peer(make_peer(NodeRole::Controller))
        .unwrap();

    // Unhealthy Worker — should NOT be included
    let sick = make_peer(NodeRole::Worker);
    let sick_id = sick.node_id;
    cluster.register_peer(sick).unwrap();
    cluster.mark_unhealthy(&sick_id).unwrap();

    let healthy = cluster.healthy_workers();
    assert_eq!(healthy.len(), 2);
    let ids: Vec<NodeId> = healthy.iter().map(|p| p.node_id).collect();
    assert!(ids.contains(&worker_id));
    assert!(ids.contains(&hybrid_id));
}

// ── ClusterSummaryStatus::from_counts() ─────────────────────────────────────

#[test]
fn cluster_summary_all_active_is_healthy() {
    assert_eq!(
        ClusterSummaryStatus::from_counts(5, 0, 0),
        ClusterSummaryStatus::Healthy
    );
}

#[test]
fn cluster_summary_zero_nodes_is_critical() {
    assert_eq!(
        ClusterSummaryStatus::from_counts(0, 0, 0),
        ClusterSummaryStatus::Critical
    );
}

#[test]
fn cluster_summary_majority_active_is_degraded() {
    // 3 active out of 5 total → active*2 = 6 > 5 → Degraded
    assert_eq!(
        ClusterSummaryStatus::from_counts(3, 1, 1),
        ClusterSummaryStatus::Degraded
    );
}

#[test]
fn cluster_summary_half_active_is_critical() {
    // 2 active out of 4 total → active*2 = 4, not > 4 → Critical
    assert_eq!(
        ClusterSummaryStatus::from_counts(2, 1, 1),
        ClusterSummaryStatus::Critical
    );
}

#[test]
fn cluster_summary_minority_active_is_critical() {
    assert_eq!(
        ClusterSummaryStatus::from_counts(1, 2, 2),
        ClusterSummaryStatus::Critical
    );
}

#[test]
fn cluster_summary_single_draining_is_degraded() {
    // 1 active, 1 draining → active*2 = 2 > 2? No → Critical
    assert_eq!(
        ClusterSummaryStatus::from_counts(1, 1, 0),
        ClusterSummaryStatus::Critical
    );
    // 2 active, 1 draining → active*2 = 4 > 3 → Degraded
    assert_eq!(
        ClusterSummaryStatus::from_counts(2, 1, 0),
        ClusterSummaryStatus::Degraded
    );
}

#[test]
fn cluster_summary_display() {
    assert_eq!(ClusterSummaryStatus::Healthy.to_string(), "healthy");
    assert_eq!(ClusterSummaryStatus::Degraded.to_string(), "degraded");
    assert_eq!(ClusterSummaryStatus::Critical.to_string(), "critical");
}

// ── NodeCapabilityAdvertisement ─────────────────────────────────────────────

#[test]
fn capability_satisfies_exact_match() {
    let cap = make_capabilities();
    let req = make_capabilities();
    assert!(cap.satisfies(&req));
}

#[test]
fn capability_satisfies_excess_resources() {
    let cap = NodeCapabilityAdvertisement {
        gpu_count: 4,
        vram_gb: 32,
        cpu_cores: 16,
        available_memory_gb: 64,
        supported_runtimes: vec!["docker".to_string(), "firecracker".to_string()],
        tags: vec!["gpu".to_string(), "fast".to_string()],
    };
    let req = make_capabilities();
    assert!(cap.satisfies(&req));
}

#[test]
fn capability_does_not_satisfy_insufficient_gpu() {
    let cap = NodeCapabilityAdvertisement {
        gpu_count: 0,
        ..make_capabilities()
    };
    let req = make_capabilities(); // requires 2 GPUs
    assert!(!cap.satisfies(&req));
}

#[test]
fn capability_does_not_satisfy_missing_runtime() {
    let cap = NodeCapabilityAdvertisement {
        supported_runtimes: vec!["docker".to_string()],
        ..make_capabilities()
    };
    let req = make_capabilities(); // requires docker + firecracker
    assert!(!cap.satisfies(&req));
}

#[test]
fn capability_does_not_satisfy_missing_tag() {
    let cap = NodeCapabilityAdvertisement {
        tags: vec![],
        ..make_capabilities()
    };
    let req = make_capabilities(); // requires "gpu" tag
    assert!(!cap.satisfies(&req));
}

#[test]
fn capability_hash_is_deterministic() {
    let cap = make_capabilities();
    let h1 = cap.hash();
    let h2 = cap.hash();
    assert_eq!(h1, h2);
    // SHA-256 hex is 64 chars
    assert_eq!(h1.len(), 64);
}

#[test]
fn capability_hash_differs_for_different_values() {
    let cap1 = make_capabilities();
    let cap2 = NodeCapabilityAdvertisement {
        gpu_count: 99,
        ..make_capabilities()
    };
    assert_ne!(cap1.hash(), cap2.hash());
}

// ── NodeSecurityToken ───────────────────────────────────────────────────────

#[test]
fn node_security_token_claims_valid_jwt() {
    let claims = make_token_claims(300);
    let token = NodeSecurityToken(make_jwt(&claims));
    let decoded = token.claims().unwrap();
    assert_eq!(decoded.node_id, claims.node_id);
    assert_eq!(decoded.role, claims.role);
    assert_eq!(decoded.capabilities_hash, claims.capabilities_hash);
    assert_eq!(decoded.exp, claims.exp);
}

#[test]
fn node_security_token_claims_invalid_jwt_format() {
    let token = NodeSecurityToken("not.a.valid.jwt.at.all".to_string());
    // More than 3 parts
    assert!(token.claims().is_err());

    let token2 = NodeSecurityToken("onlyonepart".to_string());
    assert!(token2.claims().is_err());
}

#[test]
fn node_security_token_is_expired_future() {
    let claims = make_token_claims(300); // 5 minutes from now
    let token = NodeSecurityToken(make_jwt(&claims));
    assert!(!token.is_expired());
}

#[test]
fn node_security_token_is_expired_past() {
    let claims = make_token_claims(-60); // 1 minute ago
    let token = NodeSecurityToken(make_jwt(&claims));
    assert!(token.is_expired());
}

#[test]
fn node_security_token_is_expired_invalid_token() {
    let token = NodeSecurityToken("garbage".to_string());
    assert!(token.is_expired()); // Invalid tokens are treated as expired
}

#[test]
fn node_security_token_seconds_until_expiry() {
    let claims = make_token_claims(120);
    let token = NodeSecurityToken(make_jwt(&claims));
    let remaining = token.seconds_until_expiry();
    // Should be close to 120 (within a few seconds of test execution)
    assert!(remaining > 115 && remaining <= 120);
}

#[test]
fn node_security_token_seconds_until_expiry_already_expired() {
    let claims = make_token_claims(-30);
    let token = NodeSecurityToken(make_jwt(&claims));
    assert_eq!(token.seconds_until_expiry(), 0);
}

#[test]
fn node_security_token_seconds_until_expiry_invalid() {
    let token = NodeSecurityToken("bad".to_string());
    assert_eq!(token.seconds_until_expiry(), 0);
}

// ── NodeChallenge ───────────────────────────────────────────────────────────

#[test]
fn node_challenge_not_expired_when_fresh() {
    let challenge = NodeChallenge {
        challenge_id: Uuid::new_v4(),
        node_id: make_node_id(),
        nonce: vec![42; 32],
        public_key: vec![1, 2, 3],
        role: NodeRole::Worker,
        capabilities: make_capabilities(),
        grpc_address: "https://node:9090".to_string(),
        created_at: Utc::now(),
    };
    assert!(!challenge.is_expired());
}

#[test]
fn node_challenge_expired_after_300s() {
    let challenge = NodeChallenge {
        challenge_id: Uuid::new_v4(),
        node_id: make_node_id(),
        nonce: vec![42; 32],
        public_key: vec![1, 2, 3],
        role: NodeRole::Worker,
        capabilities: make_capabilities(),
        grpc_address: "https://node:9090".to_string(),
        created_at: Utc::now() - Duration::seconds(301),
    };
    assert!(challenge.is_expired());
}

// ── ConfigScope ─────────────────────────────────────────────────────────────

#[test]
fn config_scope_display() {
    assert_eq!(ConfigScope::Global.to_string(), "global");
    assert_eq!(ConfigScope::Tenant.to_string(), "tenant");
    assert_eq!(ConfigScope::Node.to_string(), "node");
}

#[test]
fn config_scope_equality() {
    assert_eq!(ConfigScope::Global, ConfigScope::Global);
    assert_ne!(ConfigScope::Global, ConfigScope::Tenant);
    assert_ne!(ConfigScope::Tenant, ConfigScope::Node);
}

// ── RegisteredNode ──────────────────────────────────────────────────────────

#[test]
fn registered_node_from_peer() {
    let peer = make_peer(NodeRole::Worker);
    let metadata = HashMap::from([("region".to_string(), "us-east-1".to_string())]);
    let registered = RegisteredNode::from_peer(
        &peer,
        "worker-01.local".to_string(),
        "0.15.0-pre-alpha".to_string(), // TODO: Why is the version hardcoded here?
        metadata.clone(),
        Some("v1-abc".to_string()),
    );
    assert_eq!(registered.node_id, peer.node_id);
    assert_eq!(registered.hostname, "worker-01.local");
    assert_eq!(registered.role, NodeRole::Worker);
    assert_eq!(registered.registry_status, RegistryStatus::Pending);
    assert_eq!(registered.software_version, "0.15.0-pre-alpha"); // TODO: Why is the version hardcoded here?
    assert_eq!(registered.config_version, Some("v1-abc".to_string()));
    assert_eq!(registered.metadata, metadata);
}

#[test]
fn registered_node_pending_is_not_active() {
    let peer = make_peer(NodeRole::Worker);
    let node = RegisteredNode::from_peer(
        &peer,
        "h".to_string(),
        "v".to_string(),
        HashMap::new(),
        None,
    );
    // from_peer initialises to Pending — not yet active
    assert_eq!(node.registry_status, RegistryStatus::Pending);
    assert!(!node.is_active());
}

#[test]
fn registered_node_active_after_activate() {
    let peer = make_peer(NodeRole::Worker);
    let mut node = RegisteredNode::from_peer(
        &peer,
        "h".to_string(),
        "v".to_string(),
        HashMap::new(),
        None,
    );
    node.activate()
        .expect("activate should succeed from Pending");
    assert_eq!(node.registry_status, RegistryStatus::Active);
    assert!(node.is_active());
}

#[test]
fn registered_node_decommissioned_is_not_active() {
    let peer = make_peer(NodeRole::Worker);
    let mut node = RegisteredNode::from_peer(
        &peer,
        "h".to_string(),
        "v".to_string(),
        HashMap::new(),
        None,
    );
    node.decommission()
        .expect("decommission should succeed from Pending");
    assert_eq!(node.registry_status, RegistryStatus::Decommissioned);
    assert!(!node.is_active());
}

// ── ExecutionRoute ──────────────────────────────────────────────────────────

#[test]
fn execution_route_fields() {
    let node_id = make_node_id();
    let route = ExecutionRoute {
        target_node_id: node_id,
        worker_grpc_address: "https://worker-3:9090".to_string(),
    };
    assert_eq!(route.target_node_id, node_id);
    assert_eq!(route.worker_grpc_address, "https://worker-3:9090");
}

// ============================================================================
// Serialization Round-Trips
// ============================================================================

#[test]
fn serde_roundtrip_stimulus() {
    let stim = Stimulus::new(
        StimulusSource::Webhook {
            source_name: "github".to_string(),
        },
        "event body",
    )
    .with_idempotency_key("key-1");
    let json = serde_json::to_string(&stim).expect("serialize Stimulus");
    let deser: Stimulus = serde_json::from_str(&json).expect("deserialize Stimulus");
    assert_eq!(deser.id, stim.id);
    assert_eq!(deser.content, stim.content);
    assert_eq!(deser.idempotency_key, Some("key-1".to_string()));
}

#[test]
fn serde_roundtrip_stimulus_source_all_variants() {
    let sources = vec![
        StimulusSource::Webhook {
            source_name: "test".to_string(),
        },
        StimulusSource::HttpApi,
        StimulusSource::Stdin,
        StimulusSource::TemporalSignal,
    ];
    for src in sources {
        let json = serde_json::to_string(&src).expect("serialize");
        let deser: StimulusSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deser, src);
    }
}

#[test]
fn serde_roundtrip_routing_decision() {
    let decision = RoutingDecision {
        workflow_id: make_workflow_id(),
        confidence: 0.92,
        mode: RoutingMode::LlmClassified,
    };
    let json = serde_json::to_string(&decision).expect("serialize");
    let deser: RoutingDecision = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(deser.workflow_id, decision.workflow_id);
    assert_eq!(deser.confidence, 0.92);
}

#[test]
fn serde_roundtrip_rejection_reason() {
    let reasons: Vec<RejectionReason> = vec![
        RejectionReason::LowConfidence {
            confidence: 0.3,
            threshold: 0.7,
        },
        RejectionReason::NoRouterConfigured {
            source: "s".to_string(),
        },
        RejectionReason::HmacInvalid,
        RejectionReason::IdempotentDuplicate {
            original_stimulus_id: StimulusId::new(),
        },
    ];
    for reason in reasons {
        let json = serde_json::to_string(&reason).expect("serialize");
        let _deser: RejectionReason = serde_json::from_str(&json).expect("deserialize");
    }
}

#[test]
fn serde_roundtrip_node_peer() {
    let peer = make_peer(NodeRole::Hybrid);
    let json = serde_json::to_string(&peer).expect("serialize NodePeer");
    let deser: NodePeer = serde_json::from_str(&json).expect("deserialize NodePeer");
    assert_eq!(deser.node_id, peer.node_id);
    assert_eq!(deser.role, peer.role);
    assert_eq!(deser.status, peer.status);
    assert_eq!(deser.grpc_address, peer.grpc_address);
}

#[test]
fn serde_roundtrip_node_capability_advertisement() {
    let cap = make_capabilities();
    let json = serde_json::to_string(&cap).expect("serialize");
    let deser: NodeCapabilityAdvertisement = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(deser, cap);
}

#[test]
fn serde_roundtrip_execution_route() {
    let route = ExecutionRoute {
        target_node_id: make_node_id(),
        worker_grpc_address: "https://w:9090".to_string(),
    };
    let json = serde_json::to_string(&route).expect("serialize");
    let deser: ExecutionRoute = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(deser.target_node_id, route.target_node_id);
    assert_eq!(deser.worker_grpc_address, route.worker_grpc_address);
}

#[test]
fn serde_roundtrip_cluster_summary_status() {
    let statuses = vec![
        ClusterSummaryStatus::Healthy,
        ClusterSummaryStatus::Degraded,
        ClusterSummaryStatus::Critical,
    ];
    for status in statuses {
        let json = serde_json::to_string(&status).expect("serialize");
        let deser: ClusterSummaryStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deser, status);
    }
}

#[test]
fn serde_roundtrip_config_scope() {
    let scopes = vec![ConfigScope::Global, ConfigScope::Tenant, ConfigScope::Node];
    for scope in scopes {
        let json = serde_json::to_string(&scope).expect("serialize");
        let deser: ConfigScope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deser, scope);
    }
}

#[test]
fn serde_roundtrip_node_security_token() {
    let claims = make_token_claims(300);
    let token = NodeSecurityToken(make_jwt(&claims));
    let json = serde_json::to_string(&token).expect("serialize");
    let deser: NodeSecurityToken = serde_json::from_str(&json).expect("deserialize");
    let deser_claims = deser.claims().unwrap();
    assert_eq!(deser_claims.node_id, claims.node_id);
    assert_eq!(deser_claims.exp, claims.exp);
}
