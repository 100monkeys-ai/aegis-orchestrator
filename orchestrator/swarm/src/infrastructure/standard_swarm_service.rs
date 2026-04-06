// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::application::{LockToken, SpawnedChild, SwarmService};
use crate::domain::swarm::SwarmChildSpec;
use crate::domain::{
    CancellationReason, MessageEnvelope, ResourceLock, Swarm, SwarmId, SwarmStatus,
};
use aegis_orchestrator_core::application::ports::SwarmCancellationPort;
use aegis_orchestrator_core::domain::shared_kernel::{AgentId, ExecutionId};
use aegis_orchestrator_core::domain::tenant::TenantId;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Default)]
struct SwarmState {
    swarms: HashMap<SwarmId, Swarm>,
    execution_to_swarm: HashMap<ExecutionId, SwarmId>,
    locks: HashMap<String, ResourceLock>,
    tokens: HashMap<String, String>,
    messages: HashMap<SwarmId, Vec<MessageEnvelope>>,
}

/// Phase 1 in-memory swarm coordination service.
pub struct StandardSwarmService {
    state: Arc<RwLock<SwarmState>>,
}

impl StandardSwarmService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(SwarmState::default())),
        }
    }

    pub async fn messages_for_swarm(&self, swarm_id: SwarmId) -> Vec<MessageEnvelope> {
        let state = self.state.read().await;
        state.messages.get(&swarm_id).cloned().unwrap_or_default()
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
        let expired_count = expired_resources.len() as u64;
        for resource in expired_resources {
            state.locks.remove(&resource);
            state
                .tokens
                .retain(|_, held_resource| held_resource != &resource);
        }
        if expired_count > 0 {
            metrics::counter!("aegis_swarm_lock_expirations_total").increment(expired_count);
        }
    }

    /// Spawn a background task that garbage-collects expired locks every 30 seconds.
    pub fn start_gc_task(self: &Arc<Self>) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let mut guard = state.write().await;
                Self::cleanup_expired_locks(&mut guard);
            }
        });
    }
}

impl Default for StandardSwarmService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SwarmService for StandardSwarmService {
    async fn create_swarm(
        &self,
        parent_execution_id: ExecutionId,
        tenant_id: TenantId,
    ) -> Result<SwarmId> {
        let mut state = self.state.write().await;
        if state.execution_to_swarm.contains_key(&parent_execution_id) {
            bail!("parent execution {parent_execution_id:?} already belongs to a swarm");
        }

        let swarm = Swarm::new(parent_execution_id, tenant_id);
        let swarm_id = swarm.id;
        state
            .execution_to_swarm
            .insert(parent_execution_id, swarm_id);
        state.swarms.insert(swarm_id, swarm);
        metrics::gauge!("aegis_swarms_active").increment(1);
        Ok(swarm_id)
    }

    async fn spawn_child(
        &self,
        swarm_id: SwarmId,
        _spec: SwarmChildSpec,
        _parent_security_context: Option<String>,
    ) -> Result<SpawnedChild> {
        let mut state = self.state.write().await;
        let swarm = match state.swarms.get_mut(&swarm_id) {
            Some(s) => s,
            None => {
                metrics::counter!("aegis_swarm_child_spawns_total", "result" => "rejected")
                    .increment(1);
                bail!("swarm {swarm_id:?} not found");
            }
        };

        if swarm.status != SwarmStatus::Active {
            metrics::counter!("aegis_swarm_child_spawns_total", "result" => "rejected")
                .increment(1);
            bail!("swarm {swarm_id:?} is not active");
        }

        let agent_id = AgentId::new();
        let execution_id = ExecutionId::new();
        swarm.add_member(execution_id, agent_id);
        state.execution_to_swarm.insert(execution_id, swarm_id);

        metrics::counter!("aegis_swarm_child_spawns_total", "result" => "success").increment(1);
        Ok(SpawnedChild {
            agent_id,
            execution_id,
            swarm_id,
        })
    }

