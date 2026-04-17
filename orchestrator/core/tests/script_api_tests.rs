// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Script API-Boundary Tests (BC-7, ADR-110 §D7)
//!
//! Exercises the contract the HTTP handlers depend on — status-code
//! mapping, ownership invariants, and DTO shape — at the
//! [`ScriptService`] boundary. Handler-layer wiring (JWT extraction,
//! routing, serialization) lives in `cli/src/daemon/handlers/script.rs`
//! and is validated end-to-end at the daemon smoke-test level.
//!
//! | HTTP behaviour | Covered here by |
//! |---|---|
//! | `POST /v1/scripts` → 201 + version 1 | `post_returns_version_one` |
//! | `GET /v1/scripts/:id` wrong owner → 404 | `get_wrong_owner_returns_not_found` |
//! | `PUT /v1/scripts/:id` → 200 + version 2 | `put_bumps_version` |
//! | `DELETE /v1/scripts/:id` → 204; list excludes | `delete_removes_from_list` |
//! | Duplicate name on create → 409 | `create_duplicate_name_conflict` |
//! | Over tier limit on create → 422 | `create_over_tier_limit_unprocessable` |
//! | Domain validation error → 400 | `invalid_tag_is_domain_error` |

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use aegis_orchestrator_core::application::script_service::{
    CreateScriptCommand, ScriptService, ScriptServiceError, UpdateScriptCommand,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::RepositoryError;
use aegis_orchestrator_core::domain::script::{
    Script, ScriptError, ScriptId, ScriptRepository, ScriptVersionSummary,
};
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;

// ============================================================================
// In-memory ScriptRepository — duplicated from service_tests to keep this
// integration target self-contained (cargo treats shared modules in
// tests/common as an integration binary on their own, so we prefer
// per-file copies).
// ============================================================================

#[derive(Default)]
struct InMemoryScriptRepository {
    current: RwLock<HashMap<ScriptId, Script>>,
    history: RwLock<HashMap<ScriptId, BTreeMap<u32, (Script, chrono::DateTime<chrono::Utc>)>>>,
}

#[async_trait]
impl ScriptRepository for InMemoryScriptRepository {
    async fn save(&self, script: &Script) -> Result<(), RepositoryError> {
        self.current
            .write()
            .unwrap()
            .insert(script.id, script.clone());
        if script.deleted_at.is_none() {
            let mut hist = self.history.write().unwrap();
            let per_script = hist.entry(script.id).or_default();
            per_script.insert(script.version, (script.clone(), script.updated_at));
        }
        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<Option<Script>, RepositoryError> {
        let current = self.current.read().unwrap();
        Ok(current
            .get(id)
            .filter(|s| &s.tenant_id == tenant_id && s.deleted_at.is_none())
            .cloned())
    }

    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<Vec<Script>, RepositoryError> {
        let current = self.current.read().unwrap();
        Ok(current
            .values()
            .filter(|s| {
                &s.tenant_id == tenant_id && s.created_by == created_by && s.deleted_at.is_none()
            })
            .cloned()
            .collect())
    }

    async fn find_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Script>, RepositoryError> {
        let current = self.current.read().unwrap();
        Ok(current
            .values()
            .find(|s| &s.tenant_id == tenant_id && s.name == name && s.deleted_at.is_none())
            .cloned())
    }

    async fn list_versions(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<Vec<ScriptVersionSummary>, RepositoryError> {
        let current = self.current.read().unwrap();
        let parent = match current.get(id) {
            Some(p) if &p.tenant_id == tenant_id => p.clone(),
            _ => return Ok(Vec::new()),
        };
        drop(current);
        let hist = self.history.read().unwrap();
        Ok(hist
            .get(id)
            .map(|per_script| {
                per_script
                    .iter()
                    .map(|(version, (_s, updated_at))| ScriptVersionSummary {
                        version: *version,
                        updated_at: *updated_at,
                        updated_by: parent.created_by.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get_version(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        version: u32,
    ) -> Result<Option<Script>, RepositoryError> {
        let current = self.current.read().unwrap();
        if !current
            .get(id)
            .map(|p| &p.tenant_id == tenant_id)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        drop(current);
        let hist = self.history.read().unwrap();
        Ok(hist
            .get(id)
            .and_then(|per_script| per_script.get(&version).map(|(s, _)| s.clone())))
    }

    async fn soft_delete(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        let mut current = self.current.write().unwrap();
        if let Some(s) = current.get_mut(id) {
            if &s.tenant_id == tenant_id && s.deleted_at.is_none() {
                s.deleted_at = Some(chrono::Utc::now());
            }
        }
        Ok(())
    }

    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<u32, RepositoryError> {
        let current = self.current.read().unwrap();
        Ok(current
            .values()
            .filter(|s| {
                &s.tenant_id == tenant_id && s.created_by == created_by && s.deleted_at.is_none()
            })
            .count() as u32)
    }
}

fn build_service() -> ScriptService {
    let event_bus = Arc::new(EventBus::new(64));
    let repo = Arc::new(InMemoryScriptRepository::default()) as Arc<dyn ScriptRepository>;
    ScriptService::new(repo, event_bus)
}

fn create_cmd(
    tenant: TenantId,
    owner: &str,
    tier: ZaruTier,
    name: &str,
    code: &str,
) -> CreateScriptCommand {
    CreateScriptCommand {
        tenant_id: tenant,
        created_by: owner.to_string(),
        zaru_tier: tier,
        name: name.to_string(),
        description: "auto".to_string(),
        code: code.to_string(),
        tags: vec![],
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn post_returns_version_one() {
    // POST /v1/scripts → handler wraps the returned aggregate as 201
    // with version=1.
    let svc = build_service();
    let s = svc
        .create(create_cmd(
            TenantId::consumer(),
            "alice",
            ZaruTier::Pro,
            "first",
            "code",
        ))
        .await
        .unwrap();
    assert_eq!(s.version, 1);
    assert!(s.deleted_at.is_none());
    assert_eq!(s.visibility.as_str(), "private");
}

#[tokio::test]
async fn get_wrong_owner_returns_not_found() {
    // GET /v1/scripts/:id with a different owner → handler maps
    // ScriptServiceError::NotFound to 404.
    let svc = build_service();
    let t = TenantId::consumer();
    let s = svc
        .create(create_cmd(t.clone(), "alice", ZaruTier::Pro, "a", "x"))
        .await
        .unwrap();

    let res = svc.get(&s.id, &t, "mallory").await;
    assert!(
        matches!(res, Err(ScriptServiceError::NotFound)),
        "non-owner sees NotFound → 404, got {res:?}"
    );
}

#[tokio::test]
async fn put_bumps_version() {
    // PUT /v1/scripts/:id → 200 with bumped version.
    let svc = build_service();
    let t = TenantId::consumer();
    let s = svc
        .create(create_cmd(t.clone(), "alice", ZaruTier::Pro, "x", "v1"))
        .await
        .unwrap();
    let updated = svc
        .update(
            &s.id,
            &t,
            "alice",
            UpdateScriptCommand {
                name: "x".to_string(),
                description: "".to_string(),
                code: "v2".to_string(),
                tags: vec![],
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.code, "v2");

    // list_versions sees both 1 and 2.
    let versions = svc.list_versions(&s.id, &t, "alice").await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[1].version, 2);
}

#[tokio::test]
async fn delete_removes_from_list() {
    // DELETE /v1/scripts/:id → 204; GET /v1/scripts no longer
    // returns the row, and GET /v1/scripts/:id → 404.
    let svc = build_service();
    let t = TenantId::consumer();
    let s = svc
        .create(create_cmd(t.clone(), "alice", ZaruTier::Pro, "solo", "x"))
        .await
        .unwrap();

    svc.delete(&s.id, &t, "alice").await.unwrap();

    let list = svc.list(&t, "alice").await.unwrap();
    assert!(list.is_empty());

    let get = svc.get(&s.id, &t, "alice").await;
    assert!(matches!(get, Err(ScriptServiceError::NotFound)));
}

#[tokio::test]
async fn create_duplicate_name_conflict() {
    // POST /v1/scripts with duplicate name → 409 Conflict.
    let svc = build_service();
    let t = TenantId::consumer();
    svc.create(create_cmd(t.clone(), "alice", ZaruTier::Pro, "dup", "a"))
        .await
        .unwrap();
    let second = svc
        .create(create_cmd(t.clone(), "alice", ZaruTier::Pro, "dup", "b"))
        .await;
    assert!(matches!(second, Err(ScriptServiceError::DuplicateName)));
}

#[tokio::test]
async fn create_over_tier_limit_unprocessable() {
    // POST /v1/scripts over tier quota → 422 Unprocessable Entity.
    let svc = build_service();
    let t = TenantId::consumer();
    for i in 0..5 {
        svc.create(create_cmd(
            t.clone(),
            "alice",
            ZaruTier::Free,
            &format!("s{i}"),
            "x",
        ))
        .await
        .unwrap();
    }
    let sixth = svc
        .create(create_cmd(t.clone(), "alice", ZaruTier::Free, "s5", "x"))
        .await;
    assert!(matches!(
        sixth,
        Err(ScriptServiceError::TierLimitExceeded { max: 5 })
    ));
}

#[tokio::test]
async fn invalid_tag_is_domain_error() {
    // POST /v1/scripts with invalid tag → handler maps
    // ScriptServiceError::Domain(_) to 400 Bad Request.
    let svc = build_service();
    let t = TenantId::consumer();
    let res = svc
        .create(CreateScriptCommand {
            tenant_id: t,
            created_by: "alice".to_string(),
            zaru_tier: ZaruTier::Pro,
            name: "ok".to_string(),
            description: "".to_string(),
            code: "x".to_string(),
            tags: vec!["INVALID".to_string()],
        })
        .await;
    assert!(matches!(
        res,
        Err(ScriptServiceError::Domain(ScriptError::TagInvalid(_)))
    ));
}
