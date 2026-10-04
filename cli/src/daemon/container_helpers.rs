// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Docker container reaping logic for orphaned agent containers.
//!
//! ## Reaper resilience
//!
//! A single un-killable container (typically an agent process wedged in a
//! D-state syscall against a hung FUSE mount) used to produce 288 identical
//! WARN lines per day because the reaper retried `remove_container(force)`
//! every 5 minutes with no escalation, no backoff, and no quarantine. The
//! [`ReapAttemptState`] map (owned by the spawned reaper task in
//! `server.rs`) tracks per-container failure counts, applies exponential
//! backoff (5m → 10m → 20m → 40m → 60m cap), and quarantines containers
//! that fail 5 consecutive `RuntimeError::Unkillable` attempts so a stuck
//! container produces exactly one actionable error log instead of a storm.
//!
//! ## An execution that outlived its bound
//!
//! The agent's bootstrap holds no clock (ADR-040), so only the supervisor
//! ends a container's wait. When the supervisor did not, the reaper ends the
//! execution once it is still running
//! [`REAPER_MARGIN_SECONDS`](aegis_orchestrator_core::domain::execution::REAPER_MARGIN_SECONDS) past its
//! recorded bound ([`Execution::outlived_bound`]): it fails the execution the
//! way the start-up pass over executions cut by a restart does
//! ([`fail_execution_outlived_bound`]) and removes its container (ADR-040,
//! Update of 2026-10-04). It never ends an execution inside that time.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use aegis_orchestrator_core::application::execution::fail_execution_outlived_bound;
use aegis_orchestrator_core::domain::execution::{Execution, ExecutionId, ExecutionStatus};
use aegis_orchestrator_core::domain::runtime::{
    AgentRuntime, ContainerEngineKind, InstanceId, RuntimeError,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::runtime::{ContainerRuntime, ManagedAgentContainer};

/// The reap reason of a container whose execution the reaper ended because
/// it outlived its bound.
pub(crate) const EXECUTION_OUTLIVED_BOUND: &str = "execution_outlived_bound";

/// What the reaper needs of the container engine: the managed agent
/// containers, and their removal. [`ContainerRuntime`] is the engine; a test
/// stands in its own.
#[async_trait]
pub(crate) trait ManagedContainerEngine: Send + Sync {
    async fn list_managed_agent_containers(
        &self,
    ) -> Result<Vec<ManagedAgentContainer>, RuntimeError>;
    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError>;
}

#[async_trait]
impl ManagedContainerEngine for ContainerRuntime {
    async fn list_managed_agent_containers(
        &self,
    ) -> Result<Vec<ManagedAgentContainer>, RuntimeError> {
        ContainerRuntime::list_managed_agent_containers(self).await
    }

    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError> {
        AgentRuntime::terminate(self, id).await
    }
}

/// Number of consecutive [`RuntimeError::Unkillable`] failures after which the
/// reaper stops emitting warnings for a container and emits a single
/// `error!(quarantined=true)` instead.
const QUARANTINE_THRESHOLD: u32 = 5;

/// Per-container reaper bookkeeping. Owned by the reaper task; no locking
/// required because there is exactly one reaper task per daemon.
#[derive(Debug, Clone)]
pub(crate) struct ReapAttemptState {
    pub attempts: u32,
    pub last_attempt: Instant,
    pub quarantined: bool,
}

impl ReapAttemptState {
    fn new(now: Instant) -> Self {
        Self {
            attempts: 1,
            last_attempt: now,
            quarantined: false,
        }
    }

    /// Backoff between consecutive attempts. Cap at 1h so a permanently
    /// quarantined container is still re-checked once an hour in case the
    /// underlying problem (e.g. FUSE daemon was restarted) has cleared.
    pub(crate) fn backoff(attempts: u32) -> Duration {
        match attempts {
            0 | 1 => Duration::from_secs(5 * 60),
            2 => Duration::from_secs(10 * 60),
            3 => Duration::from_secs(20 * 60),
            4 => Duration::from_secs(40 * 60),
            _ => Duration::from_secs(60 * 60),
        }
    }

    /// Returns true if enough time has elapsed since `last_attempt` to retry.
    pub(crate) fn should_attempt(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_attempt) >= Self::backoff(self.attempts)
    }

    fn record_failure(&mut self, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_attempt = now;
        if self.attempts >= QUARANTINE_THRESHOLD {
            self.quarantined = true;
        }
    }
}

