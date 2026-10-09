// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Profile service (AEGIS ADR-140 D1 to D5)
//!
//! Creates, reads, changes and deletes a person's profiles, and answers the
//! tools a set of the person's bindings may be narrowed to.
//!
//! **What is checked at create and update.** The name (D1's bounds, unique
//! for the owner ignoring case); each binding newly listed is the owner's
//! own and active (a binding the stored profile already holds may since
//! have been removed, D4, and stays); each tool pattern parses and admits
//! something the owner's security context admits for the families of the
//! profile's active bindings, else D2's sentence; the repository default is
//! one of the owner's repositories in the tenant; the instructions and the
//! notes workspace are within bounds.
//!
//! **A binding's families** (the grant's reading of `credential_service.rs`
//! [`is_mailbox_binding`] and [`is_calendar_binding`]): a mailbox governs
//! `mail.*`, a calendar account `calendar.*` (one OAuth binding with both
//! settings governs both), and every binding governs `<provider>.*`, which
//! is a remote server's family when the provider names one.
//!
//! **Available tools** (the grant's reading R1): for a family the builtin
//! catalogue has tools of (`mail`, `calendar`), each of those tools the
//! security context admits, `gated` from the router's approval marks; for
//! any other family (a remote server's), each of the node configuration's
//! `tool_capabilities` names in it that the security context admits, gated
//! the same way, and one `<family>.*` entry, `gated: false`, when the
//! security context admits some tool of it.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::application::credential_service::{
    is_calendar_binding, is_mailbox_binding, CredentialManagementService,
};
use crate::domain::credential::{CredentialBindingId, CredentialStatus, UserCredentialBinding};
use crate::domain::git_repo::{GitRepoBindingId, GitRepoBindingRepository};
use crate::domain::iam::UserIdentity;
use crate::domain::node_config::ToolCapabilityConfig;
use crate::domain::profile::{
    binding_refusal, check_instructions, check_name, check_notes_workspace, narrow_refusal,
    Profile, ProfileId, ProfileRepository, ProfileSave, ToolAllowList, ToolPattern,
    BINDINGS_DUPLICATE_REFUSAL, BINDINGS_SHAPE_REFUSAL, NAME_TAKEN_REFUSAL, REPOSITORY_REFUSAL,
    TOOLS_SHAPE_REFUSAL,
};
use crate::domain::repository::RepositoryError;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::SecurityContext;
use crate::domain::tenant::TenantId;
use crate::infrastructure::tool_router::ToolRouter;

/// The family of the mail tools.
pub const MAIL_FAMILY: &str = "mail";
/// The family of the calendar tools.
pub const CALENDAR_FAMILY: &str = "calendar";

/// Why a profile call was not done.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProfileError {
    /// Refused before anything was stored: the sentence.
    #[error("{0}")]
    Refused(String),
    /// No such profile, or not the caller's.
    #[error("Not found")]
    NotFound,
    #[error("Profile store failed: {0}")]
    Repository(String),
}

impl From<RepositoryError> for ProfileError {
    fn from(e: RepositoryError) -> Self {
        Self::Repository(e.to_string())
    }
}

