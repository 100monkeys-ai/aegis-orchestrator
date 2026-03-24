// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::application::{LockToken, SwarmService};
use crate::domain::{
    CancellationReason, MessageEnvelope, ResourceLock, Swarm, SwarmId, SwarmStatus,
};
use aegis_orchestrator_core::domain::agent::{AgentId, AgentManifest};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Default)]
struct SwarmState {
    swarms: HashMap<SwarmId, Swarm>,
    agent_to_swarm: HashMap<AgentId, SwarmId>,
    locks: HashMap<String, ResourceLock>,
    tokens: HashMap<String, String>,
    messages: HashMap<SwarmId, Vec<MessageEnvelope>>,
}

/// Phase 1 in-memory swarm coordination service.
pub struct StandardSwarmService {
    state: Arc<RwLock<SwarmState>>,
    default_lock_ttl: Duration,
}

impl StandardSwarmService {
    pub fn new() -> Self {
        Self::with_default_lock_ttl(Duration::minutes(5))
    }

    pub fn with_default_lock_ttl(default_lock_ttl: Duration) -> Self {
        Self {
            state: Arc::new(RwLock::new(SwarmState::default())),
            default_lock_ttl,
        }
    }

    pub async fn messages_for_swarm(&self, swarm_id: SwarmId) -> Vec<MessageEnvelope> {
        let state = self.state.read().await;
        state.messages.get(&swarm_id).cloned().unwrap_or_default()
    }

    pub async fn cancel_swarm(&self, swarm_id: SwarmId, _reason: CancellationReason) -> Result<()> {
        let mut state = self.state.write().await;
        let member_ids = {
            let swarm = state
                .swarms
                .get_mut(&swarm_id)
                .ok_or_else(|| anyhow!("swarm {swarm_id:?} not found"))?;
            swarm.status = SwarmStatus::Dissolving;
            let ids = swarm.member_ids();
            swarm.dissolve();
            ids
        };
        for agent_id in &member_ids {
            state.agent_to_swarm.remove(agent_id);
        }
        state
            .locks
            .retain(|_, lock| !member_ids.contains(&lock.held_by));
        Ok(())
    }

    pub async fn get_swarm(&self, swarm_id: SwarmId) -> Option<Swarm> {
        let state = self.state.read().await;
        state.swarms.get(&swarm_id).cloned()
    }

    pub async fn list_swarms(&self) -> Vec<Swarm> {
        let state = self.state.read().await;
        let mut swarms: Vec<Swarm> = state.swarms.values().cloned().collect();
        swarms.sort_by_key(|swarm| swarm.created_at);
        swarms
    }

    pub async fn locks_for_swarm(&self, swarm_id: SwarmId) -> Vec<ResourceLock> {
        let state = self.state.read().await;
        let Some(swarm) = state.swarms.get(&swarm_id) else {
            return Vec::new();
        };

        let member_ids = swarm.member_ids();
        state
            .locks
            .values()
            .filter(|lock| member_ids.contains(&lock.held_by))
            .cloned()
            .collect()
    }

    fn cleanup_expired_locks(state: &mut SwarmState) {
        let now = Utc::now();
        let expired_resources: Vec<String> = state
            .locks
            .iter()
            .filter_map(|(resource, lock)| (lock.expires_at <= now).then_some(resource.clone()))
            .collect();
        for resource in expired_resources {
            state.locks.remove(&resource);
            state
                .tokens
                .retain(|_, held_resource| held_resource != &resource);
        }
    }

    fn swarm_for_agent(state: &SwarmState, agent_id: AgentId) -> Result<SwarmId> {
        state
            .agent_to_swarm
            .get(&agent_id)
            .copied()
            .ok_or_else(|| anyhow!("agent {agent_id:?} is not assigned to a swarm"))
    }
}

impl Default for StandardSwarmService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SwarmService for StandardSwarmService {
    async fn create_swarm(&self, parent_id: AgentId) -> Result<SwarmId> {
        let mut state = self.state.write().await;
        if state.agent_to_swarm.contains_key(&parent_id) {
            bail!("parent agent {parent_id:?} already belongs to a swarm");
        }

        let swarm = Swarm::new(parent_id);
        let swarm_id = swarm.id;
        state.agent_to_swarm.insert(parent_id, swarm_id);
        state.swarms.insert(swarm_id, swarm);
        Ok(swarm_id)
    }

    async fn spawn_child(&self, parent_id: AgentId, _manifest: AgentManifest) -> Result<AgentId> {
        let mut state = self.state.write().await;
        let swarm_id = Self::swarm_for_agent(&state, parent_id)?;
        let child_id = {
            let swarm = state
                .swarms
                .get_mut(&swarm_id)
                .ok_or_else(|| anyhow!("swarm {swarm_id:?} not found"))?;

            if swarm.status != SwarmStatus::Active {
                bail!("swarm {swarm_id:?} is not active");
            }

            let child_id = AgentId::new();
            swarm.add_member(child_id);
            child_id
        };
        state.agent_to_swarm.insert(child_id, swarm_id);
        Ok(child_id)
    }