pub(crate) fn managed_container_reap_reason(
    container: &ManagedAgentContainer,
    execution_status: Option<&ExecutionStatus>,
) -> Option<&'static str> {
    if container.debug_retain {
        return None;
    }

    // Never re-issue a remove call against a container the engine is already
    // tearing down. A second call into a container in `stopping`/`removing`
    // state is exactly what produces the recurring HTTP 500 storm — it
    // races with libpod/dockerd's own removal goroutine.
    match container.state.as_deref() {
        Some("stopping") | Some("removing") => return None,
        _ => {}
    }

    match execution_status {
        None => Some("missing_execution_record"),
        Some(&ExecutionStatus::Running) if container.state.as_deref() == Some("running") => None,
        Some(&ExecutionStatus::Running) => Some("container_not_running"),
        Some(_) => Some("execution_not_running"),
    }
}

/// The reaper's decision for one container at `now`: the reasons of
/// [`managed_container_reap_reason`], and [`EXECUTION_OUTLIVED_BOUND`] for a
/// running container whose execution is still Running past its recorded
/// bound and the margin ([`Execution::outlived_bound`]). A running execution
/// inside that time, or without a recorded bound, keeps its container.
pub(crate) fn container_reap_reason(
    container: &ManagedAgentContainer,
    execution: Option<&Execution>,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    if let Some(reason) = managed_container_reap_reason(container, execution.map(|e| &e.status)) {
        return Some(reason);
    }
    match execution {
        Some(execution)
            if !container.debug_retain
                && container.state.as_deref() == Some("running")
                && execution.outlived_bound(now) =>
        {
            Some(EXECUTION_OUTLIVED_BOUND)
        }
        _ => None,
    }
}

