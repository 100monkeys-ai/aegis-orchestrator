// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Operator escalation service (AEGIS ADR-129)
//!
//! Mints a code for an operator's stepped-up session (D13), redeems it for
//! one API key of the same person (D10, D14), answers whether a key or an
//! escalation is active (D14, D19), ends escalations early or on time (D19;
//! the Update's U5 and U6), and writes every act to `admin_audit_log` (D18).
//! The routes that call it are `cli/src/daemon/handlers/operator_escalations.rs`;
//! the dispatch-time check is `ToolInvocationService::invoke_tool`.
//!
//! A demotion ends an escalation (ADR-129 — Updates, V1 to V9): through the
//! [`OperatorRoleLookup`] port the service re-reads the operator's federated
//! record in the system realm at dispatch, and ends every active escalation
//! of a person whose record no longer grants the escalation's role.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

use crate::domain::iam::AegisRole;
use crate::domain::node_config::OperatorEscalationConfig;
use crate::domain::operator_escalation::{
    audit_action, generate_code, hash_code, is_code_shaped, AdminAuditEntry, EscalationEndReason,
    OperatorEscalation, OperatorEscalationCode, OperatorEscalationRepository, OperatorRecord,
    OperatorRoleLookup,
};
use crate::domain::repository::RepositoryError;

/// How often the daemon ends escalations past their `expires_at` and audits
/// the end (D19), and re-reads the role of every operator still holding one
/// (ADR-129 — Updates, V2).
pub const EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// What the service refuses, each with the error code the routes answer.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum OperatorEscalationError {
    /// The minting identity's role may not mint (D16: `aegis:readonly`).
    #[error("role_not_permitted")]
    RoleNotPermitted,
    /// No live code of the key's user matches (D8, D10).
    #[error("invalid_code")]
    InvalidCode,
    /// The code matched but is past `code_validity_seconds` (D7).
    #[error("code_expired")]
    CodeExpired,
    /// The escalation is not active, or not the caller's.
    #[error("escalation_not_found")]
    NotFound,
    /// The escalation a SEAL session was attested under has ended (D19).
    #[error("operator_escalation_expired")]
    Expired,
    /// The node cannot re-read an operator's role: it has no Keycloak admin
    /// client (`spec.iam.keycloak_admin` absent, or a value that did not
    /// resolve). Every escalated call is refused (ADR-129 — Updates, V5).
    #[error(
        "the operator role check is unavailable: this node has no Keycloak admin client \
         (spec.iam.keycloak_admin)"
    )]
    RoleCheckUnavailable,
    /// The role lookup could not read the record (V5). The call is refused
    /// and the escalation is left active: an outage is not a demotion.
    #[error("{0}")]
    RoleLookupFailed(String),
    #[error("repository: {0}")]
    Repository(String),
}

impl From<RepositoryError> for OperatorEscalationError {
    fn from(e: RepositoryError) -> Self {
        Self::Repository(e.to_string())
    }
}

/// A freshly minted code: the digits, returned once and never stored (D9,
/// D13).
#[derive(Debug, Clone)]
pub struct MintedCode {
    pub code: String,
    pub code_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

/// The API key presenting a code (D10, D11).
#[derive(Debug, Clone)]
pub struct RedeemingKey {
    pub api_key_id: Uuid,
    /// `api_keys.user_id`: the `sub` the key was created under.
    pub user_id: String,
    /// Whether the key was stored with an `aegis_role` (a role-bearing key,
    /// which cannot hold an escalation, D10).
    pub has_stored_role: bool,
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub struct OperatorEscalationService {
    repo: Arc<dyn OperatorEscalationRepository>,
    config: OperatorEscalationConfig,
    clock: Clock,
    /// The operator's federated record, re-read at dispatch (V1). `None` on
    /// a node without a Keycloak admin client, which refuses every
    /// escalated call and every redemption (V5).
    role_lookup: Option<Arc<dyn OperatorRoleLookup>>,
}

impl OperatorEscalationService {
    pub fn new(
        repo: Arc<dyn OperatorEscalationRepository>,
        config: OperatorEscalationConfig,
    ) -> Self {
        Self {
            repo,
            config,
            clock: Arc::new(Utc::now),
            role_lookup: None,
        }
    }

    /// Attach the role lookup the daemon builds from the Keycloak admin
    /// client (ADR-129 — Updates, V9).
    pub fn with_role_lookup(mut self, lookup: Arc<dyn OperatorRoleLookup>) -> Self {
        self.role_lookup = Some(lookup);
        self
    }

    /// Whether this node can re-read an operator's role (V5). Without it the
    /// redemption route answers 503 and every escalated call is refused.
    pub fn can_check_roles(&self) -> bool {
        self.role_lookup.is_some()
    }

