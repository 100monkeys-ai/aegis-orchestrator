// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm, TokenData};
use crate::infrastructure::smcp::envelope::ContextClaims;

/// Token verifier abstracting cryptographic setup for JWT tokens (OpenBao integration placeholder)
pub struct SecurityTokenVerifier {
    decoding_key: DecodingKey,
    expected_issuer: String,
    expected_audiences: Vec<String>,
}

impl SecurityTokenVerifier {
    pub fn new(pem: &str, expected_issuer: &str, expected_audiences: &[&str]) -> Result<Self> {
        if expected_issuer.is_empty() {
            return Err(anyhow::anyhow!("expected_issuer must not be empty"));
        }
        if expected_audiences.is_empty() {
            return Err(anyhow::anyhow!("expected_audiences must not be empty"));
        }
        let decoding_key = DecodingKey::from_rsa_pem(pem.as_bytes())?;
        Ok(Self {
            decoding_key,
            expected_issuer: expected_issuer.to_string(),
            expected_audiences: expected_audiences.iter().map(|s| s.to_string()).collect(),
        })
    }

    pub fn verify(&self, token_str: &str) -> Result<TokenData<ContextClaims>> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.set_issuer(&[&self.expected_issuer]);
        validation.set_audience(&self.expected_audiences.iter().map(String::as_str).collect::<Vec<_>>());

        let token_data = decode::<ContextClaims>(
            token_str,
            &self.decoding_key,
            &validation
        )?;
        Ok(token_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use crate::infrastructure::smcp::envelope::AudienceClaim;

    // Minimal 2048-bit RSA key pair for testing only – never use in production.
    const TEST_RSA_PRIVATE_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAmWtpvUNARl+B9DenjbtDMcwfwkX4k7xYgkbLBJ7ON2VUPEfx\nHfOe50KqxX6AJzvHIaEWyOPM/J4YYIzO12nNzjKRElPSp5PDDigKYJePhxPl1bQn\nrY2A/L1GaVWx2rDjZqtldjJiuOI6CdsDT+GF+Twd1O4H2OMhYk6iATQqGzJQxKnd\nHEMdQqFa2NhDpuyEl9xhcUUVUboQR0+a8hfdoNTqhedK2ImTQ0JDFwt5e1c/XCLT\nj5PWfKJeHxqBYrt2hPgo8fjE0S6BX2fCOqUQ//4kPyI0ik5AZAOZ0o2RSEZn0Gei\nW3HiUl0kIMDuIMD12AMjzN5ePcHcl39zq96syQIDAQABAoIBAAEnNkNJUYPRDSzj\n6N6BEZeAp5WrVdIEhQLiR0dJXqhJ/4qD+CkWzpr2J0Lv6qmXIqYaLub+UzqqJBgp\nFdGIsFyK9T6egbTnilWcitSEXqM0zMdltix03/PQE4y+5bo/FkAvT3EEe5Kx4o8/\n64SDhqjwM3e/eRGRAJQVzOuiAIB5oy2JdDxa0JZXHU8ilKahu2GjpBAGajLD5T17\nZjHKsIfLJAQSqfxfCMnBIhqLVlUuWDoEIoBKv6bGHC7D6ElxvZRpb9JFuuigs/l5\n8rg+R7bv+7Uz9P0FVyyLFRt5puQJa1SuwgHhfK0KDnssWbeJhVXvmeSa3Z2cl0Wp\nbWT/XgECgYEA0iCyFhn3hnLlXBJHZGlTm/6qJpcSX9fIoLKMm1/GEXHJqSqyhWdE\nC7vJOkySHbNQ36sxxI+P2DteaEZMMwimzNFmw7Em1g334eTmXAhr/1qrFWzjysTN\nJWlsDfh7uDg/RO52P0kK723uvIrh82lf5Dva3wt99TH/R3TzLKXNbEsCgYEAuul/\nbE4glHKI9v4OZowrhBMnNCjpHMzS0aMLKpsu07ZVPn1HKnqxtt4IioiHQ9O0UcV6\nbXSYLhf42VxJYZ4xQ7uDGeB0Z84Pkd+d1S7ughV7QgweaIHmfAQAg+iSolOlcvyz\nM58zShVXiSaqzNp75Ai1tjkbuo/HWgLwvIDydrsCgYEAkwQXNYlzepkWykVrt+BN\nhD44lAls7KvQDkb+Q5NNxFTFkFt0TgwDOuZnEygRr0APnH5tsqXzMYnQMsrEc4xh\nD7qO2OowTuG1BlKdrdSioyWvv6zQ78Sj98H7vQaWoTyRX8wr5XlYck6LE1VkY2bd\nlZUfPKEQvqX9guRbY2iaAmMCgYA5Ptpv6V3BGXMpcpYmgjexs8wGBaGf2HuZCT6a\nRf0JioaBJQ1uzTUwtMAY7ce/1k8b3EeqzlLtixoEOGehJjogbIWynzQHtuy92KcW\na9FQthOSHvQRPffBc9hUjh6a6NN7bDnWTaP/xJmSv+z/4MqhBKnirYr4kKCVyODC\nWxvnkQKBgQDAL4bBoWRBtJJHLmMMgweY421W497kl4BvAiur36WT99fknp5ktqRU\nPxTp4+a+lU1gc393kfJvUeIVYX1vJs0tS+YkNVpCrC5hBmVaemd5Vav1q13+/sZ/\ncpc0iRy0EDCDXsAbf/guJdqShW1x1cB1moHFiM+8FsM80SsAZavjnQ==\n-----END RSA PRIVATE KEY-----";

    const TEST_RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAmWtpvUNARl+B9DenjbtD\nMcwfwkX4k7xYgkbLBJ7ON2VUPEfxHfOe50KqxX6AJzvHIaEWyOPM/J4YYIzO12nN\nzjKRElPSp5PDDigKYJePhxPl1bQnrY2A/L1GaVWx2rDjZqtldjJiuOI6CdsDT+GF\n+Twd1O4H2OMhYk6iATQqGzJQxKndHEMdQqFa2NhDpuyEl9xhcUUVUboQR0+a8hfd\noNTqhedK2ImTQ0JDFwt5e1c/XCLTj5PWfKJeHxqBYrt2hPgo8fjE0S6BX2fCOqUQ\n//4kPyI0ik5AZAOZ0o2RSEZn0GeiW3HiUl0kIMDuIMD12AMjzN5ePcHcl39zq96s\nyQIDAQAB\n-----END PUBLIC KEY-----";

    fn make_claims(iss: Option<&str>, aud: Option<Vec<String>>, exp_offset: i64) -> ContextClaims {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        ContextClaims {
            agent_id: "agent-1".to_string(),
            execution_id: "exec-1".to_string(),
            security_context: "test".to_string(),
            iss: iss.map(|s| s.to_string()),
            aud: aud.map(AudienceClaim::Multiple),
            exp: Some(now + exp_offset),
            iat: Some(now),
            nbf: None,
        }
    }

    fn make_claims_single_aud(iss: Option<&str>, aud: Option<&str>, exp_offset: i64) -> ContextClaims {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        ContextClaims {
            agent_id: "agent-1".to_string(),
            execution_id: "exec-1".to_string(),
            security_context: "test".to_string(),
            iss: iss.map(|s| s.to_string()),
            aud: aud.map(|s| AudienceClaim::Single(s.to_string())),
            exp: Some(now + exp_offset),
            iat: Some(now),
            nbf: None,
        }
    }

    fn sign_claims(claims: &ContextClaims) -> String {
        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).unwrap();
        encode(&Header::new(Algorithm::RS256), claims, &encoding_key).unwrap()
    }

