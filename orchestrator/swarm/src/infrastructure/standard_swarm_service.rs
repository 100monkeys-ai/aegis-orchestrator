// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::application::{LockToken, SpawnedChild, SwarmService};
use crate::domain::swarm::SwarmChildSpec;
use crate::domain::{
    CancellationReason, MessageEnvelope, ResourceLock, Swarm, SwarmId, SwarmStatus,
};
use aegis_orchestrator_core::application::ports::SwarmCancellationPort;
use aegis_orchestrator_core::domain::repository::ExecutionRepository;
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
    /// Token -> (tenant_id, resource) so `release_lock` can verify the
    /// caller's tenant matches the lock's owning swarm before deletion.
    tokens: HashMap<String, (TenantId, String)>,
    messages: HashMap<SwarmId, Vec<MessageEnvelope>>,
}

/// Phase 1 in-memory swarm coordination service.
///
/// All `SwarmId`-keyed lookups are additionally scoped by [`TenantId`]
/// (audit 002, finding 4.34). Knowledge of a foreign tenant's `SwarmId`
/// confers no read or write capability — mismatches surface as
/// `swarm not found`.
pub struct StandardSwarmService {
    state: Arc<RwLock<SwarmState>>,
    /// Optional in Phase 1 to keep the in-memory test harness ergonomic.
    /// When `None`, `create_swarm` cannot verify parent execution ownership
    /// and will reject the call (audit 002, finding 4.33). Production
    /// wiring MUST inject the real repository.
    execution_repo: Option<Arc<dyn ExecutionRepository>>,
}

impl StandardSwarmService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(SwarmState::default())),
            execution_repo: None,
        }
    }

    /// Inject the [`ExecutionRepository`] used to verify that the
    /// `parent_execution_id` supplied to [`SwarmService::create_swarm`]
    /// belongs to the caller's tenant.
    pub fn with_execution_repository(mut self, repository: Arc<dyn ExecutionRepository>) -> Self {
        self.execution_repo = Some(repository);
        self
    }

    /// Tenant-scoped read of recent messages on a swarm. Returns an empty
    /// vector for unknown swarms or for swarms owned by a different tenant —
    /// the two cases are intentionally indistinguishable.
    pub async fn messages_for_swarm(
        &self,
        tenant_id: &TenantId,
        swarm_id: SwarmId,
    ) -> Vec<MessageEnvelope> {
        let state = self.state.read().await;
        match state.swarms.get(&swarm_id) {
            Some(swarm) if &swarm.tenant_id == tenant_id => {
                state.messages.get(&swarm_id).cloned().unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// Cross-tenant list of all swarms. Reserved for operator-only
    /// dashboards (ADR-097) — callers MUST verify the request comes from
    /// an operator before invoking. Mirrors the
    /// `ExecutionRepository::find_by_id_unscoped` convention.
    pub async fn list_swarms_unscoped(&self) -> Vec<Swarm> {
        let state = self.state.read().await;
        let mut swarms: Vec<Swarm> = state.swarms.values().cloned().collect();
        swarms.sort_by_key(|swarm| swarm.created_at);
        swarms
    }

    /// Tenant-scoped list of swarms.
    pub async fn list_swarms(&self, tenant_id: &TenantId) -> Vec<Swarm> {
        let state = self.state.read().await;
        let mut swarms: Vec<Swarm> = state
            .swarms
            .values()
            .filter(|swarm| &swarm.tenant_id == tenant_id)
            .cloned()
            .collect();
        swarms.sort_by_key(|swarm| swarm.created_at);
        swarms
    }

    /// Tenant-scoped read of resource locks held by members of a swarm.
    /// Returns an empty vector if the swarm does not exist OR belongs to a
    /// different tenant.
    pub async fn locks_for_swarm(
        &self,
        tenant_id: &TenantId,
        swarm_id: SwarmId,
    ) -> Vec<ResourceLock> {
        let state = self.state.read().await;
        let Some(swarm) = state.swarms.get(&swarm_id) else {
            return Vec::new();
        };
        if &swarm.tenant_id != tenant_id {
            return Vec::new();
        }

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
                .retain(|_, (_, held_resource)| held_resource != &resource);
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

/// Look up a mutable swarm reference in `state`, enforcing the
/// `(tenant_id, swarm_id)` invariant. Returns the same `not found` error
/// for both genuinely missing swarms and tenant mismatches to avoid leaking
/// existence across tenants.
fn swarm_for_tenant_mut<'a>(
    state: &'a mut SwarmState,
    tenant_id: &TenantId,
    swarm_id: SwarmId,
) -> Result<&'a mut Swarm> {
    match state.swarms.get_mut(&swarm_id) {
        Some(swarm) if &swarm.tenant_id == tenant_id => Ok(swarm),
        _ => Err(anyhow!("swarm {swarm_id:?} not found")),
    }
}

/// Immutable variant of [`swarm_for_tenant_mut`].
fn swarm_for_tenant<'a>(
    state: &'a SwarmState,
    tenant_id: &TenantId,
    swarm_id: SwarmId,
) -> Result<&'a Swarm> {
    match state.swarms.get(&swarm_id) {
        Some(swarm) if &swarm.tenant_id == tenant_id => Ok(swarm),
        _ => Err(anyhow!("swarm {swarm_id:?} not found")),
    }
}