    /// Replace the clock (tests move time past a bound without sleeping).
    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    pub fn config(&self) -> &OperatorEscalationConfig {
        &self.config
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    async fn audit(
        &self,
        actor_id: &str,
        action: &str,
        target: &str,
        after_state: serde_json::Value,
    ) -> Result<(), OperatorEscalationError> {
        self.repo
            .append_audit(&AdminAuditEntry {
                actor_id: actor_id.to_string(),
                action: action.to_string(),
                target_resource: target.to_string(),
                after_state: Some(after_state),
            })
            .await?;
        Ok(())
    }

    /// Mint a code for the operator whose system token names `system_sub`,
    /// `consumer_sub` and `aegis_role` (D13). `aegis:readonly` mints nothing
    /// (D16). The caller has already checked the token's realm and `azp`.
    pub async fn mint(
        &self,
        system_sub: &str,
        consumer_sub: &str,
        aegis_role: AegisRole,
    ) -> Result<MintedCode, OperatorEscalationError> {
        if !matches!(aegis_role, AegisRole::Admin | AegisRole::Operator) {
            return Err(OperatorEscalationError::RoleNotPermitted);
        }
        let now = self.now();
        let code = generate_code();
        let row = OperatorEscalationCode {
            id: Uuid::new_v4(),
            code_hash: hash_code(&code),
            consumer_sub: consumer_sub.to_string(),
            system_sub: system_sub.to_string(),
            aegis_role,
            created_at: now,
            expires_at: now + chrono::Duration::seconds(self.config.code_validity_seconds as i64),
            consumed_at: None,
            failed_attempts: 0,
            invalidated_at: None,
        };
        self.repo.insert_code(&row).await?;
        self.audit(
            system_sub,
            audit_action::CODE_ISSUED,
            consumer_sub,
            json!({ "code_id": row.id, "expires_at": row.expires_at }),
        )
        .await?;
        Ok(MintedCode {
            code,
            code_id: row.id,
            expires_at: row.expires_at,
        })
    }

    /// Redeem `code` for `key` (D8, D10, D14). Success consumes the code and
    /// writes an escalation held by that key alone; any failure counts
    /// against every live code of the key's user (the Update's U3).
    pub async fn redeem(
        &self,
        key: &RedeemingKey,
        code: &str,
    ) -> Result<OperatorEscalation, OperatorEscalationError> {
        let now = self.now();
        let matched = if !key.has_stored_role && is_code_shaped(code) {
            self.repo.find_code(&key.user_id, &hash_code(code)).await?
        } else {
            None
        };
        let failure = match matched {
            Some(found) if found.is_live(now) => {
                self.confirm_role_at_redemption(key, &found, now).await?;
                if self.repo.consume_code(found.id, now).await? {
                    return self.start_escalation(key, &found, now).await;
                }
                OperatorEscalationError::InvalidCode
            }
            Some(found)
                if found.consumed_at.is_none()
                    && found.invalidated_at.is_none()
                    && now >= found.expires_at =>
            {
                OperatorEscalationError::CodeExpired
            }
            _ => OperatorEscalationError::InvalidCode,
        };
        self.record_failure(key, &failure, now).await?;
        Err(failure)
    }

    /// The check at redemption (ADR-129 — Updates, V3; the coordinator's
    /// V10): before a matched live code is consumed, the code's operator's
    /// record is read. A record that no longer grants the code's role
    /// invalidates that one code and is answered exactly as a wrong code is,
    /// [`OperatorEscalationError::InvalidCode`], audited `redeem_failed` with
    /// a `reason` naming the demotion; no failure is counted against the
    /// user's other codes. A lookup that fails answers
    /// [`OperatorEscalationError::RoleLookupFailed`], the code neither
    /// consumed nor counted (V5). A service with no lookup is on a node whose
    /// route answers 503 before reaching it (V5; V11).
    async fn confirm_role_at_redemption(
        &self,
        key: &RedeemingKey,
        code: &OperatorEscalationCode,
        now: DateTime<Utc>,
    ) -> Result<(), OperatorEscalationError> {
        let Some(lookup) = &self.role_lookup else {
            return Ok(());
        };
        let record = match lookup.lookup(&code.system_sub).await {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!(
                    code_id = %code.id,
                    error = %e,
                    "Operator role lookup failed at redemption; refused, the code neither consumed nor counted"
                );
                return Err(OperatorEscalationError::RoleLookupFailed(e.0));
            }
        };
        if record.grants(&code.aegis_role) {
            return Ok(());
        }
        self.repo.invalidate_code(code.id, now).await?;
        self.audit(
            &code.system_sub,
            audit_action::REDEEM_FAILED,
            &key.api_key_id.to_string(),
            json!({
                "error": OperatorEscalationError::InvalidCode.to_string(),
                "reason": "operator_demoted",
                "role_found": record.role_found(),
                "code_id": code.id,
                "invalidated": true,
            }),
        )
        .await?;
        Err(OperatorEscalationError::InvalidCode)
    }

