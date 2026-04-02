// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Envelope and JWT Claims (ADR-035 §3)
//!
//! Infrastructure implementation of the SEAL envelope format. An `SealEnvelope`
//! is the outer wrapper that every MCP message from an agent must be wrapped in
//! before the orchestrator will forward it to a tool server.
//!
//! ## Envelope Structure
//!
//! ```text
//! SealEnvelope {
//!     security_token: JWT (ContextClaims, signed by orchestrator)
//!     signature:      base64(Ed25519_sign(inner_mcp, agent_private_key))
//!     inner_mcp:      raw MCP JSON-RPC payload bytes
//! }
//! ```
//!
//! ## Verification
//!
//! `SealEnvelope` implements [`crate::domain::seal_session::EnvelopeVerifier`]:
//! - `verify_signature` decodes the signature and verifies it against `inner_mcp`
//!   using the Ed25519 public key stored in the `SealSession`.
//! - `extract_tool_name` / `extract_arguments` parse the `inner_mcp` JSON-RPC payload.
//!
//! See ADR-035 §3 (Protocol Wire Format).
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::seal_session::{EnvelopeVerifier, SealSessionError};

/// The outer SEAL wrapper applied to every MCP message from an agent container.
///
/// An immutable value object: once constructed the fields must not be modified
/// (the signature covers `inner_mcp` and would be invalidated by any change).
///
/// # Security
///
/// The envelope achieves **non-repudiation**: the agent cannot deny having sent
/// a particular tool call because the `signature` is an Ed25519 signature over
/// `inner_mcp` with a key that was bound during attestation. See ADR-035 §5.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealEnvelope {
    /// Protocol identifier for version negotiation. `None` is accepted only for
    /// internal legacy callers that still send the pre-spec envelope shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Signed JWT (`SecurityToken`) issued during attestation. Encodes `ContextClaims`.
    pub security_token: String,
    /// Base64-encoded Ed25519 signature over `inner_mcp` bytes.
    pub signature: String,
    /// Raw MCP JSON-RPC payload bytes (method + params).
    pub inner_mcp: Vec<u8>,
    /// ISO-8601 UTC timestamp used for replay protection and canonical signing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// Represents the JWT `aud` claim, which may be either a single string or an array of strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AudienceClaim {
    Single(String),
    Multiple(Vec<String>),
}

/// JWT claims payload used in SEAL `SecurityToken`s.
///
/// The `ContextClaims` are signed by the orchestrator during attestation and
/// embedded in every `SealEnvelope.security_token`. They bind the token to a
/// specific agent execution and `SecurityContext`.
///
/// Standard JWT fields (`iss`, `aud`, `exp`, `iat`, `nbf`) follow RFC 7519.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextClaims {
    /// AEGIS `AgentId` (UUID string) — identifies the agent that was attested.
    pub agent_id: String,
    /// AEGIS `ExecutionId` (UUID string) — binds the token to a single execution.
    pub execution_id: String,
    /// Name of the `SecurityContext` assigned to this execution (e.g. `"research-safe"`).
    pub security_context: String,
    /// Issuer of the token (e.g. the orchestrator instance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// Intended audience(s).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<AudienceClaim>,
    /// Expiration time (Unix epoch seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    /// Issued-at time (Unix epoch seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    /// Not-before time (Unix epoch seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
    /// JWT ID — unique nonce per token for replay detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// Security context name (spec alias for `security_context`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scp: Option<String>,
    /// Workload/container ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wid: Option<String>,
    /// Execution correlation identifier (spec alias for `execution_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_id: Option<String>,
    /// Tenant routing identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

impl EnvelopeVerifier for SealEnvelope {
    fn security_token(&self) -> &str {
        &self.security_token
    }