    async fn send_message(&self, from: AgentId, to: AgentId, payload: Vec<u8>) -> Result<()> {
        let mut state = self.state.write().await;
        // Find which swarm `from` belongs to by scanning members
        let from_swarm = Self::find_swarm_for_agent(&state, from)?;
        let to_swarm = Self::find_swarm_for_agent(&state, to)?;
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

    async fn acquire_lock(
        &self,
        swarm_id: SwarmId,
        resource: &str,
        holder: AgentId,
        execution_id: ExecutionId,
        ttl: Duration,
    ) -> Result<LockToken> {
        let mut state = self.state.write().await;
        Self::cleanup_expired_locks(&mut state);

        // Validate holder is in the swarm
        let swarm = state
            .swarms
            .get(&swarm_id)
            .ok_or_else(|| anyhow!("swarm {swarm_id:?} not found"))?;
        if !swarm.contains(holder) {
            bail!("agent {holder:?} is not a member of swarm {swarm_id:?}");
        }

        if state.locks.contains_key(resource) {
            metrics::counter!("aegis_swarm_lock_contentions_total").increment(1);
            bail!("resource lock already held: {resource}");
        }

        let token = LockToken(Uuid::new_v4().to_string());
        let ttl_chrono =
            chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::minutes(5));
        let lock = ResourceLock {
            resource_id: resource.to_string(),
            held_by: holder,
            execution_id,
            acquired_at: Utc::now(),
            expires_at: Utc::now() + ttl_chrono,
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

    async fn cancel_swarm(&self, swarm_id: SwarmId, _reason: CancellationReason) -> Result<()> {
        let mut state = self.state.write().await;
        let member_execution_ids = {
            let swarm = state
                .swarms
                .get_mut(&swarm_id)
                .ok_or_else(|| anyhow!("swarm {swarm_id:?} not found"))?;
            swarm.status = SwarmStatus::Dissolving;
            let exec_ids = swarm.member_execution_ids();
            let member_agent_ids = swarm.member_ids();
            swarm.dissolve();
            // Release all locks held by member agents
            state
                .locks
                .retain(|_, lock| !member_agent_ids.contains(&lock.held_by));
            exec_ids
        };
        // Clean up execution_to_swarm entries
        for exec_id in &member_execution_ids {
            state.execution_to_swarm.remove(exec_id);
        }
        // Also remove parent execution mapping
        let parent_exec = state.swarms.get(&swarm_id).map(|s| s.parent_execution_id);
        if let Some(parent) = parent_exec {
            state.execution_to_swarm.remove(&parent);
        }

        metrics::gauge!("aegis_swarms_active").decrement(1);
        metrics::counter!("aegis_swarm_cascade_cancellations_total").increment(1);
        Ok(())
    }

    async fn get_swarm(&self, swarm_id: SwarmId) -> Result<Option<Swarm>> {
        let state = self.state.read().await;
        Ok(state.swarms.get(&swarm_id).cloned())
    }

    async fn list_child_executions(&self, swarm_id: SwarmId) -> Result<Vec<ExecutionId>> {
        let state = self.state.read().await;
        let swarm = state
            .swarms
            .get(&swarm_id)
            .ok_or_else(|| anyhow!("swarm {swarm_id:?} not found"))?;
        Ok(swarm.member_execution_ids())
    }

    async fn broadcast_message(
        &self,
        swarm_id: SwarmId,
        from: AgentId,
        payload: Vec<u8>,
    ) -> Result<()> {
        let mut state = self.state.write().await;
        let swarm = state
            .swarms
            .get(&swarm_id)
            .ok_or_else(|| anyhow!("swarm {swarm_id:?} not found"))?;

        let recipients: Vec<AgentId> = swarm
            .member_ids()
            .into_iter()
            .filter(|&id| id != from)
            .collect();

        let envelopes: Vec<MessageEnvelope> = recipients
            .into_iter()
            .map(|to| MessageEnvelope {
                from,
                to,
                payload: payload.clone(),
                sent_at: Utc::now(),
            })
            .collect();

        state
            .messages
            .entry(swarm_id)
            .or_default()
            .extend(envelopes);
        Ok(())
    }
}

impl StandardSwarmService {
    /// Find which swarm an agent belongs to by scanning all swarms' members.
    fn find_swarm_for_agent(state: &SwarmState, agent_id: AgentId) -> Result<SwarmId> {
        for (swarm_id, swarm) in &state.swarms {
            if swarm.contains(agent_id) {
                return Ok(*swarm_id);
            }
        }
        Err(anyhow!("agent {agent_id:?} is not assigned to a swarm"))
    }
}

#[async_trait]
impl SwarmCancellationPort for StandardSwarmService {
    async fn cascade_cancel_for_execution(&self, execution_id: ExecutionId) -> anyhow::Result<()> {
        let state = self.state.read().await;
        let swarm_id = state.execution_to_swarm.get(&execution_id).copied();
        drop(state);

        if let Some(swarm_id) = swarm_id {
            self.cancel_swarm(swarm_id, CancellationReason::ParentCancelled)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::swarm::{SwarmChildSpec, SwarmResourceLimits};

    fn test_child_spec() -> SwarmChildSpec {
        SwarmChildSpec {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: TenantId::consumer(),
            name: "child-agent".to_string(),
            language: "python".to_string(),
            version: "3.11".to_string(),
            resource_limits: Some(SwarmResourceLimits {
                cpu: 1000,
                memory: "512Mi".to_string(),
            }),
        }
    }

    #[tokio::test]
    async fn creates_swarm_spawns_child_and_records_message() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();
        let spawned = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        service
            .send_message(spawned.agent_id, spawned.agent_id, b"hello".to_vec())
            .await
            .unwrap();

        let swarm = service.get_swarm(swarm_id).await.unwrap().unwrap();
        assert!(swarm.contains(spawned.agent_id));
        assert!(swarm.contains_execution(&spawned.execution_id));
        assert_eq!(service.messages_for_swarm(swarm_id).await.len(), 1);
    }

    #[tokio::test]
    async fn lock_acquire_release_and_contention() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();
        let spawned = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        let ttl = Duration::from_secs(300);
        let token = service
            .acquire_lock(
                swarm_id,
                "workspace",
                spawned.agent_id,
                spawned.execution_id,
                ttl,
            )
            .await
            .unwrap();
        // Same resource should fail
        assert!(service
            .acquire_lock(
                swarm_id,
                "workspace",
                spawned.agent_id,
                spawned.execution_id,
                ttl
            )
            .await
            .is_err());
        service.release_lock(token).await.unwrap();
        // After release, should succeed
        assert!(service
            .acquire_lock(
                swarm_id,
                "workspace",
                spawned.agent_id,
                spawned.execution_id,
                ttl
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn acquire_lock_rejects_non_member() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();

        let outsider = AgentId::new();
        let outsider_exec = ExecutionId::new();
        let ttl = Duration::from_secs(60);
        let result = service
            .acquire_lock(swarm_id, "resource", outsider, outsider_exec, ttl)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_swarm_releases_locks_and_cleans_execution_map() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();
        let spawned = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        // Acquire a lock
        let ttl = Duration::from_secs(300);
        let _token = service
            .acquire_lock(
                swarm_id,
                "shared-file",
                spawned.agent_id,
                spawned.execution_id,
                ttl,
            )
            .await
            .unwrap();

        service
            .cancel_swarm(swarm_id, CancellationReason::Manual)
            .await
            .unwrap();

        let swarm = service.get_swarm(swarm_id).await.unwrap().unwrap();
        assert_eq!(swarm.status, SwarmStatus::Dissolved);
        assert!(swarm.dissolved_at.is_some());

        // Locks should be cleaned up
        let locks = service.locks_for_swarm(swarm_id).await;
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn list_child_executions_returns_correct_ids() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();

        let spawned1 = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();
        let spawned2 = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        let exec_ids = service.list_child_executions(swarm_id).await.unwrap();
        assert_eq!(exec_ids.len(), 2);
        assert!(exec_ids.contains(&spawned1.execution_id));
        assert!(exec_ids.contains(&spawned2.execution_id));
    }

    #[tokio::test]
    async fn broadcast_message_delivers_to_all_except_sender() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();

        let spawned1 = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();
        let spawned2 = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();
        let spawned3 = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        service
            .broadcast_message(swarm_id, spawned1.agent_id, b"broadcast-payload".to_vec())
            .await
            .unwrap();

        let messages = service.messages_for_swarm(swarm_id).await;
        // Should deliver to spawned2 and spawned3 but not spawned1
        assert_eq!(messages.len(), 2);
        for msg in &messages {
            assert_eq!(msg.from, spawned1.agent_id);
            assert_ne!(msg.to, spawned1.agent_id);
            assert!(msg.to == spawned2.agent_id || msg.to == spawned3.agent_id);
            assert_eq!(msg.payload, b"broadcast-payload");
        }
    }

    #[tokio::test]
    async fn spawn_child_returns_spawned_child_with_correct_swarm_id() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();

        let spawned = service
            .spawn_child(swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        assert_eq!(spawned.swarm_id, swarm_id);
    }

    #[tokio::test]
    async fn spawn_child_on_dissolved_swarm_fails() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        let swarm_id = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();

        service
            .cancel_swarm(swarm_id, CancellationReason::Manual)
            .await
            .unwrap();

        let result = service.spawn_child(swarm_id, test_child_spec(), None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_swarm_returns_none_for_unknown() {
        let service = StandardSwarmService::new();
        let result = service.get_swarm(SwarmId::new()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_child_executions_fails_for_unknown_swarm() {
        let service = StandardSwarmService::new();
        let result = service.list_child_executions(SwarmId::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn duplicate_parent_execution_rejected() {
        let service = StandardSwarmService::new();
        let parent_exec = ExecutionId::new();
        service
            .create_swarm(parent_exec, TenantId::consumer())
            .await
            .unwrap();
        let result = service
            .create_swarm(parent_exec, TenantId::consumer())
            .await;
        assert!(result.is_err());
    }
}