#[async_trait]
impl SwarmService for StandardSwarmService {
    async fn create_swarm(
        &self,
        parent_execution_id: ExecutionId,
        tenant_id: TenantId,
    ) -> Result<SwarmId> {
        // Audit 002, finding 4.33: verify the parent execution belongs to
        // the caller's tenant before creating a swarm that will claim it as
        // its root.
        let repo = self
            .execution_repo
            .as_ref()
            .ok_or_else(|| anyhow!("swarm service missing execution repository binding"))?;

        let parent = repo
            .find_by_id_for_tenant(&tenant_id, parent_execution_id)
            .await
            .map_err(|e| anyhow!("failed to look up parent execution: {e}"))?;
        if parent.is_none() {
            metrics::counter!(
                "aegis_swarm_creates_total",
                "result" => "rejected_parent_not_found"
            )
            .increment(1);
            bail!(
                "parent execution {parent_execution_id:?} not found for tenant {:?}",
                tenant_id
            );
        }

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
        tenant_id: &TenantId,
        swarm_id: SwarmId,
        spec: SwarmChildSpec,
        _parent_security_context: Option<String>,
    ) -> Result<SpawnedChild> {
        let mut state = self.state.write().await;
        let swarm = match swarm_for_tenant_mut(&mut state, tenant_id, swarm_id) {
            Ok(s) => s,
            Err(e) => {
                metrics::counter!("aegis_swarm_child_spawns_total", "result" => "rejected")
                    .increment(1);
                return Err(e);
            }
        };

        if swarm.status != SwarmStatus::Active {
            metrics::counter!("aegis_swarm_child_spawns_total", "result" => "rejected")
                .increment(1);
            bail!("swarm {swarm_id:?} is not active");
        }

        // Audit 002, finding 4.14: route through the aggregate so the
        // cross-tenant invariant in `Swarm::spawn_child` is enforced.
        if let Err(err) = swarm.spawn_child(spec.clone()) {
            metrics::counter!(
                "aegis_swarm_child_spawns_total",
                "result" => "rejected_cross_tenant"
            )
            .increment(1);
            return Err(anyhow!("{err}"));
        }
        let agent_id = spec.agent_id;
        let execution_id = spec.execution_id;
        state.execution_to_swarm.insert(execution_id, swarm_id);

        metrics::counter!("aegis_swarm_child_spawns_total", "result" => "success").increment(1);
        Ok(SpawnedChild {
            agent_id,
            execution_id,
            swarm_id,
        })
    }

    async fn send_message(
        &self,
        tenant_id: &TenantId,
        from: AgentId,
        to: AgentId,
        payload: Vec<u8>,
    ) -> Result<()> {
        let mut state = self.state.write().await;
        // Find which swarm `from` belongs to (tenant-scoped).
        let from_swarm = Self::find_swarm_for_agent(&state, tenant_id, from)?;
        let to_swarm = Self::find_swarm_for_agent(&state, tenant_id, to)?;
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
        tenant_id: &TenantId,
        swarm_id: SwarmId,
        resource: &str,
        holder: AgentId,
        execution_id: ExecutionId,
        ttl: Duration,
    ) -> Result<LockToken> {
        let mut state = self.state.write().await;
        Self::cleanup_expired_locks(&mut state);

        // Validate holder is in the swarm (tenant-scoped).
        let swarm = swarm_for_tenant(&state, tenant_id, swarm_id)?;
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
        state
            .tokens
            .insert(token.0.clone(), (tenant_id.clone(), resource.to_string()));
        Ok(token)
    }

