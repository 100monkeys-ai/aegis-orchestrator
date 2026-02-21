// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use tracing::{info, warn};
use serde_json::Value;

use crate::domain::smcp_session::{SmcpSession, EnvelopeVerifier, SmcpSessionError};

pub struct SmcpMiddleware;

impl SmcpMiddleware {
    pub fn new() -> Self {
        Self
    }

    pub fn verify_and_unwrap(
        &self, 
        session: &SmcpSession, 
        envelope: &impl EnvelopeVerifier
    ) -> Result<Value, SmcpSessionError> {
        info!("Verifying SMCP envelope for session {}", session.id);
        
        match session.evaluate_call(envelope) {
            Ok(()) => {
                info!("SMCP envelope verified successfully");
                if let Some(args) = envelope.extract_arguments() {
                    Ok(args)
                } else {
                    Err(SmcpSessionError::MalformedPayload)
                }
            }
            Err(e) => {
                warn!("SMCP envelope verification failed: {}", e);
                Err(e)
            }
        }
    }
}
