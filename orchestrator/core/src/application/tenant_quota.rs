// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # TenantQuotaService (ADR-056 §Quota Enforcement)
//!
//! Enforces per-tenant resource quotas for agents and concurrent executions.
//!
//! ## Design
//!
//! Two check methods are provided — one per quota dimension — so that callers
//! can fail fast with a precise error before creating any domain resources.
//!
//! Both methods publish [`TenantEvent::TenantQuotaExceeded`] on the event bus
//! before returning the error so that observability and alerting pipelines
//! receive real-time quota signals.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::events::TenantEvent;
use crate::domain::repository::{
    AgentRepository, ExecutionRepository, RepositoryError, TenantRepository,
};
use crate::domain::tenancy::TenantQuotaKind;
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("quota exceeded for {kind:?}: current {current}, limit {limit}")]
    Exceeded {
        kind: TenantQuotaKind,
        current: u64,
        limit: u64,
    },
    #[error("tenant not found: {0}")]
    TenantNotFound(String),
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
}

pub struct TenantQuotaService {
    tenant_repo: Arc<dyn TenantRepository>,
    agent_repo: Arc<dyn AgentRepository>,
    execution_repo: Arc<dyn ExecutionRepository>,
    event_bus: Arc<EventBus>,
}

impl TenantQuotaService {
    pub fn new(
        tenant_repo: Arc<dyn TenantRepository>,
        agent_repo: Arc<dyn AgentRepository>,
        execution_repo: Arc<dyn ExecutionRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            tenant_repo,
            agent_repo,
            execution_repo,
            event_bus,
        }
    }

    /// Check whether deploying a new agent would exceed the tenant's `max_agents` quota.
    ///
    /// Fetches the current active agent count and compares it against the tenant's configured
    /// quota. Publishes [`TenantEvent::TenantQuotaExceeded`] and returns `Err(QuotaError::Exceeded)`
    /// when the limit is reached or exceeded.
    pub async fn check_agent_quota(&self, tenant_id: &TenantId) -> Result<(), QuotaError> {
        let tenant = self
            .tenant_repo
            .find_by_slug(tenant_id)
            .await?
            .ok_or_else(|| QuotaError::TenantNotFound(tenant_id.as_str().to_string()))?;

        let current = self.agent_repo.count_active(tenant_id).await?;
        let limit = tenant.quotas.max_agents as u64;

        if current >= limit {
            self.event_bus
                .publish_tenant_event(TenantEvent::TenantQuotaExceeded {
                    tenant_slug: tenant_id.as_str().to_string(),
                    quota_kind: TenantQuotaKind::TotalAgents,
                    current_value: current,
                    limit,
                    exceeded_at: Utc::now(),
                });
            return Err(QuotaError::Exceeded {
                kind: TenantQuotaKind::TotalAgents,
                current,
                limit,
            });
        }

        Ok(())
    }

    /// Check whether starting a new execution would exceed the tenant's `max_concurrent_executions` quota.
    ///
    /// Counts running and pending executions and compares against the configured limit.
    /// Publishes [`TenantEvent::TenantQuotaExceeded`] and returns `Err(QuotaError::Exceeded)`
    /// when the limit is reached or exceeded.
    pub async fn check_execution_quota(&self, tenant_id: &TenantId) -> Result<(), QuotaError> {
        let tenant = self
            .tenant_repo
            .find_by_slug(tenant_id)
            .await?
            .ok_or_else(|| QuotaError::TenantNotFound(tenant_id.as_str().to_string()))?;

        let current = self.execution_repo.count_running(tenant_id).await?;
        let limit = tenant.quotas.max_concurrent_executions as u64;

        if current >= limit {
            self.event_bus
                .publish_tenant_event(TenantEvent::TenantQuotaExceeded {
                    tenant_slug: tenant_id.as_str().to_string(),
                    quota_kind: TenantQuotaKind::ConcurrentExecutions,
                    current_value: current,
                    limit,
                    exceeded_at: Utc::now(),
                });
            return Err(QuotaError::Exceeded {
                kind: TenantQuotaKind::ConcurrentExecutions,
                current,
                limit,
            });
        }

        Ok(())
    }
}