    #[test]
    fn test_verify_valid_token_with_iss_and_aud() {
        let claims = make_claims(Some("aegis-orchestrator"), Some(vec!["aegis-agents".to_string()]), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_ok());
    }

    #[test]
    fn test_verify_rejects_wrong_issuer() {
        let claims = make_claims(Some("untrusted-issuer"), Some(vec!["aegis-agents".to_string()]), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_err());
    }

    #[test]
    fn test_verify_rejects_wrong_audience() {
        let claims = make_claims(Some("aegis-orchestrator"), Some(vec!["wrong-audience".to_string()]), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_err());
    }

    #[test]
    fn test_verify_rejects_missing_iss() {
        let claims = make_claims(None, Some(vec!["aegis-agents".to_string()]), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_err());
    }

    #[test]
    fn test_verify_rejects_missing_aud() {
        let claims = make_claims(Some("aegis-orchestrator"), None, 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_err());
    }

    #[test]
    fn test_verify_accepts_token_with_one_of_multiple_audiences() {
        let claims = make_claims(Some("aegis-orchestrator"), Some(vec!["aegis-agents".to_string()]), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["other-service", "aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_ok());
    }

    #[test]
    fn test_verify_rejects_expired_token() {
        let claims = make_claims(Some("aegis-orchestrator"), Some(vec!["aegis-agents".to_string()]), -3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_err());
    }

    #[test]
    fn test_verify_accepts_string_aud() {
        // Token where aud is a single string (not an array) — both forms are valid per the JWT spec.
        let claims = make_claims_single_aud(Some("aegis-orchestrator"), Some("aegis-agents"), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_ok());
    }

    #[test]
    fn test_verify_rejects_wrong_string_aud() {
        let claims = make_claims_single_aud(Some("aegis-orchestrator"), Some("untrusted-audience"), 3600);
        let token = sign_claims(&claims);
        let verifier = SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &["aegis-agents"]).unwrap();
        assert!(verifier.verify(&token).is_err());
    }

    #[test]
    fn test_new_rejects_empty_issuer() {
        assert!(SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "", &["aegis-agents"]).is_err());
    }

    #[test]
    fn test_new_rejects_empty_audiences() {
        assert!(SecurityTokenVerifier::new(TEST_RSA_PUBLIC_PEM, "aegis-orchestrator", &[]).is_err());
    }
}

