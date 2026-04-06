// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agent Scope Service (ADR-076 mirror)
//!
//! Application service for promoting and demoting agent visibility scope.
//! Enforces authorization rules and the two-hop prohibition.
//! Mirrors `WorkflowScopeService` for the Agent bounded context.

use crate::application::scope_requester::ScopeChangeRequester;
use crate::domain::agent::{AgentId, AgentScope};
use crate::domain::events::AgentLifecycleEvent;
use crate::domain::repository::{AgentRepository, RepositoryError};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentScopeChangeError {
    #[error("Unauthorized: {reason}")]
    Unauthorized { reason: String },

    #[error("Name collision: agent '{name}' already exists at target scope")]
    NameCollision { existing_id: AgentId, name: String },

    #[error("Agent not found")]
    NotFound,

    #[error("Invalid scope transition from {from} to {to}: must traverse through Tenant")]
    InvalidTransition { from: String, to: String },

    #[error("Repository error: {0}")]
    Repository(#[from] RepositoryError),
}

pub struct AgentScopeService {
    agent_repo: Arc<dyn AgentRepository>,
    event_bus: Arc<EventBus>,
}

impl AgentScopeService {
    pub fn new(agent_repo: Arc<dyn AgentRepository>, event_bus: Arc<EventBus>) -> Self {
        Self {
            agent_repo,
            event_bus,
        }
    }

    /// Change the scope of an agent with full authorization and collision checks.
    pub async fn change_scope(
        &self,
        agent_id: AgentId,
        target_scope: AgentScope,
        requester: &ScopeChangeRequester,
    ) -> Result<(), AgentScopeChangeError> {
        // 1. Load the agent
        let agent = self
            .agent_repo
            .find_by_id_for_tenant(&requester.tenant_id, agent_id)
            .await?
            .ok_or(AgentScopeChangeError::NotFound)?;

        let current_scope = &agent.scope;

        // 2. Validate transition (two-hop prohibition)
        Self::validate_transition(current_scope, &target_scope)?;

        // 3. Check authorization
        Self::check_authorization(current_scope, &target_scope, requester)?;

        // 4. Determine new tenant_id
        let new_tenant_id = match &target_scope {
            AgentScope::Global => TenantId::system(),
            _ => requester.tenant_id.clone(),
        };

        // 5. Check for name collision at target scope
        self.check_name_collision(&new_tenant_id, &agent.name, &target_scope)
            .await?;

        // 6. Perform the scope update
        self.agent_repo
            .update_scope(agent_id, target_scope.clone(), &new_tenant_id)
            .await?;

        // 7. Publish domain event
        self.event_bus
            .publish_agent_event(AgentLifecycleEvent::AgentScopeChanged {
                agent_id,
                agent_name: agent.name.clone(),
                previous_scope: current_scope.clone(),
                new_scope: target_scope,
                previous_tenant_id: agent.tenant_id.clone(),
                new_tenant_id,
                changed_by: requester.user_id.clone(),
                changed_at: Utc::now(),
            });

        Ok(())
    }

    fn validate_transition(
        from: &AgentScope,
        to: &AgentScope,
    ) -> Result<(), AgentScopeChangeError> {
        // Only Tenant <-> Global transitions are valid
        if from == to {
            return Err(AgentScopeChangeError::InvalidTransition {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        Ok(())
    }

    fn check_authorization(
        _from: &AgentScope,
        to: &AgentScope,
        requester: &ScopeChangeRequester,
    ) -> Result<(), AgentScopeChangeError> {
        match to {
            // Promoting to Global or demoting from Global: operator/admin only
            AgentScope::Global | AgentScope::Tenant => {
                if !requester.is_operator_or_admin() {
                    return Err(AgentScopeChangeError::Unauthorized {
                        reason: "Scope changes require aegis:operator or aegis:admin role"
                            .to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    async fn check_name_collision(
        &self,
        target_tenant_id: &TenantId,
        name: &str,
        target_scope: &AgentScope,
    ) -> Result<(), AgentScopeChangeError> {
        let existing = self
            .agent_repo
            .resolve_by_name(target_tenant_id, name)
            .await?;

        if let Some(existing_agent) = existing {
            // Only a collision if it's at the exact target scope level
            if existing_agent.scope.as_db_str() == target_scope.as_db_str() {
                return Err(AgentScopeChangeError::NameCollision {
                    existing_id: existing_agent.id,
                    name: name.to_string(),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::event_bus::EventBusError;
    use crate::infrastructure::repositories::InMemoryAgentRepository;

    fn test_event_bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(32))
    }

    fn operator_requester(tenant_id: &TenantId) -> ScopeChangeRequester {
        ScopeChangeRequester {
            user_id: "operator-user".to_string(),
            roles: vec!["aegis:operator".to_string()],
            tenant_id: tenant_id.clone(),
        }
    }

    fn unprivileged_requester(tenant_id: &TenantId) -> ScopeChangeRequester {
        ScopeChangeRequester {
            user_id: "regular-user".to_string(),
            roles: vec!["user".to_string()],
            tenant_id: tenant_id.clone(),
        }
    }

    fn make_test_agent(
        name: &str,
        scope: AgentScope,
        tenant_id: &TenantId,
    ) -> crate::domain::agent::Agent {
        use crate::domain::agent::{
            Agent, AgentManifest, AgentSpec, ManifestMetadata, RuntimeConfig,
        };

        let manifest = AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: None,
                labels: std::collections::HashMap::new(),
                annotations: std::collections::HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: Some("python".to_string()),
                    version: Some("3.11".to_string()),
                    image: None,
                    image_pull_policy: crate::domain::agent::ImagePullPolicy::IfNotPresent,
                    isolation: "docker".to_string(),
                    model: "default".to_string(),
                },
                task: None,
                context: vec![],
                execution: None,
                security: None,
                schedule: None,
                tools: vec![],
                env: std::collections::HashMap::new(),
                volumes: vec![],
                advanced: None,
                input_schema: None,
                security_context: None,
            },
        };

        let mut agent = Agent::new(manifest);
        agent.scope = scope;
        agent.tenant_id = tenant_id.clone();
        agent
    }

    #[tokio::test]
    async fn promote_tenant_to_global_as_operator() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryAgentRepository::new());
        let event_bus = test_event_bus();
        let mut events = event_bus.subscribe();

        let agent = make_test_agent("test-agent", AgentScope::Tenant, &tenant_id);
        let agent_id = agent.id;
        repo.save_for_tenant(&tenant_id, &agent).await.unwrap();

        let service = AgentScopeService::new(repo, event_bus);
        let requester = operator_requester(&tenant_id);

        service
            .change_scope(agent_id, AgentScope::Global, &requester)
            .await
            .unwrap();

        match events.recv().await.unwrap() {
            crate::infrastructure::event_bus::DomainEvent::AgentLifecycle(
                AgentLifecycleEvent::AgentScopeChanged {
                    agent_id: eid,
                    new_scope,
                    new_tenant_id,
                    ..
                },
            ) => {
                assert_eq!(eid, agent_id);
                assert!(matches!(new_scope, AgentScope::Global));
                assert!(new_tenant_id.is_system());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unprivileged_user_cannot_promote_tenant_to_global() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryAgentRepository::new());
        let event_bus = test_event_bus();

        let agent = make_test_agent("test-agent", AgentScope::Tenant, &tenant_id);
        let agent_id = agent.id;
        repo.save_for_tenant(&tenant_id, &agent).await.unwrap();

        let service = AgentScopeService::new(repo, event_bus);
        let requester = unprivileged_requester(&tenant_id);

        let err = service
            .change_scope(agent_id, AgentScope::Global, &requester)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentScopeChangeError::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn not_found_returns_error() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryAgentRepository::new());
        let event_bus = test_event_bus();

        let service = AgentScopeService::new(repo, event_bus);
        let requester = operator_requester(&tenant_id);

        let err = service
            .change_scope(AgentId::new(), AgentScope::Global, &requester)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentScopeChangeError::NotFound));
    }

    #[tokio::test]
    async fn no_event_published_on_authorization_failure() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryAgentRepository::new());
        let event_bus = test_event_bus();
        let mut events = event_bus.subscribe();

        let agent = make_test_agent("test-agent", AgentScope::Tenant, &tenant_id);
        let agent_id = agent.id;
        repo.save_for_tenant(&tenant_id, &agent).await.unwrap();

        let service = AgentScopeService::new(repo, event_bus);
        let requester = unprivileged_requester(&tenant_id);

        let _ = service
            .change_scope(agent_id, AgentScope::Global, &requester)
            .await
            .unwrap_err();

        assert!(matches!(events.try_recv(), Err(EventBusError::Empty)));
    }
}