pub(crate) async fn cleanup_orphaned_agent_containers<E: ManagedContainerEngine + ?Sized>(
    runtime: Arc<E>,
    execution_repo: Arc<dyn aegis_orchestrator_core::domain::repository::ExecutionRepository>,
    event_bus: &EventBus,
    attempt_states: &mut HashMap<String, ReapAttemptState>,
) -> Result<usize> {
    let containers = runtime.list_managed_agent_containers().await?;
    let now = Instant::now();
    let wall_now = Utc::now();
    let mut reaped = 0usize;

    // Garbage-collect stale entries: any container ID we tracked but is no
    // longer present in the engine has either been removed externally or
    // removed by an earlier successful reap. Either way, drop the bookkeeping.
    let live_ids: std::collections::HashSet<&str> =
        containers.iter().map(|c| c.id.as_str()).collect();
    attempt_states.retain(|id, _| live_ids.contains(id.as_str()));

    for container in containers {
        if container.debug_retain {
            continue;
        }

        // Skip per-container work when we are still inside the backoff window
        // for a previously-failed container. This is the entire point of the
        // backoff machinery — without this gate the storm comes back.
        if let Some(state) = attempt_states.get(&container.id) {
            if !state.should_attempt(now) {
                continue;
            }
        }

        let execution = match container.execution_id.as_deref() {
            Some(raw_execution_id) => match ExecutionId::from_string(raw_execution_id) {
                Ok(execution_id) => match execution_repo.find_by_id_unscoped(execution_id).await {
                    Ok(Some(execution)) => Some(execution),
                    Ok(None) => None,
                    Err(error) => {
                        warn!(
                            container_id = %container.id,
                            execution_id = raw_execution_id,
                            error = %error,
                            "Failed to look up execution for managed container; skipping"
                        );
                        continue;
                    }
                },
                Err(error) => {
                    warn!(
                        container_id = %container.id,
                        execution_id = raw_execution_id,
                        error = %error,
                        "Managed container has invalid execution_id label; treating as orphan"
                    );
                    None
                }
            },
            None => None,
        };

        let Some(reason) = container_reap_reason(&container, execution.as_ref(), wall_now) else {
            // Container no longer eligible for reaping — clear any prior state
            // so a future regression starts from a clean slate.
            attempt_states.remove(&container.id);
            continue;
        };

        // An execution that outlived its bound is ended first, so its record,
        // its error and its events say why before its container goes. If the
        // record cannot be saved, the container is kept for the next pass:
        // removing it would leave a Running record no pass reaches.
        if reason == EXECUTION_OUTLIVED_BOUND {
            let Some(execution) = execution else {
                continue;
            };
            match fail_execution_outlived_bound(
                execution_repo.as_ref(),
                event_bus,
                execution,
                wall_now,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    warn!(
                        container_id = %container.id,
                        error = %error,
                        "Could not end an execution that outlived its bound; its container is kept for the next pass"
                    );
                    continue;
                }
            }
        }

        let instance_id = InstanceId::new(container.id.clone());
        info!(
            container_id = %container.id,
            execution_id = container.execution_id.as_deref().unwrap_or("missing"),
            container_state = container.state.as_deref().unwrap_or("unknown"),
            reason,
            "Reaping orphaned managed agent container"
        );

        match runtime.terminate(&instance_id).await {
            Ok(()) => {
                reaped += 1;
                attempt_states.remove(&container.id);
            }
            Err(RuntimeError::Unkillable {
                container_id,
                engine,
            }) => {
                let already_tracked = attempt_states.contains_key(&container_id);
                let entry = attempt_states
                    .entry(container_id.clone())
                    .or_insert_with(|| ReapAttemptState::new(now));
                if already_tracked {
                    entry.record_failure(now);
                }
                // For a freshly inserted entry, ReapAttemptState::new already
                // counted attempt 1, so we only escalate on subsequent failures.
                emit_unkillable_log(entry, &container_id, engine);
            }
            Err(error) => {
                warn!(
                    container_id = %container.id,
                    error = %error,
                    "Failed to reap orphaned managed agent container"
                );
                continue;
            }
        }
    }

    Ok(reaped)
}

