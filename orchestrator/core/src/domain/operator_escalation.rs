// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Operator escalation (AEGIS ADR-129)
//!
//! An operator reaches the operator tool surface through MCP only by an
//! escalation: a six-digit, single-use [`OperatorEscalationCode`] minted by
//! the operator web interface's stepped-up session, given to the agent, and
//! redeemed by the operator's own consumer API key, which then holds an
//! [`OperatorEscalation`] for `ttl_seconds` (D5, D12).
//!
//! The code is stored as its SHA-256 only (D9); every act is appended to
//! `admin_audit_log` (D18) through the same [`OperatorEscalationRepository`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::iam::AegisRole;
use crate::domain::repository::RepositoryError;

/// The only client whose `aegis-system` token may mint a code: the operator
/// web interface's step-up client (ADR-129 D13; ADR-073 line 185).
pub const MINTING_CLIENT_ID: &str = "zaru-client-system";

/// The number of decimal digits in a code (ADR-129 D6).
pub const CODE_DIGITS: usize = 6;

/// `admin_audit_log.action` values (ADR-129 D18).
pub mod audit_action {
    pub const CODE_ISSUED: &str = "operator_escalation.code_issued";
    pub const REDEEMED: &str = "operator_escalation.redeemed";
    pub const REDEEM_FAILED: &str = "operator_escalation.redeem_failed";
    pub const ENDED: &str = "operator_escalation.ended";
    pub const TOOL_CALL: &str = "operator_escalation.tool_call";
}

/// Draw a code: six decimal digits, uniform, from the operating system's
/// cryptographically secure generator (ADR-129 D6). Rejection sampling keeps
/// the distribution uniform over `000000..=999999`.
pub fn generate_code() -> String {
    const SPACE: u32 = 1_000_000;
    // The largest multiple of SPACE that fits in a u32; draws at or above it
    // are discarded so that no residue is favoured.
    const LIMIT: u32 = u32::MAX - (u32::MAX % SPACE);
    loop {
        let draw = OsRng.next_u32();
        if draw < LIMIT {
            return format!("{:0width$}", draw % SPACE, width = CODE_DIGITS);
        }
    }
}

/// Whether `code` has the shape of a code: exactly six ASCII digits.
pub fn is_code_shaped(code: &str) -> bool {
    code.len() == CODE_DIGITS && code.bytes().all(|b| b.is_ascii_digit())
}

/// The stored form of a code: its SHA-256, hex (ADR-129 D9, as an API key is
/// stored).
pub fn hash_code(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code.as_bytes());
    hex::encode(hasher.finalize())
}

/// One minted code (ADR-129 D6 to D10). Never holds the code itself.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorEscalationCode {
    pub id: Uuid,
    pub code_hash: String,
    /// The consumer-realm `sub` of the operator who minted it (the system
    /// token's `consumer_sub` claim); only a key of this user redeems it.
    pub consumer_sub: String,
    /// The `aegis-system` realm `sub` of the minting token.
    pub system_sub: String,
    pub aegis_role: AegisRole,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
    pub failed_attempts: u32,
    pub invalidated_at: Option<DateTime<Utc>>,
}

impl OperatorEscalationCode {
    /// Unconsumed, not invalidated, and inside its validity (D7, D8).
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.consumed_at.is_none() && self.invalidated_at.is_none() && now < self.expires_at
    }
}

/// Why an escalation ended (ADR-129 D19; the Update's U5 and U6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationEndReason {
    /// Reached `expires_at`.
    Expired,
    /// Ended by the operator on the web page.
    OperatorWeb,
    /// Ended by the agent over MCP.
    AgentRelease,
    /// The holding API key was revoked.
    ApiKeyRevoked,
    /// The operator was demoted.
    OperatorDemoted,
}

impl EscalationEndReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::OperatorWeb => "operator_web",
            Self::AgentRelease => "agent_release",
            Self::ApiKeyRevoked => "api_key_revoked",
            Self::OperatorDemoted => "operator_demoted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "expired" => Some(Self::Expired),
            "operator_web" => Some(Self::OperatorWeb),
            "agent_release" => Some(Self::AgentRelease),
            "api_key_revoked" => Some(Self::ApiKeyRevoked),
            "operator_demoted" => Some(Self::OperatorDemoted),
            _ => None,
        }
    }
}

