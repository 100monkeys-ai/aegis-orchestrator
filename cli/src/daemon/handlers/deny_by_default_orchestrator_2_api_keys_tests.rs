// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Zaru ADR-0071 D3 and D6 (deny by default): an API key carries the
//! profile chosen for it. `POST /v1/api-keys` takes an optional `profile`,
//! `PATCH /v1/api-keys/{id}` sets or clears it with `{"profile": id or
//! null}` and answers the key as listed, each key of `GET /v1/api-keys`
//! carries `profile` and `profile_name`, and the validation answer carries
//! `profile`. A key never carries another person's profile.

use std::collections::HashMap;
use std::sync::Mutex;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::{
    ApiKeyRow, CreateApiKeyRow,
};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::{
    create_answer, list_answer, update_answer, validate_answer, ApiKeyStore, CreateApiKeyRequest,
    KeyProfiles, PROFILE_REFUSAL,
};
use crate::daemon::api_key_identity::test_keys::{key_row, KeyTable};
use crate::daemon::handlers::test_support::consumer;

const OWNER: &str = "owner-sub";
const OTHER: &str = "other-sub";

/// The `api_keys` table as the repository keeps it.
#[derive(Default)]
struct Keys(Mutex<Vec<ApiKeyRow>>);

#[async_trait::async_trait]
impl ApiKeyStore for Keys {
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<ApiKeyRow>, String> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn create(&self, row: &CreateApiKeyRow) -> Result<ApiKeyRow, String> {
        let stored = ApiKeyRow {
            id: row.id,
            user_id: row.user_id.clone(),
            name: row.name.clone(),
            key_hash: row.key_hash.clone(),
            scopes: row.scopes.clone(),
            expires_at: row.expires_at,
            last_used_at: None,
            created_at: chrono::Utc::now(),
            status: "active".into(),
            tenant_id: row.tenant_id.clone(),
            aegis_role: row.aegis_role.clone(),
            zaru_tier: row.zaru_tier.clone(),
            profile_id: row.profile_id,
        };
        self.0.lock().unwrap().push(stored.clone());
        Ok(stored)
    }

    async fn set_profile(
        &self,
        id: Uuid,
        user_id: &str,
        profile_id: Option<Uuid>,
    ) -> Result<bool, String> {
        let mut rows = self.0.lock().unwrap();
        match rows
            .iter_mut()
            .find(|r| r.id == id && r.user_id == user_id && r.status == "active")
        {
            Some(row) => {
                row.profile_id = profile_id;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Each person's profiles by id and name.
struct Profiles(HashMap<String, Vec<(Uuid, String)>>);

#[async_trait::async_trait]
impl KeyProfiles for Profiles {
    async fn owned_profiles(
        &self,
        owner: &UserIdentity,
        _tenant: &TenantId,
    ) -> Result<Vec<(Uuid, String)>, String> {
        Ok(self.0.get(&owner.sub).cloned().unwrap_or_default())
    }
}

struct World {
    keys: Keys,
    profiles: Profiles,
    mine: Uuid,
    theirs: Uuid,
}

fn world() -> World {
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    let mut profiles = HashMap::new();
    profiles.insert(OWNER.to_string(), vec![(mine, "Mail only".to_string())]);
    profiles.insert(OTHER.to_string(), vec![(theirs, "Theirs".to_string())]);
    World {
        keys: Keys::default(),
        profiles: Profiles(profiles),
        mine,
        theirs,
    }
}

fn tenant(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).unwrap()
}

fn request(profile: Option<Uuid>) -> CreateApiKeyRequest {
    serde_json::from_value(json!({
        "name": "mcp",
        "scopes": [],
        "profile": profile,
    }))
    .unwrap()
}

/// D3: a key created with one of the person's profiles carries it, and the
/// list answers it with the profile's name.
#[tokio::test]
async fn a_key_created_with_one_of_your_profiles_carries_it_and_is_listed_with_its_name() {
    let w = world();
    let owner = consumer(OWNER);
    let (status, made) = create_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        request(Some(w.mine)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["profile"], json!(w.mine), "{made}");
    assert_eq!(made["profile_name"], json!("Mail only"), "{made}");
    assert!(
        made["key"].as_str().unwrap().starts_with("aegis_"),
        "{made}"
    );

    let (status, listed) = list_answer(&w.keys, Some(&w.profiles), &owner, &tenant(OWNER)).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let key = &listed["api_keys"][0];
    assert_eq!(key["profile"], json!(w.mine), "{listed}");
    assert_eq!(key["profile_name"], json!("Mail only"), "{listed}");
}

/// D3: "An API key will carry a profile chosen for it on the API keys
/// page, none by default."
#[tokio::test]
async fn a_key_created_with_no_profile_carries_none() {
    let w = world();
    let owner = consumer(OWNER);
    let (status, made) = create_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        request(None),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert!(made["profile"].is_null(), "{made}");
    assert!(made["profile_name"].is_null(), "{made}");
    let (_, listed) = list_answer(&w.keys, Some(&w.profiles), &owner, &tenant(OWNER)).await;
    assert!(listed["api_keys"][0]["profile"].is_null(), "{listed}");
    assert!(listed["api_keys"][0]["profile_name"].is_null(), "{listed}");
}

/// A key never carries another person's profile, at creation or by PATCH:
/// refused before anything is stored.
#[tokio::test]
async fn a_key_never_carries_another_persons_profile() {
    let w = world();
    let owner = consumer(OWNER);
    let (status, refused) = create_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        request(Some(w.theirs)),
    )
    .await;
    assert_eq!(
        (status, refused["error"].clone()),
        (StatusCode::BAD_REQUEST, json!(PROFILE_REFUSAL))
    );
    assert!(w.keys.0.lock().unwrap().is_empty(), "nothing stored");

    let (_, made) = create_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        request(None),
    )
    .await;
    let id: Uuid = serde_json::from_value(made["id"].clone()).unwrap();
    let (status, refused) = update_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        id,
        json!({"profile": w.theirs}),
    )
    .await;
    assert_eq!(
        (status, refused["error"].clone()),
        (StatusCode::BAD_REQUEST, json!(PROFILE_REFUSAL))
    );
    assert_eq!(w.keys.0.lock().unwrap()[0].profile_id, None);
}