    async fn start_escalation(
        &self,
        key: &RedeemingKey,
        code: &OperatorEscalationCode,
        now: DateTime<Utc>,
    ) -> Result<OperatorEscalation, OperatorEscalationError> {
        let escalation = OperatorEscalation {
            id: Uuid::new_v4(),
            api_key_id: key.api_key_id,
            consumer_sub: code.consumer_sub.clone(),
            system_sub: code.system_sub.clone(),
            aegis_role: code.aegis_role.clone(),
            code_id: code.id,
            started_at: now,
            expires_at: now + chrono::Duration::seconds(self.config.ttl_seconds as i64),
            ended_at: None,
            end_reason: None,
        };
        self.repo.insert_escalation(&escalation).await?;
        self.audit(
            &escalation.system_sub,
            audit_action::REDEEMED,
            &key.api_key_id.to_string(),
            json!({
                "escalation_id": escalation.id,
                "code_id": code.id,
                "aegis_role": escalation.aegis_role.as_claim_str(),
                "expires_at": escalation.expires_at,
            }),
        )
        .await?;
        Ok(escalation)
    }

    async fn record_failure(
        &self,
        key: &RedeemingKey,
        failure: &OperatorEscalationError,
        now: DateTime<Utc>,
    ) -> Result<(), OperatorEscalationError> {
        let counted = if key.has_stored_role {
            Vec::new()
        } else {
            self.repo
                .record_failure(&key.user_id, now, self.config.code_max_failed_attempts)
                .await?
        };
        let target = key.api_key_id.to_string();
        if counted.is_empty() {
            // The Update's U4: no live code to name a system sub.
            return self
                .audit(
                    &format!("api_key:{}", key.api_key_id),
                    audit_action::REDEEM_FAILED,
                    &target,
                    json!({ "error": failure.to_string() }),
                )
                .await;
        }
        for code in counted {
            self.audit(
                &code.system_sub,
                audit_action::REDEEM_FAILED,
                &target,
                json!({
                    "error": failure.to_string(),
                    "code_id": code.id,
                    "failed_attempts": code.failed_attempts,
                    "invalidated": code.invalidated_at.is_some(),
                }),
            )
            .await?;
        }
        Ok(())
    }

    /// The key's active escalation, if any (D14).
    pub async fn active_for_api_key(
        &self,
        api_key_id: Uuid,
    ) -> Result<Option<OperatorEscalation>, OperatorEscalationError> {
        Ok(self.repo.active_for_api_key(api_key_id, self.now()).await?)
    }

    /// The dispatch-time check (D19): the escalation a session was attested
    /// under, if it is still active.
    pub async fn check_active(
        &self,
        escalation_id: Uuid,
    ) -> Result<OperatorEscalation, OperatorEscalationError> {
        match self.repo.find_escalation(escalation_id).await? {
            Some(e) if e.is_active(self.now()) => Ok(e),
            _ => Err(OperatorEscalationError::Expired),
        }
    }

    /// The dispatch-time role check (ADR-129 — Updates, V1, V5, V6), made
    /// after [`Self::check_active`] finds the escalation active.
    ///
    /// - A node with no lookup cannot make the check: refused, nothing ended
    ///   (V5).
    /// - A lookup that fails: refused, the escalation left active, a warning
    ///   logged naming the escalation and the error, no audit row (V5, V6).
    /// - A record that is absent, disabled, or holds no role or another role:
    ///   every active escalation of that `system_sub` is ended
    ///   `operator_demoted`, and the call is refused
    ///   [`OperatorEscalationError::Expired`] (V1).
    pub async fn confirm_role(
        &self,
        escalation: &OperatorEscalation,
    ) -> Result<(), OperatorEscalationError> {
        let Some(lookup) = &self.role_lookup else {
            return Err(OperatorEscalationError::RoleCheckUnavailable);
        };
        let record = match lookup.lookup(&escalation.system_sub).await {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!(
                    escalation_id = %escalation.id,
                    error = %e,
                    "Operator role lookup failed; the escalated call is refused and the escalation left active"
                );
                return Err(OperatorEscalationError::RoleLookupFailed(e.0));
            }
        };
        if record.grants(&escalation.aegis_role) {
            return Ok(());
        }
        self.end_demoted(&escalation.system_sub, &record).await?;
        Err(OperatorEscalationError::Expired)
    }

