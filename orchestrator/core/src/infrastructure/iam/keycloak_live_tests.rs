// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The Keycloak admin client against a real Keycloak.
//!
//! The stand-in used by the other tests of this client answers as Keycloak
//! is documented to. These tests check the same writes against Keycloak
//! itself: that it accepts every setting a new realm is created with, that
//! a user write keeps what it must, and that a SAML provider is stored with
//! its checks on.
//!
//! They need a Keycloak whose `master` realm has an administrator `admin`
//! with the password `admin`, at `AEGIS_TEST_KEYCLOAK_URL` (CI starts one in
//! development mode on the loopback interface). Without the variable they
//! do nothing and say so; where `AEGIS_TEST_KEYCLOAK` is set (CI sets it) a
//! missing variable fails them instead, so they cannot pass there without
//! running. Each test makes its own realm, except the invitation test, which
//! needs the consumer realm by name and makes its own users in it.

use super::tests::{saml_config, weakened_checks, TEST_IDP_CERTIFICATE_PEM};
use super::*;

/// The Keycloak to test against, or `None` when there is none and none is
/// required.
fn keycloak(test: &str) -> Option<KeycloakAdminClient> {
    let url = match std::env::var("AEGIS_TEST_KEYCLOAK_URL") {
        Ok(url) if !url.is_empty() => url,
        _ if std::env::var_os("AEGIS_TEST_KEYCLOAK").is_some() => {
            panic!("{test}: AEGIS_TEST_KEYCLOAK is set and AEGIS_TEST_KEYCLOAK_URL is not")
        }
        _ => {
            eprintln!(
                "SKIPPED {test}: AEGIS_TEST_KEYCLOAK_URL is not set. This test ran nothing. \
                 CI starts a Keycloak and sets it."
            );
            return None;
        }
    };
    Some(KeycloakAdminClient::new(KeycloakAdminConfig {
        host: url.trim_end_matches('/').to_string(),
        admin_username: "admin".to_string(),
        admin_password: crate::domain::secrets::SensitiveString::new("admin"),
    }))
}

/// A name no other test uses.
fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

/// Call the admin API directly, for what the client under test does not do
/// (reading a realm back, making a user as a test needs it). Returns the
/// status, the `Location` header and the body.
async fn admin(
    kc: &KeycloakAdminClient,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, Option<String>, serde_json::Value) {
    let token = kc.get_admin_token().await.expect("an admin token");
    let mut request = kc
        .http
        .request(method, format!("{}{path}", kc.config.host))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let resp = request.send().await.expect("Keycloak answers");
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let text = resp.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, location, body)
}

/// Let the realm's users hold attributes its user profile does not
/// declare, as the consumer realm does.
async fn allow_unmanaged_attributes(kc: &KeycloakAdminClient, realm: &str) {
    let path = format!("/admin/realms/{realm}/users/profile");
    let (status, _, mut profile) = admin(kc, reqwest::Method::GET, &path, None).await;
    assert_eq!(
        status, 200,
        "the user profile of {realm} is read: {profile}"
    );
    profile["unmanagedAttributePolicy"] = serde_json::json!("ENABLED");
    let (status, _, body) = admin(kc, reqwest::Method::PUT, &path, Some(profile)).await;
    assert_eq!(
        status, 200,
        "the user profile of {realm} is written: {body}"
    );
}

/// Make a user in `realm` and return its id.
async fn make_user(
    kc: &KeycloakAdminClient,
    realm: &str,
    email: &str,
    enabled: bool,
    attributes: serde_json::Value,
) -> String {
    let (status, location, body) = admin(
        kc,
        reqwest::Method::POST,
        &format!("/admin/realms/{realm}/users"),
        Some(serde_json::json!({
            "username": email,
            "email": email,
            "firstName": "Ada",
            "lastName": "Lovelace",
            "emailVerified": true,
            "enabled": enabled,
            "attributes": attributes,
        })),
    )
    .await;
    assert_eq!(status, 201, "the user {email} is made: {body}");
    location
        .and_then(|l| l.rsplit('/').next().map(ToOwned::to_owned))
        .expect("Keycloak names the new user")
}