/// The owner's credential bindings, active or not.
#[async_trait]
pub trait ProfileBindingSource: Send + Sync {
    async fn owner_bindings(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>>;
}

#[async_trait]
impl ProfileBindingSource for Arc<dyn CredentialManagementService> {
    async fn owner_bindings(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        self.list_bindings(tenant_id, user_sub).await
    }
}

/// What the profile pages are offered: the builtin tools, the node
/// configuration's named tool capabilities, and which tools are gated.
pub trait ProfileToolCatalogue: Send + Sync {
    /// Every builtin tool, by name and description.
    fn builtin_tools(&self) -> Vec<(String, String)>;
    /// The exact tool names the node configuration's `tool_capabilities`
    /// speaks for (its wildcard entries are left out).
    fn capability_names(&self) -> Vec<String>;
    /// Whether a call of the tool waits at the approval gate.
    fn requires_approval(&self, tool_name: &str) -> bool;
}

/// The catalogue of a running node: the tool router and its configuration.
pub struct RouterToolCatalogue {
    router: Arc<ToolRouter>,
    capability_names: Vec<String>,
}

impl RouterToolCatalogue {
    pub fn new(router: Arc<ToolRouter>, tool_capabilities: &[ToolCapabilityConfig]) -> Self {
        Self {
            router,
            capability_names: tool_capabilities
                .iter()
                .map(|entry| entry.tool_pattern.clone())
                .filter(|pattern| !pattern.contains('*'))
                .collect(),
        }
    }
}

impl ProfileToolCatalogue for RouterToolCatalogue {
    fn builtin_tools(&self) -> Vec<(String, String)> {
        ToolRouter::builtin_dispatchers()
            .into_iter()
            .map(|d| (d.name, d.description))
            .collect()
    }

    fn capability_names(&self) -> Vec<String> {
        self.capability_names.clone()
    }

    fn requires_approval(&self, tool_name: &str) -> bool {
        self.router.requires_approval(tool_name)
    }
}

/// Sets a field to the value given, absent and `null` told apart: an
/// absent field stays `None`, a given one (`null` included) is `Some`.
fn given<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// What a person sends to make a profile (D3).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileDraft {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bindings: Value,
    #[serde(default)]
    pub tools: Value,
    #[serde(default)]
    pub repository: Option<Value>,
    #[serde(default)]
    pub notes_workspace: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

/// What a person sends to change a profile (D3): any field; `bindings` and
/// `tools` are replaced whole; `null` clears a default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfilePatch {
    #[serde(default, deserialize_with = "given")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "given")]
    pub bindings: Option<Value>,
    #[serde(default, deserialize_with = "given")]
    pub tools: Option<Value>,
    #[serde(default, deserialize_with = "given")]
    pub repository: Option<Value>,
    #[serde(default, deserialize_with = "given")]
    pub notes_workspace: Option<Value>,
    #[serde(default, deserialize_with = "given")]
    pub instructions: Option<Value>,
}

/// One of a profile's bindings as a read answers it (D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileBindingView {
    pub binding_id: CredentialBindingId,
    /// `active`; `removed` for a binding revoked or deleted since; or the
    /// binding's own status (`expired`, `pending_oauth`, `pending_migration`)
    /// otherwise.
    pub state: &'static str,
    /// The binding's label, when it still exists.
    pub label: Option<String>,
}

/// A profile with its bindings' states.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileView {
    pub profile: Profile,
    pub bindings: Vec<ProfileBindingView>,
}

/// One tool a profile may be narrowed to (D3's `available-tools`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableTool {
    pub name: String,
    pub description: String,
    pub family: String,
    pub gated: bool,
}

/// The families a binding's tools belong to (the grant's reading).
pub fn binding_families(binding: &UserCredentialBinding) -> Vec<String> {
    let mut families = Vec::new();
    if is_mailbox_binding(binding) {
        families.push(MAIL_FAMILY.to_string());
    }
    if is_calendar_binding(binding) {
        families.push(CALENDAR_FAMILY.to_string());
    }
    let provider = binding.provider.as_str().to_string();
    if !families.contains(&provider) {
        families.push(provider);
    }
    families
}

fn binding_state(binding: Option<&UserCredentialBinding>) -> &'static str {
    match binding.map(|b| &b.status) {
        Some(CredentialStatus::Active) => "active",
        None | Some(CredentialStatus::Revoked) => "removed",
        Some(CredentialStatus::Expired) => "expired",
        Some(CredentialStatus::PendingOAuth) => "pending_oauth",
        Some(CredentialStatus::PendingMigration) => "pending_migration",
    }
}