    /// End every active escalation of `system_sub` because its record no
    /// longer grants the role (V1, V2), each `ended` row carrying what was
    /// found and when it was read (V6).
    async fn end_demoted(
        &self,
        system_sub: &str,
        record: &OperatorRecord,
    ) -> Result<Vec<OperatorEscalation>, OperatorEscalationError> {
        let checked_at = self.now();
        let ended = self
            .repo
            .end_active_for_system_sub(system_sub, checked_at, EscalationEndReason::OperatorDemoted)
            .await?;
        for e in &ended {
            self.audit_ended_with(
                e,
                json!({ "role_found": record.role_found(), "checked_at": checked_at }),
            )
            .await?;
        }
        if !ended.is_empty() {
            tracing::info!(
                ended = ended.len(),
                role_found = %record.role_found(),
                "Ended the operator escalations of a demoted operator"
            );
        }
        Ok(ended)
    }

    /// The operator's own active escalations (the Update's U5).
    pub async fn list_active_for_operator(
        &self,
        system_sub: &str,
    ) -> Result<Vec<OperatorEscalation>, OperatorEscalationError> {
        Ok(self
            .repo
            .active_for_system_sub(system_sub, self.now())
            .await?)
    }

    /// End one of the operator's own escalations from the web page (U5).
    pub async fn end_by_operator(
        &self,
        system_sub: &str,
        escalation_id: Uuid,
    ) -> Result<OperatorEscalation, OperatorEscalationError> {
        match self.repo.find_escalation(escalation_id).await? {
            Some(e) if e.system_sub == system_sub => {}
            _ => return Err(OperatorEscalationError::NotFound),
        }
        let ended = self
            .repo
            .end_escalation(escalation_id, self.now(), EscalationEndReason::OperatorWeb)
            .await?
            .ok_or(OperatorEscalationError::NotFound)?;
        self.audit_ended(&ended).await?;
        Ok(ended)
    }

    /// End every active escalation of a key: the agent's release (U5) or
    /// the key's revocation (D19).
    pub async fn end_for_api_key(
        &self,
        api_key_id: Uuid,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, OperatorEscalationError> {
        let ended = self
            .repo
            .end_active_for_api_key(api_key_id, self.now(), reason)
            .await?;
        for e in &ended {
            self.audit_ended(e).await?;
        }
        Ok(ended)
    }

    /// End every active escalation of an operator, at demotion (U6).
    pub async fn end_for_operator(
        &self,
        system_sub: &str,
    ) -> Result<Vec<OperatorEscalation>, OperatorEscalationError> {
        let ended = self
            .repo
            .end_active_for_system_sub(system_sub, self.now(), EscalationEndReason::OperatorDemoted)
            .await?;
        for e in &ended {
            self.audit_ended(e).await?;
        }
        Ok(ended)
    }

    /// End and audit every escalation past its `expires_at` (D19). Returns
    /// how many ended.
    pub async fn end_expired(&self) -> Result<usize, OperatorEscalationError> {
        let ended = self.repo.end_expired(self.now()).await?;
        for e in &ended {
            self.audit_ended(e).await?;
        }
        Ok(ended.len())
    }

    async fn audit_ended(&self, e: &OperatorEscalation) -> Result<(), OperatorEscalationError> {
        self.audit_ended_with(e, json!({})).await
    }

    /// The `ended` row, with `extra`'s fields added to its `after_state`.
    async fn audit_ended_with(
        &self,
        e: &OperatorEscalation,
        extra: serde_json::Value,
    ) -> Result<(), OperatorEscalationError> {
        let mut state = json!({
            "escalation_id": e.id,
            "end_reason": e.end_reason.map(|r| r.as_str()),
            "ended_at": e.ended_at,
        });
        if let (Some(state), serde_json::Value::Object(extra)) = (state.as_object_mut(), extra) {
            state.extend(extra);
        }
        self.audit(
            &e.system_sub,
            audit_action::ENDED,
            &e.api_key_id.to_string(),
            state,
        )
        .await
    }

    /// Audit one tool call made under an escalation (D18): `target` is the
    /// tool and the tenant it acted on, `*` for an all-tenant read.
    pub async fn audit_tool_call(
        &self,
        escalation: &OperatorEscalation,
        tool_name: &str,
        tenant: &str,
    ) -> Result<(), OperatorEscalationError> {
        self.audit(
            &escalation.system_sub,
            audit_action::TOOL_CALL,
            &format!("{tool_name}@{tenant}"),
            json!({
                "escalation_id": escalation.id,
                "api_key_id": escalation.api_key_id,
                "tool": tool_name,
                "tenant": tenant,
            }),
        )
        .await
    }

    /// The sweep's role re-check (ADR-129 — Updates, V2): one read per
    /// distinct `system_sub` holding an active escalation; a record that no
    /// longer grants the role of every one of that operator's active
    /// escalations ends them all as V1 does. A read that fails leaves that
    /// operator's escalations active and is logged (V5). Returns how many
    /// ended. A node with no lookup has nothing to read.
    pub async fn recheck_roles(&self) -> Result<usize, OperatorEscalationError> {
        let Some(lookup) = &self.role_lookup else {
            return Ok(0);
        };
        let mut ended = 0;
        for system_sub in self.repo.active_system_subs(self.now()).await? {
            let record = match lookup.lookup(&system_sub).await {
                Ok(record) => record,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Operator role lookup failed in the sweep; that operator's escalations are left active"
                    );
                    continue;
                }
            };
            let active = self
                .repo
                .active_for_system_sub(&system_sub, self.now())
                .await?;
            if active.iter().all(|e| record.grants(&e.aegis_role)) {
                continue;
            }
            ended += self.end_demoted(&system_sub, &record).await?.len();
        }
        Ok(ended)
    }

