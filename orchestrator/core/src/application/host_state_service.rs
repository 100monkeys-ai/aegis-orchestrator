// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Host state (ADR-157 D2, D3): register, read and archive a tenant's
//! `HostState`s, and plan (check mode) or apply one over its edge target
//! through the fleet dispatcher.
//!
//! Every surface reaches host state through this service: REST, the
//! `aegis.host_state.*` tools, and workflows. They all get one validation,
//! one store and one way of running it.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::application::edge::fleet::dispatcher::FleetInvocation;
use crate::application::edge::fleet::{EdgeFleetResolver, FleetDispatcher, FleetEvent};
use crate::domain::cluster::FleetCommandId;
use crate::domain::edge_fleet::{FleetDispatchPolicy, FleetMode};
use std::collections::BTreeMap;

use crate::domain::host_state::{
    redact_secrets, HostState, HostStateManifest, HostStateRepository,
};
use crate::domain::repository::RepositoryError;
use crate::domain::secrets::{AccessContext, SecretPath};
use crate::domain::shared_kernel::TenantId;
use crate::infrastructure::aegis_cluster_proto::SealEnvelope;
use crate::infrastructure::secrets_manager::SecretsManager;

#[derive(Debug, thiserror::Error)]
pub enum HostStateError {
    #[error("the host state is not valid: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("no host state named {0}")]
    NotFound(String),
    #[error("its target matches no connected edge: {0}")]
    NoTargets(String),
    #[error("{0}")]
    Repository(#[from] RepositoryError),
    /// A secret the state names could not be read. The sentence names the
    /// secret and its path, never a value.
    #[error("{0}")]
    Unavailable(String),
}

/// Who a plan or an apply runs for, as the edge reads it (ADR-157 D12).
#[derive(Debug, Clone)]
pub enum HostStatePrincipal {
    /// A REST caller: their own bearer token travels in the inner envelope.
    Bearer(crate::domain::secrets::SensitiveString),
    /// A caller attested by the orchestrator: an MCP call (its SEAL
    /// session) or a workflow run (its run's authority). `via` names which,
    /// `reference` the session or run.
    Attested {
        principal: String,
        via: &'static str,
        reference: String,
    },
}

/// The outcome of one fleet run: each edge's result, and the edges the
/// target named that were not reached.
#[derive(Debug, Clone)]
pub struct FleetRun {
    pub fleet_command_id: FleetCommandId,
    pub hosts: Vec<HostRun>,
    pub skipped: Vec<(String, String)>,
}

/// One edge's outcome of a plan or an apply.
#[derive(Debug, Clone, Serialize)]
pub struct HostRun {
    pub node_id: String,
    pub ok: bool,
    /// `host.apply`'s report: `changed`, `failed` and per-resource results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostStateRun {
    pub name: String,
    pub version: i32,
    pub check: bool,
    pub fleet_command_id: String,
    pub hosts: Vec<HostRun>,
    pub skipped: Vec<(String, String)>,
    /// Edges that reported a change (or would, in check mode).
    pub changed_hosts: usize,
    pub failed_hosts: usize,
}

pub struct HostStateService {
    repo: Arc<dyn HostStateRepository>,
    resolver: Arc<EdgeFleetResolver>,
    dispatcher: Arc<FleetDispatcher>,
    /// Where a state's `secrets` are read, in the tenant's own store. None
    /// when the daemon runs with no secret store: a state that names a
    /// secret is then refused, never applied with the placeholder.
    secrets: Option<Arc<SecretsManager>>,
}

/// The tenant's own store for host state secrets: `host-states/<path>` in
/// the tenant's OpenBao namespace, under `users/<tenant>/` for a person's
/// tenant, whose namespace is shared by its realm (ADR-056).
pub fn host_state_secret_path(tenant: &TenantId, path: &str) -> SecretPath {
    let inner = if tenant.as_str().starts_with("u-") {
        format!("users/{}/host-states/{path}", tenant.as_str())
    } else {
        format!("host-states/{path}")
    };
    SecretPath::for_tenant(tenant.clone(), "kv", inner)
}

/// How long one edge may take to apply a state (packages, image pulls).
const APPLY_DEADLINE: Duration = Duration::from_secs(600);

impl HostStateService {
    pub fn new(
        repo: Arc<dyn HostStateRepository>,
        resolver: Arc<EdgeFleetResolver>,
        dispatcher: Arc<FleetDispatcher>,
        secrets: Option<Arc<SecretsManager>>,
    ) -> Self {
        Self {
            repo,
            resolver,
            dispatcher,
            secrets,
        }
    }

