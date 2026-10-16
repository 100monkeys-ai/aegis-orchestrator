// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Zaru ADR-0071 D1 (c): the session records the credential that attested it
//!
//! `/v1/seal/attest` attests with the person's own session token (Zaru
//! Web's turn) or with an API key carrying the profile chosen for it. The
//! attestation service records which on the session it saves, so the
//! orchestrator, not a header, tells the person's own chat turn from a key.
//! A request that names none (the gRPC path) is a key with no profile,
//! which reaches no connection.

use std::sync::Arc;

use aegis_orchestrator_core::application::attestation_service::AttestationServiceImpl;
use aegis_orchestrator_core::application::ports::{
    AttestationTokenClaims, SecurityTokenIssuerPort,
};
use aegis_orchestrator_core::domain::iam::RealmKind;
use aegis_orchestrator_core::domain::seal_session::{AttestingCredential, SealSession};
use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use aegis_orchestrator_core::domain::security_context::capability::Capability;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::seal::attestation::{
    AttestationRequest, AttestationService,
};
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::SigningKey;

/// Issues a distinct opaque token per session; the token's form is not
/// what these tests are about.
struct Tokens;

impl SecurityTokenIssuerPort for Tokens {
    fn issue(&self, _claims: &mut AttestationTokenClaims) -> anyhow::Result<String> {
        Ok(format!("token-{}", uuid::Uuid::new_v4()))
    }
}

fn zaru_pro() -> SecurityContext {
    SecurityContext {
        name: "zaru-pro".to_string(),
        description: "test context".to_string(),
        capabilities: vec![Capability {
            tool_pattern: "*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// The session the service saves for a person's attestation of `zaru-pro`
/// by `attested_by`.
async fn attested(attested_by: AttestingCredential) -> SealSession {
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(zaru_pro()).await.unwrap();
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let issuer: Arc<dyn SecurityTokenIssuerPort> = Arc::new(Tokens);
    let service = AttestationServiceImpl::new(contexts, sessions.clone(), issuer);
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let response = service
        .attest(AttestationRequest {
            agent_id: None,
            execution_id: None,
            container_id: None,
            public_key_pem: STANDARD.encode(key.verifying_key().as_bytes()),
            security_context: Some("zaru-pro".to_string()),
            principal_subject: Some("person-sub".to_string()),
            user_id: Some("person-sub".to_string()),
            workload_id: None,
            zaru_tier: Some("pro".to_string()),
            tenant_id: TenantId::consumer(),
            realm: RealmKind::Consumer,
            task_summary: None,
            attested_by,
        })
        .await
        .expect("the attestation was refused");
    sessions
        .find_active_by_security_token(&response.security_token)
        .await
        .unwrap()
        .expect("no session was saved")
}

#[tokio::test]
async fn a_session_attested_by_the_persons_own_token_records_interactive() {
    let session = attested(AttestingCredential::Interactive).await;
    assert_eq!(session.attested_by, AttestingCredential::Interactive);
}

#[tokio::test]
async fn a_session_attested_by_a_key_records_the_key_and_its_profile() {
    let profile = uuid::Uuid::new_v4();
    let session = attested(AttestingCredential::ApiKey {
        profile: Some(profile),
    })
    .await;
    assert_eq!(
        session.attested_by,
        AttestingCredential::ApiKey {
            profile: Some(profile)
        }
    );
}

#[tokio::test]
async fn a_session_attested_by_a_key_with_no_profile_records_none() {
    let session = attested(AttestingCredential::ApiKey { profile: None }).await;
    assert_eq!(
        session.attested_by,
        AttestingCredential::ApiKey { profile: None }
    );
}