    async fn release_lock(&self, tenant_id: &TenantId, token: LockToken) -> Result<()> {
        let mut state = self.state.write().await;
        // Tenant-scoped: a token issued to tenant-A cannot be released by
        // tenant-B even if they somehow learned the token string.
        let entry = state.tokens.get(&token.0).cloned();
        let Some((token_tenant, resource)) = entry else {
            bail!("unknown swarm lock token");
        };
        if &token_tenant != tenant_id {
            bail!("unknown swarm lock token");
        }
        state.tokens.remove(&token.0);
        state.locks.remove(&resource);
        Ok(())
    }

    async fn cancel_swarm(
        &self,
        tenant_id: &TenantId,
        swarm_id: SwarmId,
        _reason: CancellationReason,
    ) -> Result<()> {
        let mut state = self.state.write().await;
        let member_execution_ids = {
            let swarm = swarm_for_tenant_mut(&mut state, tenant_id, swarm_id)?;
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

    async fn get_swarm(&self, tenant_id: &TenantId, swarm_id: SwarmId) -> Result<Option<Swarm>> {
        let state = self.state.read().await;
        Ok(state
            .swarms
            .get(&swarm_id)
            .filter(|swarm| &swarm.tenant_id == tenant_id)
            .cloned())
    }

    async fn list_child_executions(
        &self,
        tenant_id: &TenantId,
        swarm_id: SwarmId,
    ) -> Result<Vec<ExecutionId>> {
        let state = self.state.read().await;
        let swarm = swarm_for_tenant(&state, tenant_id, swarm_id)?;
        Ok(swarm.member_execution_ids())
    }

    async fn broadcast_message(
        &self,
        tenant_id: &TenantId,
        swarm_id: SwarmId,
        from: AgentId,
        payload: Vec<u8>,
    ) -> Result<()> {
        let mut state = self.state.write().await;
        let swarm = swarm_for_tenant(&state, tenant_id, swarm_id)?;

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
    /// Find which swarm an agent belongs to by scanning swarms owned by
    /// `tenant_id`. Cross-tenant scans are not permitted.
    fn find_swarm_for_agent(
        state: &SwarmState,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<SwarmId> {
        for (swarm_id, swarm) in &state.swarms {
            if &swarm.tenant_id == tenant_id && swarm.contains(agent_id) {
                return Ok(*swarm_id);
            }
        }
        Err(anyhow!("agent {agent_id:?} is not assigned to a swarm"))
    }
}

#[async_trait]
impl SwarmCancellationPort for StandardSwarmService {
    async fn cascade_cancel_for_execution(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
    ) -> anyhow::Result<()> {
        let swarm_id = {
            let state = self.state.read().await;
            // Only return a swarm id if the swarm belongs to the caller's
            // tenant — prevents cross-tenant cascade.
            state
                .execution_to_swarm
                .get(&execution_id)
                .copied()
                .filter(|sid| {
                    state
                        .swarms
                        .get(sid)
                        .is_some_and(|s| &s.tenant_id == tenant_id)
                })
        };

        if let Some(swarm_id) = swarm_id {
            self.cancel_swarm(tenant_id, swarm_id, CancellationReason::ParentCancelled)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::swarm::{SwarmChildSpec, SwarmResourceLimits};
    use aegis_orchestrator_core::domain::execution::{Execution, ExecutionInput};
    use aegis_orchestrator_core::domain::repository::RepositoryError;
    use aegis_orchestrator_core::domain::shared_kernel::WorkflowId;

    fn placeholder_execution(id: ExecutionId, tenant: TenantId) -> Execution {
        let input = ExecutionInput {
            intent: None,
            input: serde_json::Value::Null,
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        };
        let mut exec = Execution::new_with_id(id, AgentId::new(), input, 1, "test".to_string());
        exec.tenant_id = tenant;
        exec
    }

    /// Minimal in-memory `ExecutionRepository` for service-level tests.
    /// Stores executions by `(tenant_id, execution_id)` so the
    /// `find_by_id_for_tenant` contract is faithful and cross-tenant
    /// look-ups return `None`.
    #[derive(Default)]
    struct StubExecutionRepository {
        inner: tokio::sync::Mutex<HashMap<(TenantId, ExecutionId), Execution>>,
    }

    impl StubExecutionRepository {
        fn new() -> Self {
            Self::default()
        }

        async fn insert(&self, tenant_id: TenantId, execution_id: ExecutionId) {
            let exec = placeholder_execution(execution_id, tenant_id.clone());
            self.inner
                .lock()
                .await
                .insert((tenant_id, execution_id), exec);
        }
    }

    #[async_trait]
    impl ExecutionRepository for StubExecutionRepository {
        async fn save_for_tenant(
            &self,
            tenant_id: &TenantId,
            execution: &Execution,
        ) -> std::result::Result<(), RepositoryError> {
            self.inner
                .lock()
                .await
                .insert((tenant_id.clone(), execution.id), execution.clone());
            Ok(())
        }

        async fn find_by_id_for_tenant(
            &self,
            tenant_id: &TenantId,
            id: ExecutionId,
        ) -> std::result::Result<Option<Execution>, RepositoryError> {
            Ok(self
                .inner
                .lock()
                .await
                .get(&(tenant_id.clone(), id))
                .cloned())
        }

        async fn find_by_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _agent_id: AgentId,
            _limit: usize,
        ) -> std::result::Result<Vec<Execution>, RepositoryError> {
            Ok(Vec::new())
        }

        async fn find_by_workflow_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _workflow_id: WorkflowId,
            _limit: usize,
        ) -> std::result::Result<Vec<Execution>, RepositoryError> {
            Ok(Vec::new())
        }

        async fn find_recent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _limit: usize,
        ) -> std::result::Result<Vec<Execution>, RepositoryError> {
            Ok(Vec::new())
        }

        async fn delete_for_tenant(
            &self,
            tenant_id: &TenantId,
            id: ExecutionId,
        ) -> std::result::Result<(), RepositoryError> {
            self.inner.lock().await.remove(&(tenant_id.clone(), id));
            Ok(())
        }

        async fn count_by_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _agent_id: AgentId,
        ) -> std::result::Result<i64, RepositoryError> {
            Ok(0)
        }

        async fn find_by_id_unscoped(
            &self,
            id: ExecutionId,
        ) -> std::result::Result<Option<Execution>, RepositoryError> {
            Ok(self
                .inner
                .lock()
                .await
                .iter()
                .find_map(|((_, exec_id), exec)| (*exec_id == id).then(|| exec.clone())))
        }

        async fn count_running(
            &self,
            _tenant_id: &TenantId,
        ) -> std::result::Result<u64, RepositoryError> {
            Ok(0)
        }
    }

    fn tenant_a() -> TenantId {
        TenantId::consumer()
    }

    fn tenant_b() -> TenantId {
        TenantId::system()
    }

    fn test_child_spec_for(tenant: TenantId) -> SwarmChildSpec {
        SwarmChildSpec {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: tenant,
            name: "child-agent".to_string(),
            language: "python".to_string(),
            version: "3.11".to_string(),
            resource_limits: Some(SwarmResourceLimits {
                cpu: 1000,
                memory: "512Mi".to_string(),
            }),
        }
    }

    fn test_child_spec() -> SwarmChildSpec {
        test_child_spec_for(tenant_a())
    }

    /// Build a service wired with a stub execution repository and a
    /// pre-registered parent execution for `tenant`. Returns the service
    /// and the parent execution id.
    async fn service_with_parent(tenant: &TenantId) -> (StandardSwarmService, ExecutionId) {
        let repo = Arc::new(StubExecutionRepository::new());
        let parent_exec = ExecutionId::new();
        repo.insert(tenant.clone(), parent_exec).await;
        let service = StandardSwarmService::new()
            .with_execution_repository(repo as Arc<dyn ExecutionRepository>);
        (service, parent_exec)
    }

    #[tokio::test]
    async fn creates_swarm_spawns_child_and_records_message() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();
        let spawned = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        service
            .send_message(
                &tenant_a(),
                spawned.agent_id,
                spawned.agent_id,
                b"hello".to_vec(),
            )
            .await
            .unwrap();

        let swarm = service
            .get_swarm(&tenant_a(), swarm_id)
            .await
            .unwrap()
            .unwrap();
        assert!(swarm.contains(spawned.agent_id));
        assert!(swarm.contains_execution(&spawned.execution_id));
        assert_eq!(
            service
                .messages_for_swarm(&tenant_a(), swarm_id)
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn lock_acquire_release_and_contention() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();
        let spawned = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        let ttl = Duration::from_secs(300);
        let token = service
            .acquire_lock(
                &tenant_a(),
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
                &tenant_a(),
                swarm_id,
                "workspace",
                spawned.agent_id,
                spawned.execution_id,
                ttl
            )
            .await
            .is_err());
        service.release_lock(&tenant_a(), token).await.unwrap();
        // After release, should succeed
        assert!(service
            .acquire_lock(
                &tenant_a(),
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
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        let outsider = AgentId::new();
        let outsider_exec = ExecutionId::new();
        let ttl = Duration::from_secs(60);
        let result = service
            .acquire_lock(
                &tenant_a(),
                swarm_id,
                "resource",
                outsider,
                outsider_exec,
                ttl,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_swarm_releases_locks_and_cleans_execution_map() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();
        let spawned = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        // Acquire a lock
        let ttl = Duration::from_secs(300);
        let _token = service
            .acquire_lock(
                &tenant_a(),
                swarm_id,
                "shared-file",
                spawned.agent_id,
                spawned.execution_id,
                ttl,
            )
            .await
            .unwrap();

        service
            .cancel_swarm(&tenant_a(), swarm_id, CancellationReason::Manual)
            .await
            .unwrap();

        let swarm = service
            .get_swarm(&tenant_a(), swarm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(swarm.status, SwarmStatus::Dissolved);
        assert!(swarm.dissolved_at.is_some());

        // Locks should be cleaned up
        let locks = service.locks_for_swarm(&tenant_a(), swarm_id).await;
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn list_child_executions_returns_correct_ids() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        let spawned1 = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();
        let spawned2 = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        let exec_ids = service
            .list_child_executions(&tenant_a(), swarm_id)
            .await
            .unwrap();
        assert_eq!(exec_ids.len(), 2);
        assert!(exec_ids.contains(&spawned1.execution_id));
        assert!(exec_ids.contains(&spawned2.execution_id));
    }

    #[tokio::test]
    async fn broadcast_message_delivers_to_all_except_sender() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        let spawned1 = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();
        let spawned2 = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();
        let spawned3 = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        service
            .broadcast_message(
                &tenant_a(),
                swarm_id,
                spawned1.agent_id,
                b"broadcast-payload".to_vec(),
            )
            .await
            .unwrap();

        let messages = service.messages_for_swarm(&tenant_a(), swarm_id).await;
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
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        let spawned = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await
            .unwrap();

        assert_eq!(spawned.swarm_id, swarm_id);
    }

    #[tokio::test]
    async fn spawn_child_on_dissolved_swarm_fails() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        service
            .cancel_swarm(&tenant_a(), swarm_id, CancellationReason::Manual)
            .await
            .unwrap();

        let result = service
            .spawn_child(&tenant_a(), swarm_id, test_child_spec(), None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_swarm_returns_none_for_unknown() {
        let service = StandardSwarmService::new();
        let result = service
            .get_swarm(&tenant_a(), SwarmId::new())
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_child_executions_fails_for_unknown_swarm() {
        let service = StandardSwarmService::new();
        let result = service
            .list_child_executions(&tenant_a(), SwarmId::new())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn duplicate_parent_execution_rejected() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        service.create_swarm(parent_exec, tenant_a()).await.unwrap();
        let result = service.create_swarm(parent_exec, tenant_a()).await;
        assert!(result.is_err());
    }

    // ──────────────────────────────────────────────────────────────────
    // Audit 002 regression tests
    // ──────────────────────────────────────────────────────────────────

    /// Audit 002 §4.14 — `spawn_child` must reject a `SwarmChildSpec`
    /// whose `tenant_id` does not match the swarm's tenant. Before the
    /// fix, the spec was discarded and `swarm.add_member` was called
    /// directly, silently accepting cross-tenant children.
    #[tokio::test]
    async fn audit_4_14_spawn_child_rejects_cross_tenant_spec() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        // Spec carries tenant_b but the swarm is tenant_a — must be rejected.
        let foreign_spec = test_child_spec_for(tenant_b());
        let result = service
            .spawn_child(&tenant_a(), swarm_id, foreign_spec, None)
            .await;
        assert!(
            result.is_err(),
            "spawn_child must reject a child spec whose tenant_id differs from the swarm's tenant"
        );

        // The swarm must remain empty: no member smuggled in.
        let swarm = service
            .get_swarm(&tenant_a(), swarm_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            swarm.member_ids().is_empty(),
            "rejected cross-tenant spec must not be added to swarm members"
        );
    }

    /// Audit 002 §4.33 — `create_swarm` must verify that
    /// `parent_execution_id` belongs to the caller's tenant.
    #[tokio::test]
    async fn audit_4_33_create_swarm_rejects_foreign_parent_execution() {
        // Parent execution lives under tenant_b.
        let repo = Arc::new(StubExecutionRepository::new());
        let foreign_parent = ExecutionId::new();
        repo.insert(tenant_b(), foreign_parent).await;

        let service = StandardSwarmService::new()
            .with_execution_repository(repo as Arc<dyn ExecutionRepository>);

        // Tenant-A caller attempts to claim tenant-B's execution as a parent.
        let result = service.create_swarm(foreign_parent, tenant_a()).await;
        assert!(
            result.is_err(),
            "create_swarm must reject a parent execution that does not belong to the caller's tenant"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not found"),
            "error must not leak that the execution exists in another tenant: {err_msg}"
        );
    }

    /// Audit 002 §4.34 — every `SwarmId`-keyed method must scope by
    /// `(tenant_id, swarm_id)`. We assert one example per call type:
    ///
    /// * Read    — `get_swarm` returns `None` across tenants
    /// * Read    — `list_child_executions` returns `Err` across tenants
    /// * Write   — `spawn_child` returns `Err` across tenants
    /// * Delete  — `cancel_swarm` returns `Err` across tenants and
    ///             leaves the swarm `Active`.
    #[tokio::test]
    async fn audit_4_34_swarm_lookups_are_tenant_scoped() {
        let (service, parent_exec) = service_with_parent(&tenant_a()).await;
        // Also pre-register a parent execution for tenant_b so the stub
        // repo can satisfy a (separate) create_swarm call if needed.
        let swarm_id = service.create_swarm(parent_exec, tenant_a()).await.unwrap();

        // Read: get_swarm — tenant_b sees nothing.
        let cross_get = service.get_swarm(&tenant_b(), swarm_id).await.unwrap();
        assert!(
            cross_get.is_none(),
            "get_swarm must return None for a swarm owned by another tenant"
        );

        // Read: list_child_executions — tenant_b is rejected.
        let cross_list = service.list_child_executions(&tenant_b(), swarm_id).await;
        assert!(
            cross_list.is_err(),
            "list_child_executions must fail across tenants"
        );

        // Write: spawn_child — tenant_b is rejected even with a valid spec.
        let spec_for_b = test_child_spec_for(tenant_b());
        let cross_spawn = service
            .spawn_child(&tenant_b(), swarm_id, spec_for_b, None)
            .await;
        assert!(cross_spawn.is_err(), "spawn_child must fail across tenants");

        // Delete: cancel_swarm — tenant_b is rejected and the swarm
        // remains Active for the rightful owner.
        let cross_cancel = service
            .cancel_swarm(&tenant_b(), swarm_id, CancellationReason::Manual)
            .await;
        assert!(
            cross_cancel.is_err(),
            "cancel_swarm must fail across tenants"
        );
        let swarm_after = service
            .get_swarm(&tenant_a(), swarm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            swarm_after.status,
            SwarmStatus::Active,
            "cross-tenant cancel_swarm must not mutate the target swarm"
        );

        // Inherent read: messages_for_swarm and locks_for_swarm hide content.
        assert!(service
            .messages_for_swarm(&tenant_b(), swarm_id)
            .await
            .is_empty());
        assert!(service
            .locks_for_swarm(&tenant_b(), swarm_id)
            .await
            .is_empty());

        // list_swarms is partitioned by tenant.
        assert!(service.list_swarms(&tenant_b()).await.is_empty());
        assert_eq!(service.list_swarms(&tenant_a()).await.len(), 1);
    }
}