/// `PATCH /v1/api-keys/{id}` with `{"profile": id}` sets the profile and
/// with `{"profile": null}` clears it, each answering the key as listed;
/// a body without `profile`, or with anything but an id or null, is
/// refused; another person's key, or none, answers 404.
#[tokio::test]
async fn patch_sets_and_clears_the_profile_and_answers_the_key_as_listed() {
    let w = world();
    let owner = consumer(OWNER);
    let (_, made) = create_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        request(None),
    )
    .await;
    let id: Uuid = serde_json::from_value(made["id"].clone()).unwrap();

    let (status, set) = update_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        id,
        json!({"profile": w.mine}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    let (_, listed) = list_answer(&w.keys, Some(&w.profiles), &owner, &tenant(OWNER)).await;
    assert_eq!(set, listed["api_keys"][0], "the key as listed");
    assert_eq!(set["profile"], json!(w.mine));
    assert_eq!(set["profile_name"], json!("Mail only"));
    assert!(
        set.get("key").is_none(),
        "the key value is never answered again"
    );

    let (status, cleared) = update_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        id,
        json!({"profile": null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["profile"].is_null(), "{cleared}");
    assert!(cleared["profile_name"].is_null(), "{cleared}");

    for body in [
        json!({}),
        json!({"profile": "not-an-id"}),
        json!({"profile": 7}),
    ] {
        let (status, refused) = update_answer(
            &w.keys,
            Some(&w.profiles),
            &owner,
            &tenant(OWNER),
            id,
            body.clone(),
        )
        .await;
        assert_eq!(
            (status, refused["error"].clone()),
            (StatusCode::BAD_REQUEST, json!(PROFILE_REFUSAL)),
            "{body}"
        );
    }

    let other = consumer(OTHER);
    let (status, _) = update_answer(
        &w.keys,
        Some(&w.profiles),
        &other,
        &tenant(OTHER),
        id,
        json!({"profile": null}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "another person's key");
    let (status, _) = update_answer(
        &w.keys,
        Some(&w.profiles),
        &owner,
        &tenant(OWNER),
        Uuid::new_v4(),
        json!({"profile": null}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no such key");
}

/// A key whose profile was deleted since is listed with its id and no
/// name: it reaches none until another is chosen.
#[tokio::test]
async fn a_key_whose_profile_was_deleted_is_listed_without_a_name() {
    let w = world();
    let owner = consumer(OWNER);
    let gone = Uuid::new_v4();
    w.keys.0.lock().unwrap().push(ApiKeyRow {
        profile_id: Some(gone),
        ..key_row("aegis_k", OWNER, tenant(OWNER).as_str(), None)
    });
    let (_, listed) = list_answer(&w.keys, Some(&w.profiles), &owner, &tenant(OWNER)).await;
    assert_eq!(listed["api_keys"][0]["profile"], json!(gone), "{listed}");
    assert!(listed["api_keys"][0]["profile_name"].is_null(), "{listed}");
}

/// The validation answer carries the key's `profile`, which the Zaru MCP
/// server forwards on every call and listing; a key with none answers null.
#[tokio::test]
async fn the_validation_answer_carries_the_keys_profile() {
    let profile = Uuid::new_v4();
    let tenant = tenant(OWNER);
    let with = ApiKeyRow {
        profile_id: Some(profile),
        ..key_row("aegis_with", OWNER, tenant.as_str(), None)
    };
    let without = key_row("aegis_without", OWNER, tenant.as_str(), None);
    let table = KeyTable {
        rows: vec![with, without],
        escalations: None,
    };
    let (status, body) = validate_answer(Some(&table), "aegis_with").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["profile"], json!(profile), "{body}");
    let (_, body) = validate_answer(Some(&table), "aegis_without").await;
    assert!(body.get("profile").is_some_and(|p| p.is_null()), "{body}");
}
