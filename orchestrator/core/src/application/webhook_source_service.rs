// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Webhook sources (ADR-157 D4): register a tenant's signed sender, keep its
//! secret in the tenant's own secret store, and verify deliveries with it.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::iam::UserIdentity;
use crate::domain::repository::RepositoryError;
use crate::domain::schedule::ScheduleOwner;
use crate::domain::secrets::{AccessContext, SecretPath, SensitiveString};
use crate::domain::shared_kernel::TenantId;
use crate::domain::webhook_source::{WebhookSource, WebhookSourceRepository, WebhookSourceSpec};
use crate::infrastructure::secrets_manager::SecretsManager;
use crate::presentation::webhook_guard::WebhookSecretProvider;

const SECRET_FIELD: &str = "secret";

#[derive(Debug, thiserror::Error)]
pub enum WebhookSourceError {
    #[error("the webhook source is not valid: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("no webhook source named {0}")]
    NotFound(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Repository(#[from] RepositoryError),
}

pub struct WebhookSourceService {
    repo: Arc<dyn WebhookSourceRepository>,
    secrets: Arc<SecretsManager>,
}

/// Where a source's secret lives: `webhooks/<name>` in the tenant's own
/// store (a team's under its `teams/<tenant>/`, a person's under
/// `users/<tenant>/`).
fn secret_path(tenant: &TenantId, name: &str) -> SecretPath {
    let inner = if tenant.as_str().starts_with("u-") {
        format!("users/{}/webhooks/{name}", tenant.as_str())
    } else {
        format!("webhooks/{name}")
    };
    SecretPath::for_tenant(tenant.clone(), "kv", inner)
}

fn new_secret() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

impl WebhookSourceService {
    pub fn new(repo: Arc<dyn WebhookSourceRepository>, secrets: Arc<SecretsManager>) -> Self {
        Self { repo, secrets }
    }

    /// Register or update `name` for `tenant`, owned by `owner`. A new source
    /// gets a generated secret, answered here once and never again; an update
    /// keeps its secret.
    pub async fn register(
        &self,
        tenant: &TenantId,
        name: &str,
        spec: WebhookSourceSpec,
        owner: &UserIdentity,
    ) -> Result<(WebhookSource, Option<String>), WebhookSourceError> {
        let problems = spec.problems(name);
        if !problems.is_empty() {
            return Err(WebhookSourceError::Invalid(problems));
        }
        let projection = ScheduleOwner::from_identity(owner)
            .map_err(|e| WebhookSourceError::Invalid(vec![e]))?;
        let existed = self.repo.get(tenant, name).await?.is_some();
        let secret = if existed {
            None
        } else {
            Some(self.store_secret(tenant, name).await?)
        };
        let saved = self
            .repo
            .save(tenant, name, &spec, &projection, &owner.sub)
            .await?;
        Ok((saved, secret))
    }

    /// A new secret for `name`, answered once; the old one stops verifying.
    pub async fn rotate_secret(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<String, WebhookSourceError> {
        self.get(tenant, name).await?;
        self.store_secret(tenant, name).await
    }

    async fn store_secret(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<String, WebhookSourceError> {
        let secret = new_secret();
        let at = secret_path(tenant, name);
        self.secrets
            .write_secret(
                &at.effective_mount(),
                &at.path,
                HashMap::from([(
                    SECRET_FIELD.to_string(),
                    SensitiveString::new(secret.clone()),
                )]),
                &AccessContext::system("aegis-webhook-sources"),
            )
            .await
            .map_err(|_| {
                WebhookSourceError::Unavailable(format!("the secret of {name} could not be stored"))
            })?;
        Ok(secret)
    }

    pub async fn get(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<WebhookSource, WebhookSourceError> {
        self.repo
            .get(tenant, name)
            .await?
            .ok_or_else(|| WebhookSourceError::NotFound(name.to_string()))
    }

    pub async fn list(&self, tenant: &TenantId) -> Result<Vec<WebhookSource>, WebhookSourceError> {
        Ok(self.repo.list(tenant).await?)
    }

    pub async fn delete(&self, tenant: &TenantId, name: &str) -> Result<(), WebhookSourceError> {
        if !self.repo.delete(tenant, name).await? {
            return Err(WebhookSourceError::NotFound(name.to_string()));
        }
        let at = secret_path(tenant, name);
        let _ = self
            .secrets
            .delete_secret(
                &at.effective_mount(),
                &at.path,
                &AccessContext::system("aegis-webhook-sources"),
            )
            .await;
        Ok(())
    }

    /// Every tenant that registered `name`.
    pub async fn tenants_for(&self, name: &str) -> Vec<TenantId> {
        self.repo.tenants_for(name).await.unwrap_or_default()
    }
}

#[async_trait]
impl WebhookSecretProvider for WebhookSourceService {
    async fn get_secret(&self, tenant_id: &TenantId, source_name: &str) -> Option<Vec<u8>> {
        let at = secret_path(tenant_id, source_name);
        let fields = self
            .secrets
            .read_secret(
                &at.effective_mount(),
                &at.path,
                &AccessContext::system("aegis-webhook-sources"),
            )
            .await
            .ok()?;
        fields
            .get(SECRET_FIELD)
            .map(|s| s.expose().as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sources_secret_is_in_its_tenants_own_store() {
        let team = TenantId::new("t-465fa94f-36a6-464b-b82d-88f2bc07b03e").unwrap();
        let at = secret_path(&team, "aegis-temporal-worker");
        assert_eq!(at.effective_mount(), "tenant-zaru-consumer/kv");
        assert_eq!(
            at.path,
            "teams/t-465fa94f-36a6-464b-b82d-88f2bc07b03e/webhooks/aegis-temporal-worker"
        );
        assert_eq!(new_secret().len(), 64);
        assert_ne!(new_secret(), new_secret());
    }
}
