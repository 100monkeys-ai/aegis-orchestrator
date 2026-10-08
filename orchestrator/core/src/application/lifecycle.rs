// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Agent Lifecycle Application Service — BC-1
//!
//! Use-case implementations for the **Agent Lifecycle** bounded context:
//! deploy, update, pause, resume, and delete agent definitions.
//!
//! Orchestrates:
//! 1. Manifest validation via the `ManifestValidator` domain service
//! 2. Persistence via [`crate::domain::repository::AgentRepository`]
//! 3. Event publication (`AgentLifecycleEvent`) via the `EventBus`
//!
//! Implements the `AgentLifecycleService` trait from
//! [`crate::application::agent`].
//!
//! See AGENTS.md §BC-1 Agent Lifecycle Context.
//!
//! # Code Quality Principles
//!
//! - Keep manifest validation and lifecycle persistence in one application boundary.
//! - Fail closed on invalid manifests rather than synthesizing defaults.
//! - Publish lifecycle state changes explicitly instead of relying on implicit side effects.

use crate::application::agent::AgentLifecycleService;
use crate::application::tenant_quota::TenantQuotaService;
use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use crate::domain::events::AgentLifecycleEvent;
use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::repository::AgentRepository;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;

pub struct StandardAgentLifecycleService {
    repository: Arc<dyn AgentRepository>,
    event_bus: Arc<EventBus>,
    security_context_repo: Arc<dyn SecurityContextRepository>,
    /// Optional quota enforcement service (wired at composition root, ADR-056).
    quota_service: Option<Arc<TenantQuotaService>>,
}

impl StandardAgentLifecycleService {
    pub fn new(
        repository: Arc<dyn AgentRepository>,
        event_bus: Arc<EventBus>,
        security_context_repo: Arc<dyn SecurityContextRepository>,
    ) -> Self {
        Self {
            repository,
            event_bus,
            security_context_repo,
            quota_service: None,
        }
    }

    /// Wire the quota enforcement service.
    pub fn with_quota_service(mut self, quota_service: Arc<TenantQuotaService>) -> Self {
        self.quota_service = Some(quota_service);
        self
    }

    /// Cross-tenant agent list (ADR-097). Operator-only — callers MUST gate
    /// on [`crate::domain::iam::IdentityKind::Operator`]. Each returned
    /// [`Agent`] carries its own `tenant_id`; surface it in projections.
    pub async fn list_all_agents(&self) -> Result<Vec<Agent>> {
        self.repository
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {e}"))
    }

    /// Cross-tenant agent fetch by ID. Operator-only — callers MUST gate on
    /// the operator identity kind before calling. Returns `Ok(None)` if no
    /// agent exists with the given id in any tenant.
    pub async fn find_by_id_unscoped(&self, id: AgentId) -> Result<Option<Agent>> {
        // Iterate `list_all` and filter; the cardinality is small enough for
        // the operator detail-fetch path. A repo-level `find_by_id_unscoped`
        // can replace this in a follow-up if perf demands it.
        let agents = self.list_all_agents().await?;
        Ok(agents.into_iter().find(|a| a.id == id))
    }
}