    /// Every problem with a manifest, or none.
    pub fn validate(yaml: &str) -> Vec<String> {
        match HostStateManifest::parse(yaml) {
            Ok(_) => Vec::new(),
            Err(p) => p,
        }
    }

    /// Store a manifest as the next active version of its name.
    pub async fn register(
        &self,
        tenant: &TenantId,
        yaml: &str,
        actor: &str,
    ) -> Result<HostState, HostStateError> {
        let manifest = HostStateManifest::parse(yaml).map_err(HostStateError::Invalid)?;
        Ok(self
            .repo
            .save_version(tenant, yaml, &manifest, actor)
            .await?)
    }

    /// Set `vars` on the active version of `name`, as its next version, by
    /// `actor`; nothing is stored when they are already its values. A deploy
    /// of a new image tag is this, then an apply: the registered state stays
    /// the desired one, and each deploy is a version.
    pub async fn set_vars(
        &self,
        tenant: &TenantId,
        name: &str,
        vars: BTreeMap<String, String>,
        actor: &str,
    ) -> Result<HostState, HostStateError> {
        let current = self.get(tenant, name).await?;
        let Some(yaml) = crate::domain::host_state::with_vars(&current.manifest_yaml, &vars)
            .map_err(|e| HostStateError::Invalid(vec![e]))?
        else {
            return Ok(current);
        };
        self.register(tenant, &yaml, actor).await
    }

    pub async fn get(&self, tenant: &TenantId, name: &str) -> Result<HostState, HostStateError> {
        self.repo
            .get_active(tenant, name)
            .await?
            .ok_or_else(|| HostStateError::NotFound(name.to_string()))
    }

    pub async fn list(&self, tenant: &TenantId) -> Result<Vec<HostState>, HostStateError> {
        Ok(self.repo.list_active(tenant).await?)
    }

    pub async fn archive(&self, tenant: &TenantId, name: &str) -> Result<(), HostStateError> {
        if self.repo.archive(tenant, name).await? {
            Ok(())
        } else {
            Err(HostStateError::NotFound(name.to_string()))
        }
    }

    /// Plan (`check`) or apply the active version of `name` over its target,
    /// one edge at a time, stopping at the first edge that fails.
    /// `principal` is who the run is for, carried in the inner SEAL envelope
    /// the edge checks (D12).
    pub async fn run(
        &self,
        tenant: &TenantId,
        name: &str,
        check: bool,
        principal: &HostStatePrincipal,
    ) -> Result<HostStateRun, HostStateError> {
        let state = self.get(tenant, name).await?;
        let spec = &state.manifest.spec;
        let secrets = self.read_secrets(tenant, spec).await?;
        let fleet = self
            .invoke_fleet(
                tenant,
                &spec.target,
                "host.apply",
                serde_json::json!({ "state": spec.apply_state(&secrets), "check": check }),
                spec.security_context(),
                FleetMode::Sequential,
                principal,
            )
            .await?;
        let hosts = fleet
            .hosts
            .into_iter()
            .map(|h| HostRun {
                report: h.report.map(|r| redact_secrets(&r, &secrets)),
                error: h.error.map(|e| {
                    redact_secrets(&Value::String(e), &secrets)
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                }),
                ..h
            })
            .collect();
        Ok(summarise(
            &state,
            check,
            fleet.fleet_command_id,
            hosts,
            fleet.skipped,
        ))
    }

    /// Store `fields` at `path` (`path#field` references read them) in the
    /// tenant's own host state store, replacing what was there. Values are
    /// write-only: [`Self::secret_fields`] answers names alone.
    pub async fn put_secret(
        &self,
        tenant: &TenantId,
        path: &str,
        fields: BTreeMap<String, String>,
    ) -> Result<(), HostStateError> {
        let store = self.secret_store_for(path)?;
        if fields.is_empty() {
            return Err(HostStateError::Invalid(vec![
                "a secret needs at least one field".into(),
            ]));
        }
        for name in fields.keys() {
            if crate::domain::host_state::SecretRef::parse(&format!("{path}#{name}")).is_none() {
                return Err(HostStateError::Invalid(vec![format!(
                    "field {name:?} must be letters, digits, '-' or '_'"
                )]));
            }
        }
        let at = host_state_secret_path(tenant, path);
        store
            .write_secret(
                &at.effective_mount(),
                &at.path,
                fields
                    .into_iter()
                    .map(|(k, v)| (k, crate::domain::secrets::SensitiveString::new(v)))
                    .collect(),
                &AccessContext::system("aegis-host-state-service"),
            )
            .await
            .map_err(|_| {
                HostStateError::Unavailable(format!("host-states/{path} could not be written"))
            })
    }

