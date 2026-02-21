// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::smcp_session::{EnvelopeVerifier, SmcpSessionError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmcpEnvelope {
    pub security_token: String,
    pub signature: String,
    pub inner_mcp: Vec<u8>,
}

/// Represents the JWT `aud` claim, which may be either a single string or an array of strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AudienceClaim {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextClaims {
    pub agent_id: String,
    pub execution_id: String,
    pub security_context: String,

    /// Issuer of the token (e.g. the orchestrator instance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,

    /// Intended audience(s) for the token.
    /// The JWT spec allows `aud` to be either a single string or an array of strings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<AudienceClaim>,

    /// Expiration time (as seconds since Unix epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,

    /// Issued-at time (as seconds since Unix epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,

    /// Not-before time (as seconds since Unix epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
}

impl EnvelopeVerifier for SmcpEnvelope {
    fn verify_signature(&self, public_key_bytes: &[u8]) -> Result<(), SmcpSessionError> {
        let public_key_bytes: [u8; 32] = public_key_bytes.try_into().map_err(|_| {
            SmcpSessionError::SignatureVerificationFailed("Invalid public key length (must be 32 bytes)".to_string())
        })?;
        
        let verifying_key = VerifyingKey::from_bytes(&public_key_bytes).map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Invalid public key: {}", e))
        })?;
        
        let decoded_sig = STANDARD.decode(&self.signature).map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Invalid base64 signature: {}", e))
        })?;
        
        let sig_bytes: [u8; 64] = decoded_sig.try_into().map_err(|_| {
            SmcpSessionError::SignatureVerificationFailed("Invalid signature length (must be 64 bytes)".to_string())
        })?;
        
        let signature = Signature::from_bytes(&sig_bytes);
        
        verifying_key.verify(&self.inner_mcp, &signature).map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Signature verification failed: {}", e))
        })
    }
    
    fn extract_tool_name(&self) -> Option<String> {
        let payload: Value = serde_json::from_slice(&self.inner_mcp).ok()?;
        
        // standard MCP tool call is `{ "method": "tools/call", "params": { "name": "..." } }`
        let method = payload.get("method")?.as_str()?;
        if method == "tools/call" {
            payload.get("params")?.get("name")?.as_str().map(|s| s.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use serde_json::json;

    #[test]
    fn test_smcp_envelope_verification() {
        // Generate a test keypair
        let mut csprng = OsRng;
        let signing_key: SigningKey = SigningKey::generate(&mut csprng);
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
        let signature = signing_key.sign(&inner_mcp_bytes);
        let signature_b64 = STANDARD.encode(signature.to_bytes());
        
        let envelope = SmcpEnvelope {
            security_token: "mock-jwt.token.here".to_string(),
            signature: signature_b64,
            inner_mcp: inner_mcp_bytes.clone(),
        };
        
        // Should verify successfully with the public key
        assert!(envelope.verify_signature(verifying_key.as_bytes()).is_ok());
        
        // Property extraction tests
        assert_eq!(envelope.extract_tool_name(), Some("fs.read".to_string()));
        let args = envelope.extract_arguments().unwrap();
        assert_eq!(args.get("path").unwrap().as_str().unwrap(), "/workspace/demo.txt");
    }

    #[test]
    fn test_smcp_envelope_verification_failure() {
        // Generate two different keypairs
        let mut csprng = OsRng;
        let signing_key1: SigningKey = SigningKey::generate(&mut csprng);
        
        let signing_key2: SigningKey = SigningKey::generate(&mut csprng);
        let verifying_key2 = signing_key2.verifying_key();
        
        let inner_mcp_bytes = b"{\"method\": \"something\"}".to_vec();
        
        // Sign with key 1
        let signature = signing_key1.sign(&inner_mcp_bytes);
        let signature_b64 = STANDARD.encode(signature.to_bytes());
        
        let envelope = SmcpEnvelope {
            security_token: "mock.jwt".to_string(),
            signature: signature_b64,
            inner_mcp: inner_mcp_bytes,
        };
        
        // Verification with key 2 should fail
        assert!(matches!(
            envelope.verify_signature(verifying_key2.as_bytes()),
            Err(SmcpSessionError::SignatureVerificationFailed(_))
        ));
    }
}
