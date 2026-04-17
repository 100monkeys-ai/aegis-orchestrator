// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `ScriptService` Integration Tests (BC-7, ADR-110 §D7)
//!
//! Covers the application-service surface with an in-memory
//! [`ScriptRepository`] so no database is required.
//!
//! | Scenario | Test |
//! |---|---|
//! | Create under tier limit | `create_under_tier_limit_succeeds` |
//! | Create over tier limit | `create_over_tier_limit_rejects` |
//! | Create with duplicate name | `create_duplicate_name_rejects` |
//! | Update bumps version + writes history | `update_bumps_version_and_writes_history` |
//! | Update rejects duplicate name | `update_duplicate_name_rejects` |
//! | Delete soft-deletes; list excludes | `delete_soft_deletes_and_excludes_from_list` |
//! | Non-owner `get` returns NotFound | `get_wrong_owner_returns_not_found` |
//! | Version-history survives delete | `version_history_survives_soft_delete` |
//! | `Created`/`Updated`/`Deleted` events publish | `lifecycle_events_are_published` |

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;

use aegis_orchestrator_core::application::script_service::{
    CreateScriptCommand, ScriptService, ScriptServiceError, UpdateScriptCommand,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::RepositoryError;
use aegis_orchestrator_core::domain::script::{
    Script, ScriptId, ScriptRepository, ScriptVersionSummary,
};
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};

// ============================================================================
// In-memory ScriptRepository — version-history aware
// ============================================================================

#[derive(Default)]
struct InMemoryScriptRepository {
    /// Current-version rows by id. Corresponds to the `scripts` table.
    current: RwLock<HashMap<ScriptId, Script>>,
    /// Version history by (script_id, version). Corresponds to the
    /// `script_versions` table.
    history: RwLock<HashMap<ScriptId, BTreeMap<u32, (Script, chrono::DateTime<chrono::Utc>)>>>,
}

#[async_trait]
impl ScriptRepository for InMemoryScriptRepository {
    async fn save(&self, script: &Script) -> Result<(), RepositoryError> {
        self.current
            .write()
            .unwrap()
            .insert(script.id, script.clone());

        // Mirror the Postgres repo: only write history for active
        // (non-deleted) rows whose buffered events aren't a delete.
        let history_write_needed = script.deleted_at.is_none();
        if history_write_needed {
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
        let mut out: Vec<Script> = current
            .values()
            .filter(|s| {
                &s.tenant_id == tenant_id && s.created_by == created_by && s.deleted_at.is_none()
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
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
        // Parent tenant check — use the current row even if soft-deleted
        // so history persists for audit.
        let current = self.current.read().unwrap();
        let parent = match current.get(id) {
            Some(p) if &p.tenant_id == tenant_id => p.clone(),
            _ => return Ok(Vec::new()),
        };
        drop(current);

        let hist = self.history.read().unwrap();
        let Some(per_script) = hist.get(id) else {
            return Ok(Vec::new());
        };
        Ok(per_script
            .iter()
            .map(|(version, (_s, updated_at))| ScriptVersionSummary {
                version: *version,
                updated_at: *updated_at,
                updated_by: parent.created_by.clone(),
            })
            .collect())
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
                s.updated_at = chrono::Utc::now();
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

// ============================================================================
// Fixture + capture helper
// ============================================================================

struct Fixture {
    service: ScriptService,
    event_bus: Arc<EventBus>,
    repo: Arc<InMemoryScriptRepository>,
}

fn build_fixture() -> Fixture {
    let event_bus = Arc::new(EventBus::new(64));
    let repo = Arc::new(InMemoryScriptRepository::default());
    let service = ScriptService::new(repo.clone() as Arc<dyn ScriptRepository>, event_bus.clone());
    Fixture {
        service,
        event_bus,
        repo,
    }
}

fn captured_events(bus: &EventBus) -> Arc<Mutex<Vec<DomainEvent>>> {
    let captured = Arc::new(Mutex::new(Vec::<DomainEvent>::new()));
    let mut rx = bus.subscribe();
    let captured_clone = captured.clone();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            captured_clone.lock().unwrap().push(event);
        }
    });
    captured
}

fn cmd(
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
async fn create_under_tier_limit_succeeds() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let script = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "hello", "return 1;"))
        .await
        .expect("should succeed");
    assert_eq!(script.name, "hello");
    assert_eq!(script.version, 1);
    assert!(script.deleted_at.is_none());
}

#[tokio::test]
async fn create_over_tier_limit_rejects() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    // Free tier = 5 scripts max.
    for i in 0..5 {
        fx.service
            .create(cmd(
                t.clone(),
                "alice",
                ZaruTier::Free,
                &format!("s{i}"),
                "x",
            ))
            .await
            .expect("within quota");
    }

    let sixth = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Free, "s5", "x"))
        .await;
    assert!(
        matches!(sixth, Err(ScriptServiceError::TierLimitExceeded { max: 5 })),
        "got {sixth:?}"
    );
}

