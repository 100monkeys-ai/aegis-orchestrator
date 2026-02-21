// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm, TokenData};
use crate::infrastructure::smcp::envelope::ContextClaims;

/// Token verifier abstracting cryptographic setup for JWT tokens (OpenBao integration placeholder)
pub struct ContextTokenVerifier {
    decoding_key: DecodingKey,
}

impl ContextTokenVerifier {
    pub fn new(pem: &str) -> Result<Self> {
        let decoding_key = DecodingKey::from_rsa_pem(pem.as_bytes())?;
        Ok(Self { decoding_key })
    }

    pub fn verify(&self, token_str: &str) -> Result<TokenData<ContextClaims>> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["exp"]);
        
        let token_data = decode::<ContextClaims>(
            token_str,
            &self.decoding_key,
            &validation
        )?;
        Ok(token_data)
    }
}