    /// The field names stored at `path`, never their values.
    pub async fn secret_fields(
        &self,
        tenant: &TenantId,
        path: &str,
    ) -> Result<Vec<String>, HostStateError> {
        let store = self.secret_store_for(path)?;
        let at = host_state_secret_path(tenant, path);
        match store
            .read_secret(
                &at.effective_mount(),
                &at.path,
                &AccessContext::system("aegis-host-state-service"),
            )
            .await
        {
            Ok(fields) => Ok(fields.into_keys().collect()),
            Err(crate::domain::secrets::SecretsError::SecretNotFound { .. }) => Err(
                HostStateError::NotFound(format!("secret host-states/{path}")),
            ),
            Err(_) => Err(HostStateError::Unavailable(format!(
                "host-states/{path} could not be read"
            ))),
        }
    }

    fn secret_store_for(&self, path: &str) -> Result<&Arc<SecretsManager>, HostStateError> {
        if crate::domain::host_state::SecretRef::parse(&format!("{path}#f")).is_none() {
            return Err(HostStateError::Invalid(vec![format!(
                "{path:?} must be segments of letters, digits, '-', '_' or '.', with no '..'"
            )]));
        }
        self.secrets.as_ref().ok_or_else(|| {
            HostStateError::Unavailable("this orchestrator has no secret store".into())
        })
    }

    /// The values of the secrets `spec` names, read for this run from the
    /// tenant's own store. Held only for the run: they go to the edges in
    /// the sealed command and are scrubbed from every report.
    async fn read_secrets(
        &self,
        tenant: &TenantId,
        spec: &crate::domain::host_state::HostStateSpec,
    ) -> Result<BTreeMap<String, String>, HostStateError> {
        let refs = spec.secret_refs();
        let mut values = BTreeMap::new();
        if refs.is_empty() {
            return Ok(values);
        }
        let Some(store) = &self.secrets else {
            return Err(HostStateError::Unavailable(
                "this state names secrets, and this orchestrator has no secret store".into(),
            ));
        };
        for (name, reference) in refs {
            let path = host_state_secret_path(tenant, &reference.path);
            let fields = store
                .read_secret(
                    &path.effective_mount(),
                    &path.path,
                    &AccessContext::system("aegis-host-state-service"),
                )
                .await
                .map_err(|e| {
                    HostStateError::Unavailable(format!(
                        "secret {name} (host-states/{}#{}) could not be read: {}",
                        reference.path,
                        reference.field,
                        match e {
                            crate::domain::secrets::SecretsError::SecretNotFound { .. } =>
                                "it does not exist".to_string(),
                            _ => "the secret store refused or failed".to_string(),
                        }
                    ))
                })?;
            let Some(value) = fields.get(&reference.field) else {
                return Err(HostStateError::Unavailable(format!(
                    "secret {name}: host-states/{} has no field {}",
                    reference.path, reference.field
                )));
            };
            values.insert(name, value.expose().to_string());
        }
        Ok(values)
    }

