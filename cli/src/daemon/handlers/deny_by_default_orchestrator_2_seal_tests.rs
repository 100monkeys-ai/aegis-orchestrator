// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Zaru ADR-0071 D1 (c) and D3 at `/v1/seal/attest` and `GET /v1/seal/tools`:
//! the route resolves which credential attests the session (an `aegis_*`
//! key, carrying the profile stored for it, or the person's own session
//! token) and hands it to the attestation service, which records it; a
//! key's listing that names no profile in `X-Zaru-Profile` is for the key's
//! stored profile.

use aegis_orchestrator_core::domain::iam::{RealmKind, ZaruTier};
use aegis_orchestrator_core::domain::seal_session::AttestingCredential;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::ApiKeyRow;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::json;
use uuid::Uuid;

use super::{
    attested_request, key_credential, listing_credential, AttestBinding, HttpAttestationRequest,
    PROFILE_HEADER,
};
use crate::daemon::api_key_identity::resolve_api_key;
use crate::daemon::api_key_identity::test_keys::{key_row, KeyTable};

const SUB: &str = "5e6f0000-key-owner";
const KEY: &str = "aegis_deny-by-default-key";

/// The key `KEY` of `SUB`, stored with `profile`, as the route resolves it.
async fn key_with(profile: Option<Uuid>) -> AttestingCredential {
    let tenant = TenantId::for_consumer_user(SUB).unwrap();
    let table = KeyTable {
        rows: vec![ApiKeyRow {
            profile_id: profile,
            ..key_row(KEY, SUB, tenant.as_str(), None)
        }],
        escalations: None,
    };
    let resolved = resolve_api_key(Some(&table), KEY)
        .await
        .expect("the key resolves");
    key_credential(&resolved)
}

/// D3: a key attests as an API key carrying the profile stored for it, or
/// none.
#[tokio::test]
async fn a_key_attests_as_an_api_key_carrying_its_stored_profile() {
    let profile = Uuid::new_v4();
    assert_eq!(
        key_with(Some(profile)).await,
        AttestingCredential::ApiKey {
            profile: Some(profile)
        }
    );
    assert_eq!(
        key_with(None).await,
        AttestingCredential::ApiKey { profile: None }
    );
}

/// D1 (c): the attestation request the route sends carries the credential
/// it resolved, for a key and for the person's own token alike.
#[test]
fn the_attestation_request_carries_the_credential_the_route_resolved() {
    let request: HttpAttestationRequest = serde_json::from_value(json!({
        "public_key": "a2V5",
        "security_context": "zaru-pro",
    }))
    .unwrap();
    let binding = AttestBinding {
        tenant_id: TenantId::for_consumer_user(SUB).unwrap(),
        realm: RealmKind::Consumer,
        user_id: Some(SUB.to_string()),
        zaru_tier: Some(ZaruTier::Pro),
        escalation: None,
    };
    let profile = Uuid::new_v4();
    for credential in [
        AttestingCredential::Interactive,
        AttestingCredential::ApiKey {
            profile: Some(profile),
        },
        AttestingCredential::ApiKey { profile: None },
    ] {
        let sent = attested_request(&request, &binding, credential);
        assert_eq!(sent.attested_by, credential);
        assert_eq!(
            sent.user_id.as_deref(),
            Some(SUB),
            "the binding still governs"
        );
    }
}

fn bearer(token: &str, profile: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    if let Some(profile) = profile {
        headers.insert(PROFILE_HEADER, HeaderValue::from_str(profile).unwrap());
    }
    headers
}

/// D3: a key's listing that names no profile is for the key's stored one;
/// one it names stands; the person's own token lists as itself.
#[test]
fn a_keys_listing_naming_no_profile_is_for_its_stored_profile() {
    let stored = Uuid::new_v4();
    let named = Uuid::new_v4();
    assert_eq!(
        listing_credential(&bearer("aegis_k", None), Some(stored)),
        Ok(AttestingCredential::ApiKey {
            profile: Some(stored)
        })
    );
    assert_eq!(
        listing_credential(&bearer("aegis_k", Some(&named.to_string())), Some(stored)),
        Ok(AttestingCredential::ApiKey {
            profile: Some(named)
        })
    );
    assert_eq!(
        listing_credential(&bearer("aegis_k", None), None),
        Ok(AttestingCredential::ApiKey { profile: None })
    );
    assert_eq!(
        listing_credential(&bearer("eyJ.person.token", None), None),
        Ok(AttestingCredential::Interactive)
    );
}