    fn verify_signature(&self, public_key_bytes: &[u8]) -> Result<(), SealSessionError> {
        let public_key_bytes: [u8; 32] = public_key_bytes.try_into().map_err(|_| {
            SealSessionError::SignatureVerificationFailed(
                "Invalid public key length (must be 32 bytes)".to_string(),
            )
        })?;

        let verifying_key = VerifyingKey::from_bytes(&public_key_bytes).map_err(|e| {
            SealSessionError::SignatureVerificationFailed(format!("Invalid public key: {e}"))
        })?;

        let decoded_sig = STANDARD.decode(&self.signature).map_err(|e| {
            SealSessionError::SignatureVerificationFailed(format!("Invalid base64 signature: {e}"))
        })?;

        let sig_bytes: [u8; 64] = decoded_sig.try_into().map_err(|_| {
            SealSessionError::SignatureVerificationFailed(
                "Invalid signature length (must be 64 bytes)".to_string(),
            )
        })?;

        let signature = Signature::from_bytes(&sig_bytes);
        let signed_message = self.signed_message()?;

        verifying_key
            .verify(&signed_message, &signature)
            .map_err(|e| {
                SealSessionError::SignatureVerificationFailed(format!(
                    "Signature verification failed: {e}"
                ))
            })
    }

    fn extract_tool_name(&self) -> Option<String> {
        let payload: Value = serde_json::from_slice(&self.inner_mcp).ok()?;

        // standard MCP tool call is `{ "method": "tools/call", "params": { "name": "..." } }`
        let method = payload.get("method")?.as_str()?;
        if method == "tools/call" {
            payload
                .get("params")?
                .get("name")?
                .as_str()
                .map(|s| s.to_string())
        } else {
            // For older specs or simpler variants
            Some(method.to_string())
        }
    }

    fn extract_arguments(&self) -> Option<Value> {
        let payload: Value = serde_json::from_slice(&self.inner_mcp).ok()?;

        let method = payload.get("method")?.as_str()?;
        if method == "tools/call" {
            payload.get("params")?.get("arguments").cloned()
        } else {
            payload.get("params").cloned()
        }
    }
}

impl SealEnvelope {
    fn signed_message(&self) -> Result<Vec<u8>, SealSessionError> {
        match (&self.protocol, &self.timestamp) {
            (Some(protocol), Some(timestamp)) => {
                if protocol != "seal/v1" {
                    return Err(SealSessionError::MalformedPayload(format!(
                        "unsupported SEAL protocol '{protocol}'"
                    )));
                }

                let timestamp = parse_iso8601_timestamp(timestamp)?;
                let age_seconds = (Utc::now() - timestamp).num_seconds().abs();
                if age_seconds > 30 {
                    return Err(SealSessionError::ReplayProtectionFailed(format!(
                        "envelope timestamp is outside the 30 second freshness window ({age_seconds}s)"
                    )));
                }

                canonical_message(&self.security_token, &self.inner_mcp, timestamp)
            }
            (None, None) => Ok(self.inner_mcp.clone()),
            _ => Err(SealSessionError::MalformedPayload(
                "SEAL envelopes must provide both protocol and timestamp together".to_string(),
            )),
        }
    }
}

/// Normalize an attested Ed25519 public key into the raw 32-byte form stored in SealSession.
///
/// Accepts:
/// - raw 32-byte key material already present as bytes
/// - base64-encoded raw 32-byte key material
/// - PEM/SPKI public keys
/// - base64-encoded Ed25519 SPKI DER public keys
pub fn normalize_public_key_bytes(input: &str) -> Result<Vec<u8>, SealSessionError> {
    const ED25519_SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];

    let trimmed = input.trim();
    let candidate = if trimmed.contains("BEGIN PUBLIC KEY") {
        trimmed
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>()
    } else {
        trimmed.to_string()
    };

    let decoded = STANDARD
        .decode(&candidate)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&candidate))
        .unwrap_or_else(|_| candidate.as_bytes().to_vec());

    if decoded.len() == 32 {
        return Ok(decoded);
    }

    if decoded.len() == 44 && decoded.starts_with(&ED25519_SPKI_PREFIX) {
        return Ok(decoded[ED25519_SPKI_PREFIX.len()..].to_vec());
    }

    Err(SealSessionError::SignatureVerificationFailed(
        "unsupported Ed25519 public key encoding".to_string(),
    ))
}

