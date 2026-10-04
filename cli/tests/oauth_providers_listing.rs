// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The OAuth providers listing (AEGIS ADR-125, Update of 2026-10-04,
//! clause 2): from a node configuration's `spec.oauth_providers`, through
//! the registry the daemon builds, to the body `GET
//! /v1/credentials/oauth/providers` answers. Test (f): each registry entry
//! by its `provider` and display name, and never a client id or secret
//! (an absence test on the serialised body). The route itself is driven
//! through the daemon's authentication stack by the handler's own tests.

use aegis_orchestrator_core::application::credential_service::{
    oauth_provider_listing, oauth_provider_registry_from_config, OAuthProviderListing,
};
use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;

const CLIENT_ID: &str = "Mk7-listing-test-client-id";
const CLIENT_SECRET: &str = "Mk7-listing-test-client-secret";

const BLOCK: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: NodeConfig
metadata:
  name: listing-test
spec:
  node:
    id: "node-1"
    type: orchestrator
  oauth_providers:
    - provider: google
      display_name: "  Google  "
      authorization_url: "https://accounts.example/o/oauth2/v2/auth"
      token_url: "https://oauth2.example/token"
      client_id: "env:LISTING_TEST_CLIENT_ID"
      client_secret: "env:LISTING_TEST_CLIENT_SECRET"
      redirect_uri_allowlist:
        - "https://ask.example/vault/connections/callback"
      scopes:
        - "https://www.googleapis.com/auth/gmail.modify"
    - provider: acme-chat
      display_name: ""
      authorization_url: "https://chat.example/authorize"
      token_url: "https://chat.example/token"
      client_id: "plain-chat-client"
      redirect_uri_allowlist:
        - "https://ask.example/vault/connections/callback"
"#;

#[test]
fn the_listing_holds_each_provider_and_display_name_and_never_a_client_credential() {
    std::env::set_var("LISTING_TEST_CLIENT_ID", CLIENT_ID);
    std::env::set_var("LISTING_TEST_CLIENT_SECRET", CLIENT_SECRET);
    let entries = NodeConfigManifest::from_yaml_str(BLOCK)
        .expect("the block parses")
        .spec
        .oauth_providers;
    let registry = oauth_provider_registry_from_config(&entries).expect("registry builds");

    let listing = oauth_provider_listing(&registry);
    assert_eq!(
        listing,
        vec![
            OAuthProviderListing {
                provider: "acme-chat".to_string(),
                display_name: "acme-chat".to_string(),
            },
            OAuthProviderListing {
                provider: "google".to_string(),
                display_name: "Google".to_string(),
            },
        ],
        "sorted by provider; an empty display name shows the provider"
    );

    let body = serde_json::to_string(&listing).unwrap();
    for absent in [
        CLIENT_ID,
        CLIENT_SECRET,
        "plain-chat-client",
        "client_id",
        "client_secret",
        "token_url",
        "authorization_url",
        "gmail.modify",
    ] {
        assert!(!body.contains(absent), "the listing holds {absent}: {body}");
    }
}

#[test]
fn an_empty_registry_lists_nothing() {
    let registry = oauth_provider_registry_from_config(&[]).expect("an empty registry builds");
    assert!(oauth_provider_listing(&registry).is_empty());
}