    async fn send_message(&self, from: AgentId, to: AgentId, payload: Vec<u8>) -> Result<()> {
        let mut state = self.state.write().await;
        let from_swarm = Self::swarm_for_agent(&state, from)?;
        let to_swarm = Self::swarm_for_agent(&state, to)?;
        if from_swarm != to_swarm {
            bail!("agents {from:?} and {to:?} do not belong to the same swarm");
        }

        let envelope = MessageEnvelope {
            from,
            to,
            payload,
            sent_at: Utc::now(),
        };
        state.messages.entry(from_swarm).or_default().push(envelope);
        Ok(())
    }

    async fn acquire_lock(&self, resource: &str) -> Result<LockToken> {
        let mut state = self.state.write().await;
        Self::cleanup_expired_locks(&mut state);

        if state.locks.contains_key(resource) {
            bail!("resource lock already held: {resource}");
        }

        let token = LockToken(Uuid::new_v4().to_string());
        let held_by = state
            .agent_to_swarm
            .keys()
            .next()
            .copied()
            .ok_or_else(|| anyhow!("cannot acquire lock without an active swarm"))?;
        let lock = ResourceLock {
            resource_id: resource.to_string(),
            held_by,
            acquired_at: Utc::now(),
            expires_at: Utc::now() + self.default_lock_ttl,
        };

        state.locks.insert(resource.to_string(), lock);
        state.tokens.insert(token.0.clone(), resource.to_string());
        Ok(token)
    }

    async fn release_lock(&self, token: LockToken) -> Result<()> {
        let mut state = self.state.write().await;
        let resource = state
            .tokens
            .remove(&token.0)
            .ok_or_else(|| anyhow!("unknown swarm lock token"))?;
        state.locks.remove(&resource);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::agent::{
        AgentSpec, ManifestMetadata, ResourceLimits, RuntimeConfig, SecurityConfig,
    };

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: "child-agent".to_string(),
                version: "1.0.0".to_string(),
                description: None,
                labels: Default::default(),
                annotations: Default::default(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: Some("python".to_string()),
                    version: Some("3.11".to_string()),
                    image: None,
                    image_pull_policy: Default::default(),
                    isolation: "docker".to_string(),
                    model: "default".to_string(),
                },
                task: None,
                context: vec![],
                execution: None,
                security: Some(SecurityConfig {
                    network: Default::default(),
                    filesystem: Default::default(),
                    resources: ResourceLimits {
                        cpu: 1000,
                        memory: "512Mi".to_string(),
                        disk: "1Gi".to_string(),
                        timeout: Some("60s".to_string()),
                    },
                }),
                schedule: None,
                tools: vec![],
                env: Default::default(),
                volumes: vec![],
                advanced: None,
            },
        }
    }

    #[tokio::test]
    async fn creates_swarm_spawns_child_and_records_message() {
        let service = StandardSwarmService::new();
        let parent = AgentId::new();
        let swarm_id = service.create_swarm(parent).await.unwrap();
        let child = service.spawn_child(parent, test_manifest()).await.unwrap();

        service
            .send_message(parent, child, b"hello".to_vec())
            .await
            .unwrap();

        let swarm = service.get_swarm(swarm_id).await.unwrap();
        assert!(swarm.contains(parent));
        assert!(swarm.contains(child));
        assert_eq!(service.messages_for_swarm(swarm_id).await.len(), 1);
    }

    #[tokio::test]
    async fn lock_tokens_release_and_expire() {
        let service = StandardSwarmService::with_default_lock_ttl(Duration::milliseconds(1));
        let parent = AgentId::new();
        service.create_swarm(parent).await.unwrap();

        let token = service.acquire_lock("workspace").await.unwrap();
        assert!(service.acquire_lock("workspace").await.is_err());
        service.release_lock(token).await.unwrap();
        assert!(service.acquire_lock("workspace").await.is_ok());
    }

    #[tokio::test]
    async fn cancel_swarm_releases_membership() {
        let service = StandardSwarmService::new();
        let parent = AgentId::new();
        let swarm_id = service.create_swarm(parent).await.unwrap();
        let child = service.spawn_child(parent, test_manifest()).await.unwrap();

        service
            .cancel_swarm(swarm_id, CancellationReason::Manual)
            .await
            .unwrap();

        assert!(service.send_message(parent, child, vec![]).await.is_err());
        assert_eq!(
            service.get_swarm(swarm_id).await.unwrap().status,
            SwarmStatus::Dissolved
        );
    }
}
