// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Workflow Scope Service (ADR-076)
//!
//! Application service for promoting and demoting workflow visibility scope.
//! Enforces authorization rules and the two-hop prohibition.

use crate::domain::events::WorkflowEvent;
use crate::domain::repository::{RepositoryError, WorkflowRepository};
use crate::domain::tenant::TenantId;
use crate::domain::workflow::{WorkflowId, WorkflowScope};
use crate::infrastructure::event_bus::EventBus;
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScopeChangeError {
    #[error("Unauthorized: {reason}")]
    Unauthorized { reason: String },

    #[error("Name collision: workflow '{name}' v{version} already exists at target scope")]
    NameCollision {
        existing_id: WorkflowId,
        name: String,
        version: String,
    },

    #[error("Workflow not found")]
    NotFound,

    #[error("Invalid scope transition from {from} to {to}: must traverse through Tenant")]
    InvalidTransition { from: String, to: String },

    #[error("Repository error: {0}")]
    Repository(#[from] RepositoryError),
}

/// Requester identity for authorization checks.
pub struct ScopeChangeRequester {
    pub user_id: String,
    pub roles: Vec<String>,
    pub tenant_id: TenantId,
}

impl ScopeChangeRequester {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    pub fn is_operator_or_admin(&self) -> bool {
        self.has_role("aegis:operator") || self.has_role("aegis:admin")
    }

    pub fn is_tenant_admin(&self) -> bool {
        self.has_role("tenant:admin") || self.is_operator_or_admin()
    }
}

pub struct WorkflowScopeService {
    workflow_repo: Arc<dyn WorkflowRepository>,
    event_bus: Arc<EventBus>,
}

impl WorkflowScopeService {
    pub fn new(workflow_repo: Arc<dyn WorkflowRepository>, event_bus: Arc<EventBus>) -> Self {
        Self {
            workflow_repo,
            event_bus,
        }
    }

    /// Change the scope of a workflow with full authorization and collision checks.
    pub async fn change_scope(
        &self,
        workflow_id: WorkflowId,
        target_scope: WorkflowScope,
        requester: &ScopeChangeRequester,
    ) -> Result<(), ScopeChangeError> {
        // 1. Load the workflow
        let workflow = self
            .workflow_repo
            .find_by_id_for_tenant(&requester.tenant_id, workflow_id)
            .await?
            .ok_or(ScopeChangeError::NotFound)?;

        let current_scope = &workflow.scope;

        // 2. Validate transition (two-hop prohibition)
        Self::validate_transition(current_scope, &target_scope)?;

        // 3. Check authorization
        Self::check_authorization(current_scope, &target_scope, requester)?;

        // 4. Determine new tenant_id
        let new_tenant_id = match &target_scope {
            WorkflowScope::Global => TenantId::system(),
            _ => requester.tenant_id.clone(),
        };

        // 5. Check for name collision at target scope
        let version = workflow.metadata.version.clone().unwrap_or_default();
        self.check_name_collision(
            &new_tenant_id,
            &workflow.metadata.name,
            &version,
            &target_scope,
        )
        .await?;

        // 6. Perform the scope update
        self.workflow_repo
            .update_scope(workflow_id, target_scope.clone(), &new_tenant_id)
            .await?;

        // 7. Publish domain event
        self.event_bus
            .publish_workflow_event(WorkflowEvent::WorkflowScopeChanged {
                workflow_id,
                workflow_name: workflow.metadata.name.clone(),
                previous_scope: current_scope.clone(),
                new_scope: target_scope,
                previous_tenant_id: workflow.tenant_id.clone(),
                new_tenant_id,
                changed_by: requester.user_id.clone(),
                changed_at: Utc::now(),
            });

        Ok(())
    }

    fn validate_transition(
        from: &WorkflowScope,
        to: &WorkflowScope,
    ) -> Result<(), ScopeChangeError> {
        // Two-hop prohibition: User <-> Global is not allowed
        let is_user_scope = matches!(from, WorkflowScope::User { .. });
        let is_global_scope = matches!(from, WorkflowScope::Global);
        let target_is_user = matches!(to, WorkflowScope::User { .. });
        let target_is_global = matches!(to, WorkflowScope::Global);

        if (is_user_scope && target_is_global) || (is_global_scope && target_is_user) {
            return Err(ScopeChangeError::InvalidTransition {
                from: from.to_string(),
                to: to.to_string(),
            });
        }

        Ok(())
    }

    fn check_authorization(
        from: &WorkflowScope,
        to: &WorkflowScope,
        requester: &ScopeChangeRequester,
    ) -> Result<(), ScopeChangeError> {
        match (from, to) {
            // User -> Tenant: tenant-admin or operator/admin
            (WorkflowScope::User { .. }, WorkflowScope::Tenant) => {
                if !requester.is_tenant_admin() {
                    return Err(ScopeChangeError::Unauthorized {
                        reason: "User->Tenant promotion requires tenant:admin, aegis:operator, or aegis:admin role".to_string(),
                    });
                }
            }
            // Tenant -> Global: operator/admin only
            (WorkflowScope::Tenant, WorkflowScope::Global) => {
                if !requester.is_operator_or_admin() {
                    return Err(ScopeChangeError::Unauthorized {
                        reason:
                            "Tenant->Global promotion requires aegis:operator or aegis:admin role"
                                .to_string(),
                    });
                }
            }
            // Global -> Tenant: operator/admin only
            (WorkflowScope::Global, WorkflowScope::Tenant) => {
                if !requester.is_operator_or_admin() {
                    return Err(ScopeChangeError::Unauthorized {
                        reason:
                            "Global->Tenant demotion requires aegis:operator or aegis:admin role"
                                .to_string(),
                    });
                }
            }
            // Tenant -> User: tenant-admin or operator/admin
            (WorkflowScope::Tenant, WorkflowScope::User { .. }) => {
                if !requester.is_tenant_admin() {
                    return Err(ScopeChangeError::Unauthorized {
                        reason: "Tenant->User demotion requires tenant:admin, aegis:operator, or aegis:admin role".to_string(),
                    });
                }
            }
            // Same scope or no-op
            _ => {}
        }
        Ok(())
    }

    async fn check_name_collision(
        &self,
        target_tenant_id: &TenantId,
        name: &str,
        version: &str,
        target_scope: &WorkflowScope,
    ) -> Result<(), ScopeChangeError> {
        // Check if there's already a workflow with the same name+version at the target scope
        let user_id = target_scope.owner_user_id();
        let existing = self
            .workflow_repo
            .resolve_by_name_and_version(target_tenant_id, user_id, name, version)
            .await?;

        if let Some(existing_workflow) = existing {
            // Only a collision if it's at the exact target scope level
            if existing_workflow.scope.as_db_str() == target_scope.as_db_str() {
                return Err(ScopeChangeError::NameCollision {
                    existing_id: existing_workflow.id,
                    name: name.to_string(),
                    version: version.to_string(),
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
    use crate::infrastructure::repositories::InMemoryWorkflowRepository;

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

    fn tenant_admin_requester(tenant_id: &TenantId) -> ScopeChangeRequester {
        ScopeChangeRequester {
            user_id: "tenant-admin-user".to_string(),
            roles: vec!["tenant:admin".to_string()],
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

    fn make_test_workflow(
        name: &str,
        scope: WorkflowScope,
        tenant_id: &TenantId,
    ) -> crate::domain::workflow::Workflow {
        use crate::domain::workflow::{StateName, WorkflowMetadata, WorkflowSpec, WorkflowState};
        use std::collections::HashMap;

        let metadata = WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
        };

        let start_name = StateName::new("START").expect("valid state name");

        let mut states = HashMap::new();
        states.insert(
            start_name.clone(),
            WorkflowState {
                kind: crate::domain::workflow::StateKind::System {
                    command: "echo done".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );

        let spec = WorkflowSpec {
            initial_state: start_name,
            context: HashMap::new(),
            states,
            volumes: vec![],
            storage: None,
        };

        let mut workflow =
            crate::domain::workflow::Workflow::new(metadata, spec).expect("valid test workflow");
        workflow.scope = scope;
        workflow.tenant_id = tenant_id.clone();
        workflow
    }

    #[tokio::test]
    async fn promote_tenant_to_global_as_operator() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();
        let mut events = event_bus.subscribe();

        let workflow = make_test_workflow("test-wf", WorkflowScope::Tenant, &tenant_id);
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = operator_requester(&tenant_id);

        service
            .change_scope(wf_id, WorkflowScope::Global, &requester)
            .await
            .unwrap();

        match events.recv().await.unwrap() {
            crate::infrastructure::event_bus::DomainEvent::Workflow(
                WorkflowEvent::WorkflowScopeChanged {
                    workflow_id,
                    new_scope,
                    new_tenant_id,
                    ..
                },
            ) => {
                assert_eq!(workflow_id, wf_id);
                assert!(matches!(new_scope, WorkflowScope::Global));
                assert!(new_tenant_id.is_system());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_hop_user_to_global_rejected() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();

        let workflow = make_test_workflow(
            "test-wf",
            WorkflowScope::User {
                owner_user_id: "user-1".to_string(),
            },
            &tenant_id,
        );
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = operator_requester(&tenant_id);

        let err = service
            .change_scope(wf_id, WorkflowScope::Global, &requester)
            .await
            .unwrap_err();
        assert!(matches!(err, ScopeChangeError::InvalidTransition { .. }));
    }

    #[tokio::test]
    async fn two_hop_global_to_user_rejected() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();

        let workflow = make_test_workflow("test-wf", WorkflowScope::Global, &tenant_id);
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = operator_requester(&tenant_id);

        let err = service
            .change_scope(
                wf_id,
                WorkflowScope::User {
                    owner_user_id: "user-1".to_string(),
                },
                &requester,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ScopeChangeError::InvalidTransition { .. }));
    }

    #[tokio::test]
    async fn unprivileged_user_cannot_promote_tenant_to_global() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();

        let workflow = make_test_workflow("test-wf", WorkflowScope::Tenant, &tenant_id);
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = unprivileged_requester(&tenant_id);

        let err = service
            .change_scope(wf_id, WorkflowScope::Global, &requester)
            .await
            .unwrap_err();
        assert!(matches!(err, ScopeChangeError::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn tenant_admin_can_promote_user_to_tenant() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();

        let workflow = make_test_workflow(
            "test-wf",
            WorkflowScope::User {
                owner_user_id: "user-1".to_string(),
            },
            &tenant_id,
        );
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = tenant_admin_requester(&tenant_id);

        service
            .change_scope(wf_id, WorkflowScope::Tenant, &requester)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tenant_admin_cannot_promote_to_global() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();

        let workflow = make_test_workflow("test-wf", WorkflowScope::Tenant, &tenant_id);
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = tenant_admin_requester(&tenant_id);

        let err = service
            .change_scope(wf_id, WorkflowScope::Global, &requester)
            .await
            .unwrap_err();
        assert!(matches!(err, ScopeChangeError::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn not_found_returns_error() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = operator_requester(&tenant_id);

        let err = service
            .change_scope(WorkflowId::new(), WorkflowScope::Global, &requester)
            .await
            .unwrap_err();
        assert!(matches!(err, ScopeChangeError::NotFound));
    }

    #[tokio::test]
    async fn no_event_published_on_authorization_failure() {
        let tenant_id = TenantId::from_realm_slug("test-tenant").unwrap();
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let event_bus = test_event_bus();
        let mut events = event_bus.subscribe();

        let workflow = make_test_workflow("test-wf", WorkflowScope::Tenant, &tenant_id);
        let wf_id = workflow.id;
        repo.save_for_tenant(&tenant_id, &workflow).await.unwrap();

        let service = WorkflowScopeService::new(repo, event_bus);
        let requester = unprivileged_requester(&tenant_id);

        let _ = service
            .change_scope(wf_id, WorkflowScope::Global, &requester)
            .await
            .unwrap_err();

        assert!(matches!(events.try_recv(), Err(EventBusError::Empty)));
    }
}