/// One escalation, held by one API key (ADR-129 D10, D14).
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorEscalation {
    pub id: Uuid,
    pub api_key_id: Uuid,
    pub consumer_sub: String,
    pub system_sub: String,
    pub aegis_role: AegisRole,
    pub code_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<EscalationEndReason>,
}

impl OperatorEscalation {
    /// Not ended and before `expires_at` (D19).
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.ended_at.is_none() && now < self.expires_at
    }
}

/// One `admin_audit_log` row (ADR-129 D18; ADR-073 §11).
#[derive(Debug, Clone, PartialEq)]
pub struct AdminAuditEntry {
    pub actor_id: String,
    pub action: String,
    pub target_resource: String,
    pub after_state: Option<Value>,
}

/// Durable store of codes and escalations, and the append-only audit log.
#[async_trait]
pub trait OperatorEscalationRepository: Send + Sync {
    async fn insert_code(&self, code: &OperatorEscalationCode) -> Result<(), RepositoryError>;

    /// The newest code of `consumer_sub` whose hash is `code_hash`, in any
    /// state.
    async fn find_code(
        &self,
        consumer_sub: &str,
        code_hash: &str,
    ) -> Result<Option<OperatorEscalationCode>, RepositoryError>;

    /// Consume a live code; `false` when it was no longer live (a second use,
    /// or a race lost to another redemption).
    async fn consume_code(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, RepositoryError>;

    /// Count one failure against every live code of `consumer_sub`; a code
    /// reaching `max_failed_attempts` is invalidated. Returns the codes
    /// counted, as they stand after the update (the Update's U3).
    async fn record_failure(
        &self,
        consumer_sub: &str,
        now: DateTime<Utc>,
        max_failed_attempts: u32,
    ) -> Result<Vec<OperatorEscalationCode>, RepositoryError>;

    async fn insert_escalation(
        &self,
        escalation: &OperatorEscalation,
    ) -> Result<(), RepositoryError>;

    async fn find_escalation(
        &self,
        id: Uuid,
    ) -> Result<Option<OperatorEscalation>, RepositoryError>;

    /// The active escalation of an API key with the latest `expires_at`.
    async fn active_for_api_key(
        &self,
        api_key_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<OperatorEscalation>, RepositoryError>;

    async fn active_for_system_sub(
        &self,
        system_sub: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError>;

    /// End one active escalation; `None` when it was not active.
    async fn end_escalation(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Option<OperatorEscalation>, RepositoryError>;

    async fn end_active_for_api_key(
        &self,
        api_key_id: Uuid,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError>;

    async fn end_active_for_system_sub(
        &self,
        system_sub: &str,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError>;

    /// End every unended escalation whose `expires_at` has passed, at its
    /// `expires_at`, reason `expired`.
    async fn end_expired(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError>;

    async fn append_audit(&self, entry: &AdminAuditEntry) -> Result<(), RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_decimal_digits() {
        for _ in 0..1_000 {
            let code = generate_code();
            assert!(is_code_shaped(&code), "not six digits: {code:?}");
        }
    }

    #[test]
    fn codes_are_not_repeated_in_a_small_draw() {
        let draws: std::collections::HashSet<String> = (0..64).map(|_| generate_code()).collect();
        // 64 draws from 10^6 collide with probability about 0.2 %; a constant
        // or badly seeded generator collides every time.
        assert!(draws.len() >= 60, "draws repeat: {}", draws.len());
    }

    #[test]
    fn shape_check_refuses_everything_but_six_digits() {
        for bad in ["", "12345", "1234567", "12a456", " 123456", "１２３４５６"] {
            assert!(!is_code_shaped(bad), "accepted {bad:?}");
        }
        assert!(is_code_shaped("000000"));
    }

    #[test]
    fn hash_is_sha256_hex_and_not_the_code() {
        let h = hash_code("123456");
        assert_eq!(h.len(), 64);
        assert_ne!(h, "123456");
        assert_eq!(
            h,
            "8d969eef6ecad3c29a3a629280e686cf0c3f5d5a86aff3ca12020c923adc6c92"
        );
    }

    #[test]
    fn end_reasons_round_trip() {
        for r in [
            EscalationEndReason::Expired,
            EscalationEndReason::OperatorWeb,
            EscalationEndReason::AgentRelease,
            EscalationEndReason::ApiKeyRevoked,
            EscalationEndReason::OperatorDemoted,
        ] {
            assert_eq!(EscalationEndReason::parse(r.as_str()), Some(r));
        }
    }
}