/// A person's profiles.
pub struct ProfileService {
    repo: Arc<dyn ProfileRepository>,
    bindings: Arc<dyn ProfileBindingSource>,
    security_contexts: Arc<dyn SecurityContextRepository>,
    catalogue: Arc<dyn ProfileToolCatalogue>,
    repositories: Option<Arc<dyn GitRepoBindingRepository>>,
}

impl ProfileService {
    pub fn new(
        repo: Arc<dyn ProfileRepository>,
        bindings: Arc<dyn ProfileBindingSource>,
        security_contexts: Arc<dyn SecurityContextRepository>,
        catalogue: Arc<dyn ProfileToolCatalogue>,
        repositories: Option<Arc<dyn GitRepoBindingRepository>>,
    ) -> Self {
        Self {
            repo,
            bindings,
            security_contexts,
            catalogue,
            repositories,
        }
    }

    async fn owner_bindings(
        &self,
        tenant: &TenantId,
        sub: &str,
    ) -> Result<Vec<UserCredentialBinding>, ProfileError> {
        let bindings = self
            .bindings
            .owner_bindings(tenant, sub)
            .await
            .map_err(|e| ProfileError::Repository(format!("binding lookup failed: {e}")))?;
        Ok(bindings
            .into_iter()
            .filter(|b| b.owner_user_id == sub && &b.tenant_id == tenant)
            .collect())
    }

    /// The owner's security context; one the node does not know admits
    /// nothing.
    async fn security_context(
        &self,
        owner: &UserIdentity,
    ) -> Result<Option<SecurityContext>, ProfileError> {
        let name = owner.to_security_context_name();
        let found = self
            .security_contexts
            .find_by_name(&name)
            .await
            .map_err(|e| {
                ProfileError::Repository(format!("security context lookup failed: {e}"))
            })?;
        if found.is_none() {
            tracing::warn!(security_context = %name, "a profile owner's security context is unknown to this node");
        }
        Ok(found)
    }

    fn view(profile: Profile, owned: &[UserCredentialBinding]) -> ProfileView {
        let bindings = profile
            .bindings
            .iter()
            .map(|id| {
                let binding = owned.iter().find(|b| &b.id == id);
                ProfileBindingView {
                    binding_id: *id,
                    state: binding_state(binding),
                    label: binding.map(|b| b.metadata.label.clone()),
                }
            })
            .collect();
        ProfileView { profile, bindings }
    }