    /// Run `tool` with `args` on the edges `target` names, under `context`,
    /// one at a time (stopping at the first failure) or in parallel, for
    /// `principal`. Each edge's structured result comes back as its report.
    #[allow(clippy::too_many_arguments)]
    pub async fn invoke_fleet(
        &self,
        tenant: &TenantId,
        target: &crate::domain::edge::EdgeTarget,
        tool: &str,
        args: Value,
        context: &str,
        mode: FleetMode,
        principal: &HostStatePrincipal,
    ) -> Result<FleetRun, HostStateError> {
        let resolved = self
            .resolver
            .resolve(tenant, target)
            .await
            .map_err(|e| HostStateError::NoTargets(e.to_string()))?;
        if resolved.is_empty() {
            return Err(HostStateError::NoTargets(format!("{target:?}")));
        }
        let args = crate::application::edge::json_value_to_prost_struct(args)
            .map_err(|e| HostStateError::Invalid(vec![e.to_string()]))?;
        let inv = FleetInvocation {
            fleet_command_id: FleetCommandId::new(),
            tenant_id: tenant.clone(),
            tool_name: tool.to_string(),
            args,
            security_context_name: context.to_string(),
            user_seal_envelope: principal_envelope(principal, tenant, context.to_string()),
            resolved,
            policy: FleetDispatchPolicy {
                mode,
                per_target_deadline: APPLY_DEADLINE,
                ..FleetDispatchPolicy::default()
            },
        };
        let fleet_command_id = inv.fleet_command_id;
        let mut events = self.dispatcher.clone().spawn(inv);
        let mut hosts = Vec::new();
        let mut skipped = Vec::new();
        while let Some(ev) = events.recv().await {
            match ev {
                FleetEvent::Started { skipped: s, .. } => {
                    skipped = s.into_iter().map(|(n, why)| (n.to_string(), why)).collect();
                }
                FleetEvent::NodeCompleted {
                    node_id,
                    ok,
                    error_kind,
                    error_message,
                    result,
                    ..
                } => {
                    hosts.push(HostRun {
                        node_id: node_id.to_string(),
                        ok,
                        report: result,
                        error: if ok {
                            None
                        } else {
                            Some(
                                error_message
                                    .or(error_kind)
                                    .unwrap_or_else(|| "failed".into()),
                            )
                        },
                    });
                }
                FleetEvent::Done { .. } | FleetEvent::Cancelled { .. } => break,
                _ => {}
            }
        }
        Ok(FleetRun {
            fleet_command_id,
            hosts,
            skipped,
        })
    }
}

/// The inner envelope for `principal`: a bearer's token, or the
/// orchestrator's attestation of the principal in its payload (D12).
pub fn principal_envelope(
    principal: &HostStatePrincipal,
    tenant: &TenantId,
    context: String,
) -> SealEnvelope {
    let (token, payload) = match principal {
        HostStatePrincipal::Bearer(token) => (token.expose().to_string(), None),
        HostStatePrincipal::Attested {
            principal,
            via,
            reference,
        } => (
            String::new(),
            crate::application::edge::json_value_to_prost_struct(serde_json::json!({
                "attestation": { "via": via, "principal": principal, "reference": reference },
            }))
            .ok(),
        ),
    };
    SealEnvelope {
        user_security_token: token,
        tenant_id: tenant.as_str().to_string(),
        security_context_name: context,
        payload,
        signature: vec![],
    }
}

fn summarise(
    state: &HostState,
    check: bool,
    id: FleetCommandId,
    hosts: Vec<HostRun>,
    skipped: Vec<(String, String)>,
) -> HostStateRun {
    let changed_hosts = hosts
        .iter()
        .filter(|h| {
            h.report
                .as_ref()
                .and_then(|r| r.get("changed"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
        })
        .count();
    let failed_hosts = hosts.iter().filter(|h| !h.ok).count();
    HostStateRun {
        name: state.name.clone(),
        version: state.version,
        check,
        fleet_command_id: id.0.to_string(),
        hosts,
        skipped,
        changed_hosts,
        failed_hosts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_travels_as_the_token_and_an_attestation_in_the_payload() {
        let tenant = TenantId::from_string("t-465fa94f-36a6-464b-b82d-88f2bc07b03e").unwrap();
        let bearer = principal_envelope(
            &HostStatePrincipal::Bearer(crate::domain::secrets::SensitiveString::new(
                "a.b.c".to_string(),
            )),
            &tenant,
            "host-admin".into(),
        );
        assert_eq!(bearer.user_security_token, "a.b.c");
        assert!(bearer.payload.is_none());
        assert_eq!(bearer.security_context_name, "host-admin");

        let attested = principal_envelope(
            &HostStatePrincipal::Attested {
                principal: "user-1".into(),
                via: "seal_session",
                reference: "exec-1".into(),
            },
            &tenant,
            "host-admin".into(),
        );
        assert!(attested.user_security_token.is_empty());
        let json =
            crate::application::edge::prost_struct_to_json(attested.payload.as_ref().unwrap());
        assert_eq!(json["attestation"]["principal"], "user-1");
        assert_eq!(json["attestation"]["via"], "seal_session");
    }
}