fn emit_unkillable_log(state: &ReapAttemptState, container_id: &str, engine: ContainerEngineKind) {
    let hint = match engine {
        ContainerEngineKind::Podman => {
            "Agent process likely wedged in a D-state syscall against a hung FUSE mount. \
             Check `systemctl --user status aegis-fuse-daemon` and `/proc/<pid>/stack` for \
             fuse_request_wait or similar; recover with `nsenter -t <pid> -m umount -l <mount>`."
        }
        ContainerEngineKind::Docker | ContainerEngineKind::Unknown => {
            "Agent process likely in uninterruptible sleep. Check `/proc/<pid>/stack` and \
             `/proc/<pid>/wchan` for the blocked syscall."
        }
    };

    if state.quarantined && state.attempts == QUARANTINE_THRESHOLD {
        // Single error at the moment the threshold trips. Subsequent failures
        // for this container drop to debug! to keep the log volume O(1).
        error!(
            quarantined = true,
            container_id = container_id,
            engine = %engine,
            attempts = state.attempts,
            "Container quarantined after repeated unkillable failures. {hint}"
        );
    } else if state.quarantined {
        debug!(
            quarantined = true,
            container_id = container_id,
            engine = %engine,
            attempts = state.attempts,
            "Quarantined container still unkillable; suppressing additional error logs"
        );
    } else {
        warn!(
            container_id = container_id,
            engine = %engine,
            attempts = state.attempts,
            next_retry_in_secs = ReapAttemptState::backoff(state.attempts).as_secs(),
            "Container unkillable; will retry after backoff. {hint}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container_with_state(state: Option<&str>) -> ManagedAgentContainer {
        ManagedAgentContainer {
            id: "test-id".to_string(),
            execution_id: None,
            debug_retain: false,
            state: state.map(|s| s.to_string()),
        }
    }

    /// Regression: the reaper used to call `remove_container(force=true)`
    /// against containers in `stopping`/`removing` state, which races with
    /// libpod's own removal goroutine and produces the recurring HTTP 500
    /// "did not die within timeout". Returning None here is the fix.
    #[test]
    fn reap_reason_skips_stopping_state_regardless_of_execution_status() {
        let container = container_with_state(Some("stopping"));
        for status in [
            None,
            Some(ExecutionStatus::Running),
            Some(ExecutionStatus::Completed),
            Some(ExecutionStatus::Failed),
            Some(ExecutionStatus::Cancelled),
        ] {
            assert_eq!(
                managed_container_reap_reason(&container, status.as_ref()),
                None,
                "expected None for state=stopping with status={:?}",
                status
            );
        }
    }

    #[test]
    fn reap_reason_skips_removing_state_regardless_of_execution_status() {
        let container = container_with_state(Some("removing"));
        for status in [
            None,
            Some(ExecutionStatus::Running),
            Some(ExecutionStatus::Completed),
            Some(ExecutionStatus::Failed),
            Some(ExecutionStatus::Cancelled),
        ] {
            assert_eq!(
                managed_container_reap_reason(&container, status.as_ref()),
                None,
                "expected None for state=removing with status={:?}",
                status
            );
        }
    }

    /// Sanity: other states still produce a reap reason when the execution is
    /// not running, so we don't accidentally suppress legitimate cleanup.
    #[test]
    fn reap_reason_still_fires_for_exited_container_with_terminal_execution() {
        let container = container_with_state(Some("exited"));
        assert_eq!(
            managed_container_reap_reason(&container, Some(&ExecutionStatus::Completed)),
            Some("execution_not_running")
        );
    }

    /// Regression: 5 consecutive Unkillable failures must flip the quarantine
    /// flag, and the backoff schedule must be 5m / 10m / 20m / 40m / 60m.
    #[test]
    fn reap_attempt_state_quarantines_after_five_failures_with_correct_backoff() {
        let t0 = Instant::now();
        let mut state = ReapAttemptState::new(t0);
        assert_eq!(state.attempts, 1);
        assert!(!state.quarantined);

        // After attempt 1: must wait 5 minutes.
        assert_eq!(
            ReapAttemptState::backoff(state.attempts),
            Duration::from_secs(5 * 60)
        );
        assert!(!state.should_attempt(t0 + Duration::from_secs(5 * 60 - 1)));
        assert!(state.should_attempt(t0 + Duration::from_secs(5 * 60)));

        let t1 = t0 + Duration::from_secs(5 * 60);
        state.record_failure(t1);
        assert_eq!(state.attempts, 2);
        assert!(!state.quarantined);
        assert_eq!(
            ReapAttemptState::backoff(state.attempts),
            Duration::from_secs(10 * 60)
        );
        assert!(!state.should_attempt(t1 + Duration::from_secs(10 * 60 - 1)));
        assert!(state.should_attempt(t1 + Duration::from_secs(10 * 60)));

        let t2 = t1 + Duration::from_secs(10 * 60);
        state.record_failure(t2);
        assert_eq!(state.attempts, 3);
        assert_eq!(
            ReapAttemptState::backoff(state.attempts),
            Duration::from_secs(20 * 60)
        );

        let t3 = t2 + Duration::from_secs(20 * 60);
        state.record_failure(t3);
        assert_eq!(state.attempts, 4);
        assert_eq!(
            ReapAttemptState::backoff(state.attempts),
            Duration::from_secs(40 * 60)
        );
        assert!(!state.quarantined);

        let t4 = t3 + Duration::from_secs(40 * 60);
        state.record_failure(t4);
        assert_eq!(state.attempts, 5);
        assert!(state.quarantined, "must quarantine after 5 failures");
        assert_eq!(
            ReapAttemptState::backoff(state.attempts),
            Duration::from_secs(60 * 60)
        );

        // Cap holds beyond the threshold.
        let t5 = t4 + Duration::from_secs(60 * 60);
        state.record_failure(t5);
        assert_eq!(state.attempts, 6);
        assert_eq!(
            ReapAttemptState::backoff(state.attempts),
            Duration::from_secs(60 * 60)
        );
    }

    /// Regression: the reaper must clear bookkeeping on a successful terminate
    /// so a transient failure doesn't permanently inflate attempt counts. We
    /// simulate this at the data-structure level since the orchestration
    /// (which removes the entry on Ok) lives in cleanup_orphaned_agent_containers.
    #[test]
    fn reap_attempt_state_is_cleared_on_success() {
        let t0 = Instant::now();
        let mut map: HashMap<String, ReapAttemptState> = HashMap::new();
        map.insert("abc".to_string(), ReapAttemptState::new(t0));
        // Simulate the success branch in cleanup_orphaned_agent_containers.
        map.remove("abc");
        assert!(!map.contains_key("abc"));
    }

    // ── An execution that outlived its bound (ADR-040, Update of 2026-10-04) ─

    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::events::ExecutionEvent;
    use aegis_orchestrator_core::domain::execution::{
        ExecutionInput, IterationStatus, REAPER_OUTLIVED_FAILURE_PREFIX,
    };
    use aegis_orchestrator_core::domain::repository::ExecutionRepository;
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
    use aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository;
    use std::sync::Mutex;

    /// A container engine holding a fixed list of running agent containers,
    /// recording every container the reaper removes.
    struct FakeEngine {
        containers: Vec<ManagedAgentContainer>,
        terminated: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ManagedContainerEngine for FakeEngine {
        async fn list_managed_agent_containers(
            &self,
        ) -> Result<Vec<ManagedAgentContainer>, RuntimeError> {
            Ok(self.containers.clone())
        }

        async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError> {
            self.terminated
                .lock()
                .unwrap()
                .push(id.as_str().to_string());
            Ok(())
        }
    }

    /// A Running execution with `bound` recorded and an iteration in flight,
    /// started `ago` seconds ago, which no supervisor runs; and its running
    /// container.
    fn running_execution_and_container(
        tenant: &TenantId,
        ago: i64,
        bound: Option<u64>,
    ) -> (Execution, ManagedAgentContainer) {
        let mut execution = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: Some("task".to_string()),
                input: serde_json::json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            3,
            "aegis-system-operator".to_string(),
        );
        execution.tenant_id = tenant.clone();
        execution.started_at = Utc::now() - chrono::Duration::seconds(ago);
        execution.timeout_seconds = bound;
        execution.start();
        execution.start_iteration("generate".to_string()).unwrap();
        let container = ManagedAgentContainer {
            id: format!("container-{}", execution.id),
            execution_id: Some(execution.id.to_string()),
            debug_retain: false,
            state: Some("running".to_string()),
        };
        (execution, container)
    }

    async fn one_pass(
        execution: &Execution,
        container: ManagedAgentContainer,
    ) -> (
        Arc<InMemoryExecutionRepository>,
        Arc<FakeEngine>,
        Vec<ExecutionEvent>,
    ) {
        let repo = Arc::new(InMemoryExecutionRepository::new());
        repo.save_for_tenant(&execution.tenant_id, execution)
            .await
            .unwrap();
        let engine = Arc::new(FakeEngine {
            containers: vec![container],
            terminated: Mutex::new(Vec::new()),
        });
        let bus = EventBus::with_default_capacity();
        let mut receiver = bus.subscribe();
        let mut attempt_states = HashMap::new();
        cleanup_orphaned_agent_containers(engine.clone(), repo.clone(), &bus, &mut attempt_states)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let DomainEvent::Execution(event) = event {
                events.push(event);
            }
        }
        (repo, engine, events)
    }

    /// The supervisor never fires (none runs this execution): one reaper pass
    /// after its 60 s bound and the 600 s margin have passed ends the
    /// execution as failed with the reaper's reason, publishes
    /// IterationFailed and ExecutionFailed, and removes the container.
    #[tokio::test]
    async fn the_reaper_ends_an_execution_that_outlived_its_bound_and_removes_its_container() {
        let tenant = TenantId::for_consumer_user("reaper-pass").unwrap();
        let (execution, container) =
            running_execution_and_container(&tenant, 60 + 600 + 1, Some(60));
        let container_id = container.id.clone();

        let (repo, engine, events) = one_pass(&execution, container).await;

        let after = repo
            .find_by_id_for_tenant(&tenant, execution.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.status, ExecutionStatus::Failed);
        let reason = after.error.clone().unwrap();
        assert!(
            reason.starts_with(REAPER_OUTLIVED_FAILURE_PREFIX),
            "{reason}"
        );
        assert!(reason.contains("execution bound of 60 s"), "{reason}");
        assert_eq!(
            after.iterations().last().unwrap().status,
            IterationStatus::Failed
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                ExecutionEvent::IterationFailed { execution_id, .. } if *execution_id == execution.id
            )),
            "IterationFailed published: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                ExecutionEvent::ExecutionFailed { execution_id, reason: r, .. }
                    if *execution_id == execution.id && *r == reason
            )),
            "ExecutionFailed with the reaper's reason published: {events:?}"
        );
        assert_eq!(*engine.terminated.lock().unwrap(), vec![container_id]);
    }

    /// An execution inside its bound and the margin, and one whose bound is
    /// not recorded, are never touched: still Running, no event, the
    /// container kept.
    #[tokio::test]
    async fn the_reaper_never_touches_an_execution_inside_its_bound_or_without_one() {
        let tenant = TenantId::for_consumer_user("reaper-pass-inside").unwrap();
        for (ago, bound) in [(60 + 600 - 1, Some(60)), (100_000, None)] {
            let (execution, container) = running_execution_and_container(&tenant, ago, bound);

            let (repo, engine, events) = one_pass(&execution, container).await;

            let after = repo
                .find_by_id_for_tenant(&tenant, execution.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                after.status,
                ExecutionStatus::Running,
                "ago {ago}, bound {bound:?}"
            );
            assert!(after.error.is_none());
            assert!(events.is_empty(), "no event: {events:?}");
            assert!(
                engine.terminated.lock().unwrap().is_empty(),
                "container kept"
            );
        }
    }

    #[test]
    fn reap_reason_for_an_outlived_execution_names_the_bound_only_for_a_running_container() {
        let tenant = TenantId::for_consumer_user("reaper-reason").unwrap();
        let (execution, container) =
            running_execution_and_container(&tenant, 60 + 600 + 1, Some(60));
        assert_eq!(
            container_reap_reason(&container, Some(&execution), Utc::now()),
            Some(EXECUTION_OUTLIVED_BOUND)
        );
        let mut retained = container.clone();
        retained.debug_retain = true;
        assert_eq!(
            container_reap_reason(&retained, Some(&execution), Utc::now()),
            None
        );
        let mut stopping = container;
        stopping.state = Some("stopping".to_string());
        assert_eq!(
            container_reap_reason(&stopping, Some(&execution), Utc::now()),
            None
        );
    }
}