    /// Parse a `bindings` value into ids, each listed once.
    fn parse_bindings(raw: &Value) -> Result<Vec<CredentialBindingId>, ProfileError> {
        let refuse = || ProfileError::Refused(BINDINGS_SHAPE_REFUSAL.to_string());
        let items = match raw {
            Value::Null => return Ok(Vec::new()),
            Value::Array(items) => items,
            _ => return Err(refuse()),
        };
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            let raw = item.as_str().ok_or_else(refuse)?;
            let id = Uuid::parse_str(raw)
                .map(CredentialBindingId)
                .map_err(|_| ProfileError::Refused(binding_refusal(raw)))?;
            if ids.contains(&id) {
                return Err(ProfileError::Refused(
                    BINDINGS_DUPLICATE_REFUSAL.to_string(),
                ));
            }
            ids.push(id);
        }
        Ok(ids)
    }

    fn parse_tools(raw: &Value) -> Result<ToolAllowList, ProfileError> {
        let items = match raw {
            Value::Null => return Ok(ToolAllowList::default()),
            Value::Array(items) => items,
            _ => return Err(ProfileError::Refused(TOOLS_SHAPE_REFUSAL.to_string())),
        };
        let mut patterns: Vec<ToolPattern> = Vec::with_capacity(items.len());
        for item in items {
            let raw = item
                .as_str()
                .ok_or_else(|| ProfileError::Refused(TOOLS_SHAPE_REFUSAL.to_string()))?;
            let pattern = ToolPattern::parse(raw.trim()).map_err(ProfileError::Refused)?;
            if !patterns.contains(&pattern) {
                patterns.push(pattern);
            }
        }
        Ok(ToolAllowList(patterns))
    }

    fn parse_repository(raw: Option<&Value>) -> Result<Option<Uuid>, ProfileError> {
        match raw {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) => Uuid::parse_str(id)
                .map(Some)
                .map_err(|_| ProfileError::Refused(REPOSITORY_REFUSAL.to_string())),
            Some(_) => Err(ProfileError::Refused(REPOSITORY_REFUSAL.to_string())),
        }
    }

    /// D1: each binding in `newly_listed` is the owner's own active one.
    fn check_bindings(
        newly_listed: &[CredentialBindingId],
        owned: &[UserCredentialBinding],
    ) -> Result<(), ProfileError> {
        for id in newly_listed {
            let active = owned
                .iter()
                .any(|b| &b.id == id && b.status == CredentialStatus::Active);
            if !active {
                return Err(ProfileError::Refused(binding_refusal(&id.0.to_string())));
            }
        }
        Ok(())
    }

    /// The families of the profile's active bindings, in order.
    fn active_families(
        bindings: &[CredentialBindingId],
        owned: &[UserCredentialBinding],
    ) -> Vec<String> {
        let mut families: Vec<String> = Vec::new();
        for id in bindings {
            if let Some(binding) = owned
                .iter()
                .find(|b| &b.id == id && b.status == CredentialStatus::Active)
            {
                for family in binding_families(binding) {
                    if !families.contains(&family) {
                        families.push(family);
                    }
                }
            }
        }
        families
    }

    /// D2: each pattern admits something the security context admits for
    /// the families, else D2's sentence naming it.
    fn check_tools(
        tools: &ToolAllowList,
        families: &[String],
        context: Option<&SecurityContext>,
    ) -> Result<(), ProfileError> {
        for pattern in &tools.0 {
            let admitted = families.iter().any(|f| f == pattern.family())
                && context.is_some_and(|ctx| match pattern {
                    ToolPattern::Exact(name) => ctx.permits_tool_name(name),
                    ToolPattern::Family(family) => ctx.admits_some_tool_of(family),
                });
            if !admitted {
                return Err(ProfileError::Refused(narrow_refusal(&pattern.as_string())));
            }
        }
        Ok(())
    }

    async fn check_repository(
        &self,
        tenant: &TenantId,
        repository: Option<Uuid>,
    ) -> Result<(), ProfileError> {
        let Some(id) = repository else {
            return Ok(());
        };
        let refused = || ProfileError::Refused(REPOSITORY_REFUSAL.to_string());
        let repositories = self.repositories.as_ref().ok_or_else(refused)?;
        let found = repositories
            .find_by_id(&GitRepoBindingId(id))
            .await
            .map_err(|e| ProfileError::Repository(format!("repository lookup failed: {e}")))?;
        match found {
            Some(binding) if &binding.tenant_id == tenant => Ok(()),
            _ => Err(refused()),
        }
    }

    async fn name_free(
        &self,
        tenant: &TenantId,
        sub: &str,
        name: &str,
        except: Option<ProfileId>,
    ) -> Result<(), ProfileError> {
        let taken = self
            .repo
            .list_for_owner(tenant, sub)
            .await?
            .iter()
            .any(|p| Some(p.id) != except && p.name.to_lowercase() == name.to_lowercase());
        if taken {
            return Err(ProfileError::Refused(NAME_TAKEN_REFUSAL.to_string()));
        }
        Ok(())
    }

    fn saved(outcome: ProfileSave) -> Result<(), ProfileError> {
        match outcome {
            ProfileSave::Saved => Ok(()),
            ProfileSave::NameTaken => Err(ProfileError::Refused(NAME_TAKEN_REFUSAL.to_string())),
        }
    }

    /// `POST /v1/profiles`.
    pub async fn create(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        draft: ProfileDraft,
    ) -> Result<ProfileView, ProfileError> {
        let name = check_name(&draft.name).map_err(ProfileError::Refused)?;
        let bindings = Self::parse_bindings(&draft.bindings)?;
        let tools = Self::parse_tools(&draft.tools)?;
        let repository = Self::parse_repository(draft.repository.as_ref())?;
        let notes_workspace = check_notes_workspace(draft.notes_workspace.as_deref())
            .map_err(ProfileError::Refused)?;
        let instructions =
            check_instructions(draft.instructions.as_deref()).map_err(ProfileError::Refused)?;
        let owned = self.owner_bindings(tenant, &owner.sub).await?;
        Self::check_bindings(&bindings, &owned)?;
        let context = self.security_context(owner).await?;
        Self::check_tools(
            &tools,
            &Self::active_families(&bindings, &owned),
            context.as_ref(),
        )?;
        self.check_repository(tenant, repository).await?;
        self.name_free(tenant, &owner.sub, &name, None).await?;
        let now = Utc::now();
        let profile = Profile {
            id: ProfileId::new(),
            tenant_id: tenant.clone(),
            user_sub: owner.sub.clone(),
            name,
            bindings,
            tools,
            repository,
            notes_workspace,
            instructions,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        Self::saved(self.repo.insert(&profile).await?)?;
        Ok(Self::view(profile, &owned))
    }

    /// The owner's profile, or `NotFound` for one of another person.
    async fn owned(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        id: &ProfileId,
    ) -> Result<Profile, ProfileError> {
        match self.repo.find(id).await? {
            Some(p) if p.user_sub == owner.sub && &p.tenant_id == tenant => Ok(p),
            _ => Err(ProfileError::NotFound),
        }
    }

    /// `GET /v1/profiles`: the caller's, by name.
    pub async fn list(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
    ) -> Result<Vec<ProfileView>, ProfileError> {
        let profiles = self.repo.list_for_owner(tenant, &owner.sub).await?;
        let owned = self.owner_bindings(tenant, &owner.sub).await?;
        Ok(profiles
            .into_iter()
            .map(|p| Self::view(p, &owned))
            .collect())
    }

    /// `GET /v1/profiles/{id}`.
    pub async fn get(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        id: &ProfileId,
    ) -> Result<ProfileView, ProfileError> {
        let profile = self.owned(owner, tenant, id).await?;
        let owned = self.owner_bindings(tenant, &owner.sub).await?;
        Ok(Self::view(profile, &owned))
    }

    /// `PATCH /v1/profiles/{id}`.
    pub async fn update(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        id: &ProfileId,
        patch: ProfilePatch,
    ) -> Result<ProfileView, ProfileError> {
        let mut profile = self.owned(owner, tenant, id).await?;
        let owned = self.owner_bindings(tenant, &owner.sub).await?;
        if let Some(raw) = &patch.name {
            let raw = raw.as_str().unwrap_or_default();
            profile.name = check_name(raw).map_err(ProfileError::Refused)?;
        }
        let mut recheck_tools = false;
        if let Some(raw) = &patch.bindings {
            let bindings = Self::parse_bindings(raw)?;
            let newly: Vec<CredentialBindingId> = bindings
                .iter()
                .filter(|id| !profile.bindings.contains(id))
                .copied()
                .collect();
            Self::check_bindings(&newly, &owned)?;
            profile.bindings = bindings;
            recheck_tools = true;
        }
        if let Some(raw) = &patch.tools {
            profile.tools = Self::parse_tools(raw)?;
            recheck_tools = true;
        }
        if recheck_tools {
            let context = self.security_context(owner).await?;
            Self::check_tools(
                &profile.tools,
                &Self::active_families(&profile.bindings, &owned),
                context.as_ref(),
            )?;
        }
        if let Some(raw) = &patch.repository {
            profile.repository = Self::parse_repository(Some(raw))?;
            self.check_repository(tenant, profile.repository).await?;
        }
        if let Some(raw) = &patch.notes_workspace {
            profile.notes_workspace = match raw {
                Value::Null => None,
                Value::String(text) => {
                    check_notes_workspace(Some(text)).map_err(ProfileError::Refused)?
                }
                _ => check_notes_workspace(Some("")).map_err(ProfileError::Refused)?,
            };
        }
        if let Some(raw) = &patch.instructions {
            profile.instructions = match raw {
                Value::Null => None,
                Value::String(text) => {
                    check_instructions(Some(text)).map_err(ProfileError::Refused)?
                }
                _ => {
                    return Err(ProfileError::Refused(
                        crate::domain::profile::INSTRUCTIONS_REFUSAL.to_string(),
                    ))
                }
            };
        }
        self.name_free(tenant, &owner.sub, &profile.name, Some(profile.id))
            .await?;
        profile.updated_at = Utc::now();
        match self.repo.update(&profile).await {
            Ok(outcome) => Self::saved(outcome)?,
            Err(RepositoryError::NotFound(_)) => return Err(ProfileError::NotFound),
            Err(e) => return Err(e.into()),
        }
        Ok(Self::view(profile, &owned))
    }

    /// `DELETE /v1/profiles/{id}`.
    pub async fn delete(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        id: &ProfileId,
    ) -> Result<(), ProfileError> {
        let profile = self.owned(owner, tenant, id).await?;
        if self.repo.delete(&profile.id, Utc::now()).await? {
            Ok(())
        } else {
            Err(ProfileError::NotFound)
        }
    }

    /// `POST /v1/profiles/available-tools`: the tools the caller's security
    /// context admits for the families of `bindings` (the grant's R1).
    pub async fn available_tools(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        bindings: &Value,
    ) -> Result<Vec<AvailableTool>, ProfileError> {
        let bindings = Self::parse_bindings(bindings)?;
        let owned = self.owner_bindings(tenant, &owner.sub).await?;
        Self::check_bindings(&bindings, &owned)?;
        let Some(context) = self.security_context(owner).await? else {
            return Ok(Vec::new());
        };
        let builtin = self.catalogue.builtin_tools();
        let capability_names = self.catalogue.capability_names();
        let mut answered: Vec<AvailableTool> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let in_family =
            |name: &str, family: &str| ToolPattern::Family(family.to_string()).matches(name);
        for family in Self::active_families(&bindings, &owned) {
            let builtin_of: Vec<&(String, String)> = builtin
                .iter()
                .filter(|(name, _)| in_family(name, &family))
                .collect();
            if !builtin_of.is_empty() {
                for (name, description) in builtin_of {
                    if context.permits_tool_name(name) && seen.insert(name.clone()) {
                        answered.push(AvailableTool {
                            name: name.clone(),
                            description: description.clone(),
                            family: family.clone(),
                            gated: self.catalogue.requires_approval(name),
                        });
                    }
                }
                continue;
            }
            for name in capability_names.iter().filter(|n| in_family(n, &family)) {
                if context.permits_tool_name(name) && seen.insert(name.clone()) {
                    answered.push(AvailableTool {
                        name: name.clone(),
                        description: String::new(),
                        family: family.clone(),
                        gated: self.catalogue.requires_approval(name),
                    });
                }
            }
            let every = format!("{family}.*");
            if context.admits_some_tool_of(&family) && seen.insert(every.clone()) {
                answered.push(AvailableTool {
                    name: every,
                    description: format!("Every tool of {family} your plan allows."),
                    family: family.clone(),
                    gated: false,
                });
            }
        }
        Ok(answered)
    }
}

#[cfg(test)]
#[path = "profile_service_tests.rs"]
mod profile_service_tests;