async fn read_user(kc: &KeycloakAdminClient, realm: &str, id: &str) -> serde_json::Value {
    let (status, _, user) = admin(
        kc,
        reqwest::Method::GET,
        &format!("/admin/realms/{realm}/users/{id}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "the user is read back: {user}");
    user
}

/// A realm for user writes: made by the client under test, with its users
/// allowed to hold the platform's attributes.
async fn realm_for_users(kc: &KeycloakAdminClient) -> String {
    let realm = unique("live-users");
    kc.create_realm(&realm).await.expect("the realm is made");
    allow_unmanaged_attributes(kc, &realm).await;
    realm
}

/// Keycloak accepts every setting a new tenant or team realm is created
/// with, and holds each one as it was sent, the password policy included.
#[tokio::test]
async fn keycloak_holds_every_protection_a_new_realm_is_created_with() {
    let test = "keycloak_holds_every_protection_a_new_realm_is_created_with";
    let Some(kc) = keycloak(test) else {
        return;
    };
    let tenant_realm = unique("live-tenant");
    kc.create_realm(&tenant_realm)
        .await
        .unwrap_or_else(|e| panic!("Keycloak refused the realm {tenant_realm}: {e}"));
    let team = unique("live");
    kc.create_team_realm(&team)
        .await
        .unwrap_or_else(|e| panic!("Keycloak refused the team realm team-{team}: {e}"));

    for realm in [tenant_realm, format!("team-{team}")] {
        let (status, _, stored) = admin(
            &kc,
            reqwest::Method::GET,
            &format!("/admin/realms/{realm}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "the realm {realm} is read back: {stored}");
        let sent = protected_realm_representation(&realm);
        let differing: Vec<String> = sent
            .as_object()
            .expect("the representation is an object")
            .iter()
            .filter(|(key, value)| stored.get(key.as_str()) != Some(value))
            .map(|(key, value)| format!("{key} (sent {value}, held {})", stored[key.as_str()]))
            .collect();
        assert!(
            differing.is_empty(),
            "Keycloak holds these settings of {realm} differently from what it was created with: \
             {differing:?}"
        );
        assert_eq!(
            stored["passwordPolicy"],
            serde_json::json!("length(12) and maxLength(128) and notUsername and notEmail"),
            "the password policy of {realm}"
        );
    }
}

/// A write of an attribute through the client leaves a user an
/// administrator disabled disabled, keeps the user's other attributes and
/// fields, and sets the attribute.
#[tokio::test]
async fn keycloak_keeps_a_disabled_user_disabled_through_an_attribute_write() {
    let test = "keycloak_keeps_a_disabled_user_disabled_through_an_attribute_write";
    let Some(kc) = keycloak(test) else {
        return;
    };
    let realm = realm_for_users(&kc).await;
    let id = make_user(
        &kc,
        &realm,
        "disabled@example.com",
        false,
        serde_json::json!({"tenant_id": ["u-0123456789abcdef0123456789abcdef"]}),
    )
    .await;

    kc.set_user_attribute(&realm, &id, "zaru_tier", "pro")
        .await
        .unwrap_or_else(|e| panic!("Keycloak refused the attribute write: {e}"));

    let user = read_user(&kc, &realm, &id).await;
    assert_eq!(
        user["enabled"],
        serde_json::json!(false),
        "an attribute write enabled a disabled user in Keycloak: {user}"
    );
    assert_eq!(user["attributes"]["zaru_tier"], serde_json::json!(["pro"]));
    assert_eq!(
        user["attributes"]["tenant_id"],
        serde_json::json!(["u-0123456789abcdef0123456789abcdef"])
    );
    assert_eq!(user["email"], serde_json::json!("disabled@example.com"));
    assert_eq!(user["firstName"], serde_json::json!("Ada"));
}

/// Writes of different attributes to one user, made at the same moment
/// through the client, all land in Keycloak.
#[tokio::test]
async fn keycloak_keeps_both_of_two_writes_to_one_user_at_once() {
    let test = "keycloak_keeps_both_of_two_writes_to_one_user_at_once";
    let Some(kc) = keycloak(test) else {
        return;
    };
    let kc = std::sync::Arc::new(kc);
    let realm = realm_for_users(&kc).await;
    let id = make_user(
        &kc,
        &realm,
        "busy@example.com",
        true,
        serde_json::json!({"tenant_id": ["u-0123456789abcdef0123456789abcdef"]}),
    )
    .await;

    for round in 0..8 {
        let tier = if round % 2 == 0 { "business" } else { "pro" };
        let tenants = vec![format!("t-{round:08}-0000-0000-0000-000000000000")];
        let (a, b) = tokio::join!(
            kc.set_user_attribute(&realm, &id, "zaru_tier", tier),
            kc.set_user_team_memberships(&realm, &id, &tenants),
        );
        a.unwrap_or_else(|e| panic!("round {round}: the tier write failed: {e}"));
        b.unwrap_or_else(|e| panic!("round {round}: the memberships write failed: {e}"));
        let attributes = read_user(&kc, &realm, &id).await["attributes"].clone();
        assert!(
            attributes["zaru_tier"] == serde_json::json!([tier])
                && attributes["team_memberships"] == serde_json::json!(tenants)
                && attributes["tenant_id"]
                    == serde_json::json!(["u-0123456789abcdef0123456789abcdef"]),
            "round {round}: two writes to one user at once lost one in Keycloak: {attributes}"
        );
    }
}

/// An invitation to a person who already has a user in the consumer realm
/// leaves that user's attributes and fields as they were, and puts the
/// user in the team's group.
#[tokio::test]
async fn keycloak_keeps_an_existing_users_attributes_through_an_invitation() {
    let test = "keycloak_keeps_an_existing_users_attributes_through_an_invitation";
    let Some(kc) = keycloak(test) else {
        return;
    };
    // A Business team's invitations go to the consumer realm by name.
    kc.create_realm("zaru-consumer")
        .await
        .expect("the consumer realm is there");
    allow_unmanaged_attributes(&kc, "zaru-consumer").await;
    let email = format!("{}@example.com", unique("invitee"));
    let attributes = serde_json::json!({
        "tenant_id": ["u-0123456789abcdef0123456789abcdef"],
        "zaru_tier": ["business"],
        "team_memberships": ["t-11111111-2222-3333-4444-555555555555"],
    });
    let id = make_user(&kc, "zaru-consumer", &email, true, attributes.clone()).await;
    let before = read_user(&kc, "zaru-consumer", &id).await;

    let team = unique("live-team");
    let invited = kc
        .invite_team_user(TenantTier::Business, &team, &email)
        .await
        .unwrap_or_else(|e| panic!("Keycloak refused the invitation: {e}"));
    assert_eq!(invited, id, "the invitation did not use the existing user");

    let after = read_user(&kc, "zaru-consumer", &id).await;
    assert_eq!(
        after["attributes"], attributes,
        "inviting an existing user changed its attributes in Keycloak"
    );
    for field in ["email", "firstName", "lastName", "enabled"] {
        assert_eq!(
            after[field], before[field],
            "the invitation changed {field}"
        );
    }
    let (status, _, groups) = admin(
        &kc,
        reqwest::Method::GET,
        &format!("/admin/realms/zaru-consumer/users/{id}/groups"),
        None,
    )
    .await;
    assert_eq!(status, 200, "the user's groups are read: {groups}");
    assert!(
        groups
            .as_array()
            .is_some_and(|g| g.iter().any(|g| g["name"] == serde_json::json!(team))),
        "the invited user is not in the team's group: {groups}"
    );
}

/// A team's SAML provider is stored with signature validation on and the
/// provider's email not trusted, and a provider with no signing certificate
/// is refused before anything is stored.
#[tokio::test]
async fn keycloak_stores_a_saml_provider_with_its_checks_on() {
    let test = "keycloak_stores_a_saml_provider_with_its_checks_on";
    let Some(kc) = keycloak(test) else {
        return;
    };
    let providers = |realm: String| {
        let kc = &kc;
        async move {
            let (status, _, instances) = admin(
                kc,
                reqwest::Method::GET,
                &format!("/admin/realms/{realm}/identity-provider/instances"),
                None,
            )
            .await;
            assert_eq!(status, 200, "the providers are read: {instances}");
            instances.as_array().cloned().unwrap_or_default()
        }
    };

    let realm = unique("live-saml");
    kc.create_realm(&realm).await.expect("the realm is made");
    kc.set_idp_config(&realm, &saml_config(TEST_IDP_CERTIFICATE_PEM))
        .await
        .unwrap_or_else(|e| panic!("Keycloak refused the SAML provider: {e}"));
    let stored = providers(realm.clone()).await;
    let saml: Vec<&serde_json::Value> = stored
        .iter()
        .filter(|p| p["providerId"] == serde_json::json!("saml"))
        .collect();
    assert_eq!(saml.len(), 1, "one SAML provider is stored: {stored:?}");
    assert!(
        weakened_checks(saml[0]).is_empty(),
        "Keycloak stored the SAML provider with a check off: {:?}",
        weakened_checks(saml[0])
    );
    assert!(
        saml[0]["config"]["signingCertificate"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "the signing certificate is not stored: {}",
        saml[0]
    );

    let bare = unique("live-saml");
    kc.create_realm(&bare).await.expect("the realm is made");
    let refused = kc.set_idp_config(&bare, &saml_config("")).await;
    assert!(
        matches!(refused, Err(KeycloakAdminError::InvalidIdpConfig(_))),
        "a SAML provider with no signing certificate was not refused: {refused:?}"
    );
    assert!(
        providers(bare).await.is_empty(),
        "a refused SAML provider was stored"
    );
}