    /// One pass of the sweep: end escalations past `expires_at` (D19), then
    /// re-check the roles of the operators still holding one (V2).
    pub async fn sweep(&self) {
        match self.end_expired().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(ended = n, "Ended expired operator escalations"),
            Err(e) => tracing::warn!(error = %e, "Operator escalation expiry sweep failed"),
        }
        match self.recheck_roles().await {
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "Operator escalation role re-check failed"),
        }
    }

    /// Sweep every `interval` for the life of the process (the daemon passes
    /// [`EXPIRY_SWEEP_INTERVAL`]).
    pub fn spawn_expiry_sweep(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                self.sweep().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::repositories::postgres_operator_escalation::InMemoryOperatorEscalationRepository;
    use std::sync::Mutex;

    const CONSUMER: &str = "consumer-sub-1";
    const SYSTEM: &str = "system-sub-1";

    struct Fixture {
        repo: Arc<InMemoryOperatorEscalationRepository>,
        service: OperatorEscalationService,
        now: Arc<Mutex<DateTime<Utc>>>,
    }

    impl Fixture {
        fn new() -> Self {
            let repo = Arc::new(InMemoryOperatorEscalationRepository::new());
            let now = Arc::new(Mutex::new(Utc::now()));
            let clock = now.clone();
            let service =
                OperatorEscalationService::new(repo.clone(), OperatorEscalationConfig::default())
                    .with_clock(move || *clock.lock().unwrap());
            Self { repo, service, now }
        }

        fn advance(&self, seconds: i64) {
            let mut now = self.now.lock().unwrap();
            *now += chrono::Duration::seconds(seconds);
        }

        async fn actions(&self) -> Vec<(String, String)> {
            self.repo
                .audit_entries()
                .await
                .into_iter()
                .map(|e| (e.actor_id, e.action))
                .collect()
        }
    }

    fn consumer_key() -> RedeemingKey {
        RedeemingKey {
            api_key_id: Uuid::new_v4(),
            user_id: CONSUMER.to_string(),
            has_stored_role: false,
        }
    }

    fn wrong(code: &str) -> String {
        let n: u32 = code.parse().unwrap();
        format!("{:06}", (n + 1) % 1_000_000)
    }

    #[tokio::test]
    async fn redeem_starts_escalation_for_ttl_seconds() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let key = consumer_key();
        let e = f.service.redeem(&key, &minted.code).await.unwrap();
        assert_eq!(e.api_key_id, key.api_key_id);
        assert_eq!(e.aegis_role, AegisRole::Operator);
        assert_eq!((e.expires_at - e.started_at).num_seconds(), 1800);
        assert_eq!(
            f.service.active_for_api_key(key.api_key_id).await.unwrap(),
            Some(e)
        );
    }

    #[tokio::test]
    async fn second_redemption_refused() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let key = consumer_key();
        f.service.redeem(&key, &minted.code).await.unwrap();
        assert_eq!(
            f.service.redeem(&key, &minted.code).await,
            Err(OperatorEscalationError::InvalidCode)
        );
    }

    #[tokio::test]
    async fn redemption_refused_after_code_validity_seconds() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        f.advance(299);
        let early = consumer_key();
        // One second before the bound the code is still live; redeem it with
        // a fresh code below instead, so this one stays unconsumed.
        assert!(f
            .repo
            .find_code(CONSUMER, &hash_code(&minted.code))
            .await
            .unwrap()
            .unwrap()
            .is_live(*f.now.lock().unwrap()));
        f.advance(1);
        assert_eq!(
            f.service.redeem(&early, &minted.code).await,
            Err(OperatorEscalationError::CodeExpired)
        );
    }

    #[tokio::test]
    async fn fifth_failure_invalidates_code() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let key = consumer_key();
        for _ in 0..4 {
            assert_eq!(
                f.service.redeem(&key, &wrong(&minted.code)).await,
                Err(OperatorEscalationError::InvalidCode)
            );
        }
        // Four failures: the right code still redeems on a fresh mint's twin;
        // check the stored count instead of consuming it.
        let stored = f
            .repo
            .find_code(CONSUMER, &hash_code(&minted.code))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.failed_attempts, 4);
        assert!(stored.invalidated_at.is_none());
        assert!(f.service.redeem(&key, &wrong(&minted.code)).await.is_err());
        assert_eq!(
            f.service.redeem(&key, &minted.code).await,
            Err(OperatorEscalationError::InvalidCode),
            "the fifth failure must invalidate the code"
        );
    }

    #[tokio::test]
    async fn four_failures_leave_the_code_redeemable() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let key = consumer_key();
        for _ in 0..4 {
            let _ = f.service.redeem(&key, &wrong(&minted.code)).await;
        }
        assert!(f.service.redeem(&key, &minted.code).await.is_ok());
    }

    #[tokio::test]
    async fn other_users_key_refused_and_does_not_consume() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let other = RedeemingKey {
            api_key_id: Uuid::new_v4(),
            user_id: "someone-else".to_string(),
            has_stored_role: false,
        };
        assert_eq!(
            f.service.redeem(&other, &minted.code).await,
            Err(OperatorEscalationError::InvalidCode)
        );
        assert!(f
            .service
            .redeem(&consumer_key(), &minted.code)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn role_bearing_key_refused_even_with_matching_sub() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let role_bearing = RedeemingKey {
            api_key_id: Uuid::new_v4(),
            user_id: CONSUMER.to_string(),
            has_stored_role: true,
        };
        assert_eq!(
            f.service.redeem(&role_bearing, &minted.code).await,
            Err(OperatorEscalationError::InvalidCode)
        );
    }

    #[tokio::test]
    async fn readonly_mints_nothing() {
        let f = Fixture::new();
        assert_eq!(
            f.service
                .mint(SYSTEM, CONSUMER, AegisRole::Readonly)
                .await
                .unwrap_err(),
            OperatorEscalationError::RoleNotPermitted
        );
        assert!(f.repo.audit_entries().await.is_empty());
    }

    #[tokio::test]
    async fn check_active_refuses_after_expires_at() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let e = f
            .service
            .redeem(&consumer_key(), &minted.code)
            .await
            .unwrap();
        f.advance(1799);
        assert!(f.service.check_active(e.id).await.is_ok());
        f.advance(1);
        assert_eq!(
            f.service.check_active(e.id).await,
            Err(OperatorEscalationError::Expired)
        );
    }

    #[tokio::test]
    async fn sweep_ends_expired_and_audits() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let e = f
            .service
            .redeem(&consumer_key(), &minted.code)
            .await
            .unwrap();
        assert_eq!(f.service.end_expired().await.unwrap(), 0);
        f.advance(1800);
        assert_eq!(f.service.end_expired().await.unwrap(), 1);
        let ended = f.repo.find_escalation(e.id).await.unwrap().unwrap();
        assert_eq!(ended.end_reason, Some(EscalationEndReason::Expired));
        assert_eq!(ended.ended_at, Some(e.expires_at));
    }

    #[tokio::test]
    async fn end_by_operator_only_for_own_escalation() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let e = f
            .service
            .redeem(&consumer_key(), &minted.code)
            .await
            .unwrap();
        assert_eq!(
            f.service.end_by_operator("another-operator", e.id).await,
            Err(OperatorEscalationError::NotFound)
        );
        let ended = f.service.end_by_operator(SYSTEM, e.id).await.unwrap();
        assert_eq!(ended.end_reason, Some(EscalationEndReason::OperatorWeb));
        assert!(f.service.check_active(e.id).await.is_err());
    }

    #[tokio::test]
    async fn end_for_operator_ends_every_escalation_of_the_person() {
        let f = Fixture::new();
        let a = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let b = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        f.service.redeem(&consumer_key(), &a.code).await.unwrap();
        f.service.redeem(&consumer_key(), &b.code).await.unwrap();
        let ended = f.service.end_for_operator(SYSTEM).await.unwrap();
        assert_eq!(ended.len(), 2);
        assert!(ended
            .iter()
            .all(|e| e.end_reason == Some(EscalationEndReason::OperatorDemoted)));
        assert!(f
            .service
            .list_active_for_operator(SYSTEM)
            .await
            .unwrap()
            .is_empty());
    }

    /// ADR-129 D18: code_issued, redeemed, redeem_failed and ended are each
    /// written, with the system sub as actor, and `api_key:<id>` for a
    /// failure with no live code (the Update's U4). `tool_call` is covered in
    /// `tool_invocation_service::operator_escalation_tests`.
    #[tokio::test]
    async fn each_audit_action_is_written() {
        let f = Fixture::new();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let key = consumer_key();
        let _ = f.service.redeem(&key, &wrong(&minted.code)).await;
        let e = f.service.redeem(&key, &minted.code).await.unwrap();
        let stranger = RedeemingKey {
            api_key_id: Uuid::new_v4(),
            user_id: "nobody".to_string(),
            has_stored_role: false,
        };
        let _ = f.service.redeem(&stranger, "000000").await;
        f.service
            .end_for_api_key(key.api_key_id, EscalationEndReason::AgentRelease)
            .await
            .unwrap();
        f.service
            .audit_tool_call(&e, "aegis.task.list", "*")
            .await
            .unwrap();

        let actions = f.actions().await;
        assert_eq!(
            actions,
            vec![
                (SYSTEM.to_string(), audit_action::CODE_ISSUED.to_string()),
                (SYSTEM.to_string(), audit_action::REDEEM_FAILED.to_string()),
                (SYSTEM.to_string(), audit_action::REDEEMED.to_string()),
                (
                    format!("api_key:{}", stranger.api_key_id),
                    audit_action::REDEEM_FAILED.to_string()
                ),
                (SYSTEM.to_string(), audit_action::ENDED.to_string()),
                (SYSTEM.to_string(), audit_action::TOOL_CALL.to_string()),
            ]
        );
        let entries = f.repo.audit_entries().await;
        assert_eq!(
            entries[0].target_resource, CONSUMER,
            "code_issued targets the consumer sub"
        );
        assert!(
            !entries[0]
                .after_state
                .as_ref()
                .unwrap()
                .to_string()
                .contains(&minted.code),
            "the code itself is never audited"
        );
        assert_eq!(entries[2].target_resource, key.api_key_id.to_string());
        assert_eq!(
            entries[4].after_state.as_ref().unwrap()["end_reason"],
            "agent_release"
        );
        assert_eq!(entries[5].target_resource, "aegis.task.list@*");
    }

    // ── ADR-129 — Updates, V2 and V3 (with the coordinator's V10) ──────────

    use crate::domain::operator_escalation::RoleLookupError;
    use std::collections::HashMap;

    /// The federated records of the system realm, by `system_sub`; a sub
    /// with no entry answers absent. Counts reads per sub.
    #[derive(Default)]
    struct Records {
        answers: Mutex<HashMap<String, Result<OperatorRecord, RoleLookupError>>>,
        reads: Mutex<HashMap<String, usize>>,
    }

    impl Records {
        fn set(&self, sub: &str, answer: Result<OperatorRecord, RoleLookupError>) {
            self.answers.lock().unwrap().insert(sub.to_string(), answer);
        }
        fn holds(&self, sub: &str, role: &str) {
            self.set(
                sub,
                Ok(OperatorRecord::Found {
                    aegis_role: Some(role.to_string()),
                }),
            );
        }
        fn reads(&self, sub: &str) -> usize {
            *self.reads.lock().unwrap().get(sub).unwrap_or(&0)
        }
    }

    #[async_trait::async_trait]
    impl OperatorRoleLookup for Records {
        async fn lookup(&self, system_sub: &str) -> Result<OperatorRecord, RoleLookupError> {
            *self
                .reads
                .lock()
                .unwrap()
                .entry(system_sub.to_string())
                .or_default() += 1;
            self.answers
                .lock()
                .unwrap()
                .get(system_sub)
                .cloned()
                .unwrap_or(Ok(OperatorRecord::Absent))
        }
    }

    fn with_records() -> (Fixture, Arc<Records>) {
        let records = Arc::new(Records::default());
        let mut f = Fixture::new();
        let service = std::mem::replace(
            &mut f.service,
            OperatorEscalationService::new(
                Arc::new(InMemoryOperatorEscalationRepository::new()),
                OperatorEscalationConfig::default(),
            ),
        );
        f.service = service.with_role_lookup(records.clone());
        (f, records)
    }

    async fn escalate_as(f: &Fixture, system: &str, consumer: &str) -> OperatorEscalation {
        let minted = f
            .service
            .mint(system, consumer, AegisRole::Operator)
            .await
            .unwrap();
        let key = RedeemingKey {
            api_key_id: Uuid::new_v4(),
            user_id: consumer.to_string(),
            has_stored_role: false,
        };
        f.service.redeem(&key, &minted.code).await.unwrap()
    }

    /// V2: the sweep reads each operator holding an active escalation once,
    /// ends every idle escalation of a demoted operator `operator_demoted`
    /// with an `ended` row carrying `role_found` and `checked_at` (V6), and
    /// leaves an operator whose record still grants the role.
    #[tokio::test]
    async fn sweep_ends_idle_escalations_of_a_demoted_operator() {
        let (f, records) = with_records();
        records.holds(SYSTEM, "aegis:operator");
        records.holds("system-kept", "aegis:operator");
        let a = escalate_as(&f, SYSTEM, CONSUMER).await;
        let b = escalate_as(&f, SYSTEM, CONSUMER).await;
        let kept = escalate_as(&f, "system-kept", "consumer-kept").await;
        let reads_before = records.reads(SYSTEM);

        records.set(SYSTEM, Ok(OperatorRecord::Found { aegis_role: None }));
        assert_eq!(f.service.recheck_roles().await.unwrap(), 2);
        assert_eq!(
            records.reads(SYSTEM) - reads_before,
            1,
            "one read per operator"
        );
        for e in [&a, &b] {
            let row = f.repo.find_escalation(e.id).await.unwrap().unwrap();
            assert_eq!(row.end_reason, Some(EscalationEndReason::OperatorDemoted));
        }
        assert!(f.service.check_active(kept.id).await.is_ok());
        let ended: Vec<serde_json::Value> = f
            .repo
            .audit_entries()
            .await
            .into_iter()
            .filter(|e| e.action == audit_action::ENDED)
            .map(|e| e.after_state.unwrap())
            .collect();
        assert_eq!(ended.len(), 2);
        for state in ended {
            assert_eq!(state["end_reason"], "operator_demoted");
            assert!(state["role_found"].is_null(), "{state}");
            assert!(state["checked_at"].is_string(), "{state}");
        }
        assert_eq!(f.service.recheck_roles().await.unwrap(), 0);
    }

    /// V2 with V5: a read that fails in the sweep leaves the operator's
    /// escalations active and writes no row.
    #[tokio::test]
    async fn sweep_lookup_error_leaves_escalations_active() {
        let (f, records) = with_records();
        records.holds(SYSTEM, "aegis:operator");
        let e = escalate_as(&f, SYSTEM, CONSUMER).await;
        records.set(SYSTEM, Err(RoleLookupError("connection refused".into())));
        assert_eq!(f.service.recheck_roles().await.unwrap(), 0);
        assert!(f.service.check_active(e.id).await.is_ok());
        assert!(!f
            .repo
            .audit_entries()
            .await
            .iter()
            .any(|a| a.action == audit_action::ENDED));
    }

    /// V3 with V10: a redemption whose operator's record no longer grants
    /// the code's role is answered `invalid_code`, invalidates that code
    /// only, counts no failure against the user's other code, starts no
    /// escalation, and is audited `redeem_failed` with a reason naming the
    /// demotion.
    #[tokio::test]
    async fn redemption_refused_as_invalid_code_when_the_record_no_longer_grants() {
        let (f, records) = with_records();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        let other = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        records.set(SYSTEM, Ok(OperatorRecord::Absent));
        let key = consumer_key();
        assert_eq!(
            f.service.redeem(&key, &minted.code).await,
            Err(OperatorEscalationError::InvalidCode)
        );
        let stored = f
            .repo
            .find_code(CONSUMER, &hash_code(&minted.code))
            .await
            .unwrap()
            .unwrap();
        assert!(stored.invalidated_at.is_some(), "that code is invalidated");
        assert!(stored.consumed_at.is_none());
        let untouched = f
            .repo
            .find_code(CONSUMER, &hash_code(&other.code))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.failed_attempts, 0);
        assert!(untouched.invalidated_at.is_none());
        assert_eq!(
            f.service.active_for_api_key(key.api_key_id).await.unwrap(),
            None
        );
        let last = f.repo.audit_entries().await.pop().unwrap();
        assert_eq!(last.action, audit_action::REDEEM_FAILED);
        assert_eq!(last.actor_id, SYSTEM);
        let state = last.after_state.unwrap();
        assert_eq!(state["error"], "invalid_code");
        assert_eq!(state["reason"], "operator_demoted");
        assert_eq!(state["role_found"], "user_absent");
        assert!(!state.to_string().contains(&minted.code));
        assert!(!state.to_string().contains(&hash_code(&minted.code)));
        // Restored, the other code redeems.
        records.holds(SYSTEM, "aegis:operator");
        assert!(f.service.redeem(&consumer_key(), &other.code).await.is_ok());
    }

    /// V3 with V5: a read that fails at redemption neither consumes the code
    /// nor counts a failure.
    #[tokio::test]
    async fn redemption_lookup_error_neither_consumes_nor_counts() {
        let (f, records) = with_records();
        let minted = f
            .service
            .mint(SYSTEM, CONSUMER, AegisRole::Operator)
            .await
            .unwrap();
        records.set(
            SYSTEM,
            Err(RoleLookupError("realm operation failed: 502".into())),
        );
        let key = consumer_key();
        assert_eq!(
            f.service.redeem(&key, &minted.code).await,
            Err(OperatorEscalationError::RoleLookupFailed(
                "realm operation failed: 502".into()
            ))
        );
        let stored = f
            .repo
            .find_code(CONSUMER, &hash_code(&minted.code))
            .await
            .unwrap()
            .unwrap();
        assert!(stored.consumed_at.is_none() && stored.invalidated_at.is_none());
        assert_eq!(stored.failed_attempts, 0);
        records.holds(SYSTEM, "aegis:operator");
        assert!(f.service.redeem(&key, &minted.code).await.is_ok());
    }
}