#[tokio::test]
async fn create_duplicate_name_rejects() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    fx.service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "dup", "a"))
        .await
        .unwrap();
    let again = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "dup", "b"))
        .await;
    assert!(matches!(again, Err(ScriptServiceError::DuplicateName)));
}

#[tokio::test]
async fn get_wrong_owner_returns_not_found() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let script = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "a", "x"))
        .await
        .unwrap();

    let res = fx.service.get(&script.id, &t, "mallory").await;
    assert!(
        matches!(res, Err(ScriptServiceError::NotFound)),
        "non-owner MUST see NotFound, got {res:?}"
    );
}

#[tokio::test]
async fn update_bumps_version_and_writes_history() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let script = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "a", "x"))
        .await
        .unwrap();
    assert_eq!(script.version, 1);

    let updated = fx
        .service
        .update(
            &script.id,
            &t,
            "alice",
            UpdateScriptCommand {
                name: "a".to_string(),
                description: "changed".to_string(),
                code: "y".to_string(),
                tags: vec!["t1".to_string()],
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.code, "y");

    // History now has v1 and v2.
    let versions = fx
        .service
        .list_versions(&script.id, &t, "alice")
        .await
        .unwrap();
    let vs: Vec<u32> = versions.iter().map(|s| s.version).collect();
    assert_eq!(vs, vec![1, 2]);
}

#[tokio::test]
async fn update_duplicate_name_rejects() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    fx.service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "one", "x"))
        .await
        .unwrap();
    let two = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "two", "x"))
        .await
        .unwrap();

    let res = fx
        .service
        .update(
            &two.id,
            &t,
            "alice",
            UpdateScriptCommand {
                name: "one".to_string(),
                description: "".to_string(),
                code: "x".to_string(),
                tags: vec![],
            },
        )
        .await;
    assert!(matches!(res, Err(ScriptServiceError::DuplicateName)));
}

#[tokio::test]
async fn delete_soft_deletes_and_excludes_from_list() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let script = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "solo", "x"))
        .await
        .unwrap();

    fx.service
        .delete(&script.id, &t, "alice")
        .await
        .expect("delete should succeed");

    // Absent from list.
    let list = fx.service.list(&t, "alice").await.unwrap();
    assert!(
        list.is_empty(),
        "soft-deleted scripts must not appear in list"
    );

    // get() returns NotFound.
    let res = fx.service.get(&script.id, &t, "alice").await;
    assert!(matches!(res, Err(ScriptServiceError::NotFound)));

    // Raw repo find_by_id also filters out soft-deleted rows (this is
    // what the tenant-scoped `find_by_id` guarantees).
    let direct = fx.repo.find_by_id(&script.id, &t).await.unwrap();
    assert!(direct.is_none());
}

#[tokio::test]
async fn version_history_survives_soft_delete() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let script = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "histtest", "v1"))
        .await
        .unwrap();
    fx.service
        .update(
            &script.id,
            &t,
            "alice",
            UpdateScriptCommand {
                name: "histtest".to_string(),
                description: "".to_string(),
                code: "v2".to_string(),
                tags: vec![],
            },
        )
        .await
        .unwrap();
    fx.service.delete(&script.id, &t, "alice").await.unwrap();

    // list_versions goes directly against the repo (service check
    // would fail because `get` returns NotFound on a soft-deleted
    // script).
    let versions = fx.repo.list_versions(&script.id, &t).await.unwrap();
    let vs: Vec<u32> = versions.iter().map(|s| s.version).collect();
    assert_eq!(
        vs,
        vec![1, 2],
        "version-history table must survive soft-delete for audit"
    );
}

#[tokio::test]
async fn lifecycle_events_are_published() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let events = captured_events(&fx.event_bus);

    let script = fx
        .service
        .create(cmd(t.clone(), "alice", ZaruTier::Pro, "ev", "x"))
        .await
        .unwrap();
    fx.service
        .update(
            &script.id,
            &t,
            "alice",
            UpdateScriptCommand {
                name: "ev".to_string(),
                description: "".to_string(),
                code: "y".to_string(),
                tags: vec![],
            },
        )
        .await
        .unwrap();
    fx.service.delete(&script.id, &t, "alice").await.unwrap();

    // Give the broadcast channel a moment to drain.
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let got = events.lock().unwrap().clone();

    use aegis_orchestrator_core::domain::events::ScriptEvent;
    let mut saw_created = false;
    let mut saw_updated = false;
    let mut saw_deleted = false;
    for e in &got {
        if let DomainEvent::Script(s) = e {
            match s {
                ScriptEvent::Created { .. } => saw_created = true,
                ScriptEvent::Updated { .. } => saw_updated = true,
                ScriptEvent::Deleted { .. } => saw_deleted = true,
            }
        }
    }
    assert!(saw_created && saw_updated && saw_deleted, "events: {got:?}");
}