fn parse_iso8601_timestamp(input: &str) -> Result<DateTime<Utc>, SealSessionError> {
    DateTime::parse_from_rfc3339(input)
        .map(|ts| ts.with_timezone(&Utc))
        .map_err(|e| {
            SealSessionError::MalformedPayload(format!("invalid SEAL timestamp '{input}': {e}"))
        })
}

fn canonical_message(
    security_token: &str,
    payload: &[u8],
    timestamp: DateTime<Utc>,
) -> Result<Vec<u8>, SealSessionError> {
    let payload_json: Value = serde_json::from_slice(payload).map_err(|e| {
        SealSessionError::MalformedPayload(format!("invalid inner MCP payload JSON: {e}"))
    })?;
    let canonical = serde_json::json!({
        "payload": payload_json,
        "security_token": security_token,
        "timestamp": timestamp.timestamp(),
    });

    serde_json::to_vec(&canonical).map_err(|e| {
        SealSessionError::MalformedPayload(format!(
            "failed to serialize canonical SEAL message: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SecondsFormat;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    #[test]
    fn test_seal_envelope_verification() {
        // Deterministic test keypair (avoids rand_core version coupling in tests)
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let inner_mcp_json = json!({
            "method": "tools/call",
            "params": {
                "name": "fs.read",
                "arguments": {
                    "path": "/workspace/demo.txt"
                }
            }
        });

        let inner_mcp_bytes = serde_json::to_vec(&inner_mcp_json).unwrap();

        // Sign the MCP message
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = signing_key.sign(
            &canonical_message(
                "test-jwt.token.here",
                &inner_mcp_bytes,
                parse_iso8601_timestamp(&timestamp).unwrap(),
            )
            .unwrap(),
        );
        let signature_b64 = STANDARD.encode(signature.to_bytes());

        let envelope = SealEnvelope {
            protocol: Some("seal/v1".to_string()),
            security_token: "test-jwt.token.here".to_string(),
            signature: signature_b64,
            inner_mcp: inner_mcp_bytes.clone(),
            timestamp: Some(timestamp),
        };

        // Should verify successfully with the public key
        assert!(envelope.verify_signature(verifying_key.as_bytes()).is_ok());

        // Property extraction tests
        assert_eq!(envelope.extract_tool_name(), Some("fs.read".to_string()));
        let args = envelope.extract_arguments().unwrap();
        assert_eq!(
            args.get("path").unwrap().as_str().unwrap(),
            "/workspace/demo.txt"
        );
    }

    #[test]
    fn test_seal_envelope_verification_failure() {
        // Two deterministic but different keypairs
        let signing_key1 = SigningKey::from_bytes(&[2u8; 32]);
        let signing_key2 = SigningKey::from_bytes(&[3u8; 32]);
        let verifying_key2 = signing_key2.verifying_key();

        let inner_mcp_bytes = b"{\"method\": \"something\"}".to_vec();

        // Sign with key 1
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = signing_key1.sign(
            &canonical_message(
                "test.jwt",
                &inner_mcp_bytes,
                parse_iso8601_timestamp(&timestamp).unwrap(),
            )
            .unwrap(),
        );
        let signature_b64 = STANDARD.encode(signature.to_bytes());

        let envelope = SealEnvelope {
            protocol: Some("seal/v1".to_string()),
            security_token: "test.jwt".to_string(),
            signature: signature_b64,
            inner_mcp: inner_mcp_bytes,
            timestamp: Some(timestamp),
        };

        // Verification with key 2 should fail
        assert!(matches!(
            envelope.verify_signature(verifying_key2.as_bytes()),
            Err(SealSessionError::SignatureVerificationFailed(_))
        ));
    }

    #[test]
    fn normalize_public_key_accepts_spki_der_base64() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let mut spki = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        spki.extend_from_slice(verifying_key.as_bytes());
        let encoded = STANDARD.encode(spki);

        let normalized = normalize_public_key_bytes(&encoded).unwrap();
        assert_eq!(normalized, verifying_key.as_bytes());
    }
}