#[async_trait]
impl AgentLifecycleService for StandardAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        scope: AgentScope,
        caller_identity: Option<&UserIdentity>,
    ) -> Result<AgentId> {
        // Validate manifest before deploying
        manifest.validate().map_err(|e| anyhow::anyhow!(e))?;
        // AEGIS ADR-005 O4 and O5: refuse an agent whose declarations cannot do its work.
        crate::domain::tool_requirement::check(&manifest)?;

        // Quota check: ensure the tenant has not exceeded its max_agents limit (ADR-056).
        if let Some(quota_svc) = &self.quota_service {
            quota_svc
                .check_agent_quota(tenant_id)
                .await
                .map_err(|e| anyhow::anyhow!("quota_exceeded:{}", e))?;
        }

        // ADR-102: Only Operators and ServiceAccounts may register agents with aegis-system-* contexts.
        if let Some(ctx) = &manifest.spec.security_context {
            if ctx.starts_with("aegis-system-") {
                let permitted = caller_identity.is_none()
                    || matches!(
                        caller_identity.map(|id| &id.identity_kind),
                        Some(IdentityKind::Operator { .. })
                            | Some(IdentityKind::ServiceAccount { .. })
                    );
                if !permitted {
                    anyhow::bail!(
                        "Forbidden: only platform operators may register agents \
                         with an aegis-system-* security context"
                    );
                }
            }
            // Validate the named security context exists.
            if self
                .security_context_repo
                .find_by_name(ctx)
                .await?
                .is_none()
            {
                anyhow::bail!("Unknown security context: '{ctx}'");
            }
        }

        // Check if an agent with the same name already exists
        if let Some(existing) = self
            .repository
            .find_by_name_for_tenant(tenant_id, &manifest.metadata.name)
            .await?
        {
            let existing_version = &existing.manifest.metadata.version;
            let incoming_version = &manifest.metadata.version;

            if existing_version == incoming_version {
                // Same name AND same version — only allowed when --force is set
                if !force {
                    anyhow::bail!(
                        "Agent '{}' version '{}' is already deployed (ID: {}). \
                         Use --force to overwrite it.",
                        existing.name,
                        existing_version,
                        existing.id.0
                    );
                }
                // --force: overwrite the existing agent's manifest in place,
                // preserving its AgentId so existing execution references remain valid.
                // Preserve the existing scope on force-overwrite.
                let mut updated = existing.clone();
                let new_version = updated.manifest.metadata.version.clone();
                let old_version = existing_version.clone();
                updated.update_manifest(manifest);
                self.repository.save_for_tenant(tenant_id, &updated).await?;
                self.event_bus
                    .publish_agent_event(AgentLifecycleEvent::AgentUpdated {
                        agent_id: updated.id,
                        tenant_id: tenant_id.clone(),
                        old_version,
                        new_version,
                        updated_at: Utc::now(),
                    });
                return Ok(updated.id);
            }

            // Different version — treat as an in-place update (new version replaces old).
            // Preserve existing scope when updating version.
            let old_version = existing_version.clone();
            let new_version = manifest.metadata.version.clone();
            let mut updated = existing.clone();
            updated.update_manifest(manifest);
            self.repository.save_for_tenant(tenant_id, &updated).await?;
            self.event_bus
                .publish_agent_event(AgentLifecycleEvent::AgentUpdated {
                    agent_id: updated.id,
                    tenant_id: tenant_id.clone(),
                    old_version,
                    new_version,
                    updated_at: Utc::now(),
                });
            return Ok(updated.id);
        }

        // No existing agent with this name — create a fresh one with the requested scope
        let mut agent = Agent::new(manifest);
        agent.scope = scope;
        agent.tenant_id = tenant_id.clone();
        self.repository.save_for_tenant(tenant_id, &agent).await?;

        self.event_bus
            .publish_agent_event(AgentLifecycleEvent::AgentDeployed {
                agent_id: agent.id,
                tenant_id: tenant_id.clone(),
                manifest: agent.manifest.clone(),
                deployed_at: Utc::now(),
            });

        metrics::counter!(
            "aegis_agent_lifecycle_operations_total",
            "operation" => "deploy",
            "result" => "success"
        )
        .increment(1);

        Ok(agent.id)
    }

    async fn get_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.repository
            .find_by_id_for_tenant(tenant_id, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn get_agent_visible(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.repository
            .find_by_id_visible(tenant_id, id)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()> {
        // AEGIS ADR-005 O4 and O5: refuse an agent whose declarations cannot do its work.
        crate::domain::tool_requirement::check(&manifest)?;
        let mut agent = self.get_agent_for_tenant(tenant_id, id).await?;
        let old_version = agent.manifest.metadata.version.clone();
        let new_version = manifest.metadata.version.clone();
        agent.update_manifest(manifest);
        self.repository.save_for_tenant(tenant_id, &agent).await?;
        self.event_bus
            .publish_agent_event(AgentLifecycleEvent::AgentUpdated {
                agent_id: id,
                tenant_id: tenant_id.clone(),
                old_version,
                new_version,
                updated_at: Utc::now(),
            });
        Ok(())
    }

    async fn delete_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<()> {
        self.repository.delete_for_tenant(tenant_id, id).await?;

        self.event_bus
            .publish_agent_event(AgentLifecycleEvent::AgentRemoved {
                agent_id: id,
                tenant_id: tenant_id.clone(),
                removed_at: Utc::now(),
            });

        metrics::counter!(
            "aegis_agent_lifecycle_operations_total",
            "operation" => "delete",
            "result" => "success"
        )
        .increment(1);

        Ok(())
    }

    async fn list_agents_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.repository
            .list_all_for_tenant(tenant_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {e}"))
    }

    async fn list_agents_visible_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.repository
            .list_visible_for_tenant(tenant_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list visible agents: {e}"))
    }

    async fn lookup_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .find_by_name_for_tenant(tenant_id, name)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .resolve_by_name(tenant_id, name)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .find_by_name_and_version_for_tenant(tenant_id, name, version)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn list_versions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<crate::domain::repository::AgentVersion>> {
        self.repository
            .list_versions_for_tenant(tenant_id, agent_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agent versions: {e}"))
    }
}

#[cfg(test)]
mod tests {
    //! Phase 3 regression: AgentDeployed/AgentUpdated/AgentRemoved events
    //! must carry the actual owning `tenant_id` so downstream subscribers
    //! (Cortex discovery indexer in particular) cannot collapse every
    //! agent onto the consumer singleton.

    use super::*;
    use crate::domain::agent::{
        AgentManifest, AgentSpec, ImagePullPolicy, ManifestMetadata, RuntimeConfig, TaskConfig,
    };
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::InMemoryAgentRepository;
    use crate::infrastructure::security_context::InMemorySecurityContextRepository;
    use std::collections::HashMap;

    fn manifest(name: &str, version: &str) -> AgentManifest {
        AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: version.to_string(),
                description: None,
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: Some("python".to_string()),
                    version: Some("3.11".to_string()),
                    image: None,
                    image_pull_policy: ImagePullPolicy::IfNotPresent,
                    isolation: "inherit".to_string(),
                    model: "default".to_string(),
                    temperature: None,
                },
                task: Some(TaskConfig {
                    instruction: Some("noop".to_string()),
                    prompt_template: None,
                    input_data: None,
                }),
                context: vec![],
                contexts: Vec::new(),
                execution: None,
                security: None,
                default_schedule: None,
                tools: vec![],
                env: HashMap::new(),
                volumes: vec![],
                advanced: None,
                input_schema: None,
                security_context: None,
                output_handler: None,
                program: None,
            },
        }
    }

    fn make_service() -> (
        StandardAgentLifecycleService,
        Arc<crate::infrastructure::event_bus::EventBus>,
    ) {
        let repo = Arc::new(InMemoryAgentRepository::new());
        let bus = Arc::new(crate::infrastructure::event_bus::EventBus::new(64));
        let sec_repo = Arc::new(InMemorySecurityContextRepository::new());
        let svc = StandardAgentLifecycleService::new(repo, bus.clone(), sec_repo);
        (svc, bus)
    }

    #[tokio::test]
    async fn agent_deployed_event_carries_owning_tenant_id() {
        let (svc, bus) = make_service();
        let mut rx = bus.subscribe();

        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        let _id = svc
            .deploy_agent_for_tenant(
                &alice,
                manifest("alpha", "1.0.0"),
                false,
                AgentScope::Tenant,
                None,
            )
            .await
            .expect("deploy ok");

        let received = rx.recv().await.expect("event published");
        match received {
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed {
                tenant_id, ..
            }) => {
                assert_eq!(
                    tenant_id, alice,
                    "AgentDeployed must carry the owning tenant, not consumer()"
                );
                assert_ne!(tenant_id, TenantId::consumer());
            }
            other => panic!("expected AgentDeployed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_updated_event_carries_owning_tenant_id() {
        let (svc, bus) = make_service();
        let mut rx = bus.subscribe();

        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        // Deploy v1
        let id = svc
            .deploy_agent_for_tenant(
                &alice,
                manifest("alpha", "1.0.0"),
                false,
                AgentScope::Tenant,
                None,
            )
            .await
            .expect("deploy v1 ok");
        // drain the deployed event
        let _ = rx.recv().await.expect("deploy event");

        // Update via versioned re-deploy → publishes AgentUpdated.
        let _ = svc
            .deploy_agent_for_tenant(
                &alice,
                manifest("alpha", "1.1.0"),
                false,
                AgentScope::Tenant,
                None,
            )
            .await
            .expect("deploy v2 ok");

        let received = rx.recv().await.expect("update event");
        match received {
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentUpdated {
                agent_id,
                tenant_id,
                old_version,
                new_version,
                ..
            }) => {
                assert_eq!(agent_id, id);
                assert_eq!(tenant_id, alice);
                assert_eq!(old_version, "1.0.0");
                assert_eq!(new_version, "1.1.0");
            }
            other => panic!("expected AgentUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_removed_event_carries_owning_tenant_id() {
        let (svc, bus) = make_service();
        let mut rx = bus.subscribe();

        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        let id = svc
            .deploy_agent_for_tenant(
                &alice,
                manifest("alpha", "1.0.0"),
                false,
                AgentScope::Tenant,
                None,
            )
            .await
            .expect("deploy ok");
        // drain deploy event
        let _ = rx.recv().await.expect("deploy event");

        svc.delete_agent_for_tenant(&alice, id)
            .await
            .expect("delete ok");

        let received = rx.recv().await.expect("remove event");
        match received {
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentRemoved {
                agent_id,
                tenant_id,
                ..
            }) => {
                assert_eq!(agent_id, id);
                assert_eq!(tenant_id, alice);
            }
            other => panic!("expected AgentRemoved, got {other:?}"),
        }
    }

    /// AEGIS ADR-005 O4: the shape of `delivery-itinerary-pdf-agent`
    /// (3b367333), a `/workspace` volume, with the tools given.
    fn delivery_itinerary(version: &str, tools: &str) -> AgentManifest {
        crate::infrastructure::agent_manifest_parser::AgentManifestParser::parse_yaml(&format!(
            r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: delivery-itinerary-pdf-agent
  version: "{version}"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Write the delivery itinerary PDF to /workspace/delivery_itinerary.pdf.
{tools}
  volumes:
    - name: workspace
      storage_class: persistent
      type: seaweedfs
      mount_path: /workspace
      access_mode: read-write
      size_limit: 1Gi
"#
        ))
        .expect("the delivery itinerary manifest parses")
    }

    const O4_SENTENCE: &str = "Agent 'delivery-itinerary-pdf-agent' is refused \
        (tool-requirement/no-tools): its instruction requires writing files or running commands \
        and it declares no \
        tools; it declares the read-write volume 'workspace' at /workspace; its instruction \
        names the workspace file /workspace/delivery_itinerary.pdf. Declare the tools it needs \
        in spec.tools (fs.write to write files, cmd.run to run commands).";

    #[tokio::test]
    async fn deploy_refuses_a_tool_less_work_agent_with_the_o4_sentence() {
        let (svc, _bus) = make_service();
        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        let err = svc
            .deploy_agent_for_tenant(
                &alice,
                delivery_itinerary("1.0.0", ""),
                false,
                AgentScope::Tenant,
                None,
            )
            .await
            .expect_err("deploy must refuse a tool-less agent whose instruction writes a file");
        assert_eq!(err.to_string(), O4_SENTENCE);
        assert_eq!(
            svc.lookup_agent_for_tenant(&alice, "delivery-itinerary-pdf-agent")
                .await
                .expect("lookup ok"),
            None,
            "a refused agent is not saved"
        );
    }

    #[tokio::test]
    async fn deploy_accepts_the_same_agent_once_it_declares_fs_write() {
        let (svc, _bus) = make_service();
        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        svc.deploy_agent_for_tenant(
            &alice,
            delivery_itinerary("1.0.0", "  tools:\n    - fs.write"),
            false,
            AgentScope::Tenant,
            None,
        )
        .await
        .expect("deploy accepts the agent once it declares fs.write");
    }

    #[tokio::test]
    async fn update_refuses_a_tool_less_work_agent_with_the_o4_sentence() {
        let (svc, _bus) = make_service();
        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        let id = svc
            .deploy_agent_for_tenant(
                &alice,
                delivery_itinerary("1.0.0", "  tools:\n    - fs.write"),
                false,
                AgentScope::Tenant,
                None,
            )
            .await
            .expect("deploy v1 ok");

        let err = svc
            .update_agent_for_tenant(&alice, id, delivery_itinerary("1.1.0", ""))
            .await
            .expect_err("update must refuse a tool-less agent whose instruction writes a file");
        assert_eq!(err.to_string(), O4_SENTENCE);
        let kept = svc.get_agent_for_tenant(&alice, id).await.expect("get ok");
        assert_eq!(
            kept.manifest.spec.tools,
            vec!["fs.write".to_string()],
            "a refused update leaves the deployed manifest in place"
        );
    }
}
