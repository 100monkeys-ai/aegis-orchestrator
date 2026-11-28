// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Replay-Nonce Store (Audit 002 §4.17)
//!
//! A SEAL envelope is fresh for at most 30 seconds (enforced by the freshness
//! check inside [`crate::infrastructure::seal::envelope::SealEnvelope`] during
//! signature verification). Without per-envelope nonce tracking an attacker
//! who passively captures a signed envelope can replay it any number of times
//! within that window.
//!
//! This module provides a small, dependency-free in-memory store keyed by the
//! envelope's [`crate::domain::seal_session::EnvelopeVerifier::replay_nonce`]
//! value (the base64-encoded Ed25519 signature). Each entry expires
//! automatically after the configured TTL (30 s by default) and a background
//! sweep prunes the map so memory stays bounded.
//!
//! A cluster (AEGIS ADR-152 D4.2) shares the record: [`PostgresNonceStore`]
//! keeps this store as its first check and lets an insert into
//! `seal_replay_nonces` decide, so an envelope replayed to another node is a
//! replay too. A node with no database keeps the in-memory store alone.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// TTL for SEAL replay nonces — matches the envelope freshness window.
pub const SEAL_REPLAY_TTL: Duration = Duration::from_secs(30);

/// Outcome of recording a nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceOutcome {
    /// First time this nonce has been seen within the freshness window.
    Fresh,
    /// The nonce was already recorded and has not yet expired — replay.
    Replay,
}

/// Async-safe nonce tracker for SEAL envelope replay protection.
///
/// Implementations MUST be safe to share across many concurrent verify calls
/// and MUST honour the TTL semantics (an entry inserted at time `t` is
/// considered expired at time `t + ttl`).
#[async_trait::async_trait]
pub trait NonceStore: Send + Sync {
    /// Record `nonce`. Returns [`NonceOutcome::Replay`] if `nonce` is already
    /// present and unexpired; otherwise [`NonceOutcome::Fresh`]. An error is
    /// a store that could not decide, which the caller refuses.
    async fn record(&self, nonce: &str) -> anyhow::Result<NonceOutcome>;
}

/// Lock-free, in-memory [`NonceStore`] backed by [`DashMap`].
///
/// Entries are pruned on each `record` call (cheap O(n) sweep) so a separate
/// GC task is not required. For low-traffic deployments this is more than
/// adequate; high-throughput tenants should swap in a Redis-backed
/// implementation by providing a different [`NonceStore`].
pub struct InMemoryNonceStore {
    entries: DashMap<String, Instant>,
    ttl: Duration,
}

impl InMemoryNonceStore {
    /// Create a new store with the default 30-second TTL.
    pub fn new() -> Self {
        Self::with_ttl(SEAL_REPLAY_TTL)
    }

    /// Create a new store with a custom TTL (used in tests).
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
        }
    }

    /// Drop expired entries. Called inline before every `record` so the
    /// map size is bounded by the number of envelopes seen in the last
    /// `ttl` seconds.
    fn sweep(&self, now: Instant) {
        self.entries
            .retain(|_, inserted_at| now.duration_since(*inserted_at) < self.ttl);
    }
}

impl Default for InMemoryNonceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryNonceStore {
    /// The decision, synchronously.
    pub fn record_now(&self, nonce: &str) -> NonceOutcome {
        let now = Instant::now();
        self.sweep(now);

        // Use entry API so insert+check is atomic per-key.
        match self.entries.entry(nonce.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(occupied) => {
                let inserted_at = *occupied.get();
                if now.duration_since(inserted_at) < self.ttl {
                    NonceOutcome::Replay
                } else {
                    // Stale entry — replace with the fresh insertion.
                    occupied.replace_entry(now);
                    NonceOutcome::Fresh
                }
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(now);
                NonceOutcome::Fresh
            }
        }
    }
}

#[async_trait::async_trait]
impl NonceStore for InMemoryNonceStore {
    async fn record(&self, nonce: &str) -> anyhow::Result<NonceOutcome> {
        Ok(self.record_now(nonce))
    }
}

/// Decides in `seal_replay_nonces`: one row inserted is fresh (ADR-152 D4.2).
pub const RECORD_NONCE_SQL: &str = r#"
INSERT INTO seal_replay_nonces (nonce, expires_at)
VALUES ($1, now() + make_interval(secs => $2))
ON CONFLICT (nonce) DO NOTHING
"#;

/// The `seal-nonce-gc` duty's statement.
pub const DELETE_EXPIRED_NONCES_SQL: &str =
    "DELETE FROM seal_replay_nonces WHERE expires_at < now()";

/// The cluster's record of SEAL nonces (ADR-152 D4.2): this node's memory
/// first, then the shared table. A replay this node saw is refused without a
/// round trip; any other is decided by the insert.
pub struct PostgresNonceStore {
    local: InMemoryNonceStore,
    pool: sqlx::PgPool,
    ttl: Duration,
}

impl PostgresNonceStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self::with_ttl(pool, SEAL_REPLAY_TTL)
    }

    /// A store whose entries live `ttl`: a webhook delivery's id is
    /// remembered far longer than a SEAL nonce (AEGIS ADR-157 D4).
    pub fn with_ttl(pool: sqlx::PgPool, ttl: Duration) -> Self {
        Self {
            local: InMemoryNonceStore::with_ttl(ttl),
            pool,
            ttl,
        }
    }

    /// Delete the expired nonces; how many went.
    pub async fn delete_expired(pool: &sqlx::PgPool) -> anyhow::Result<u64> {
        Ok(sqlx::query(DELETE_EXPIRED_NONCES_SQL)
            .execute(pool)
            .await?
            .rows_affected())
    }
}

#[async_trait::async_trait]
impl NonceStore for PostgresNonceStore {
    async fn record(&self, nonce: &str) -> anyhow::Result<NonceOutcome> {
        if self.local.record_now(nonce) == NonceOutcome::Replay {
            return Ok(NonceOutcome::Replay);
        }
        let inserted = sqlx::query(RECORD_NONCE_SQL)
            .bind(nonce)
            .bind(self.ttl.as_secs_f64())
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(if inserted == 1 {
            NonceOutcome::Fresh
        } else {
            NonceOutcome::Replay
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn first_seen_is_fresh() {
        let store = InMemoryNonceStore::new();
        assert_eq!(store.record_now("sig-a"), NonceOutcome::Fresh);
    }

    #[test]
    fn second_seen_within_window_is_replay() {
        let store = InMemoryNonceStore::new();
        assert_eq!(store.record_now("sig-a"), NonceOutcome::Fresh);
        assert_eq!(store.record_now("sig-a"), NonceOutcome::Replay);
    }

    #[test]
    fn distinct_nonces_do_not_collide() {
        let store = InMemoryNonceStore::new();
        assert_eq!(store.record_now("sig-a"), NonceOutcome::Fresh);
        assert_eq!(store.record_now("sig-b"), NonceOutcome::Fresh);
    }

    #[test]
    fn nonce_is_accepted_again_after_ttl_elapses() {
        let store = InMemoryNonceStore::with_ttl(Duration::from_millis(50));
        assert_eq!(store.record_now("sig-a"), NonceOutcome::Fresh);
        sleep(Duration::from_millis(75));
        assert_eq!(store.record_now("sig-a"), NonceOutcome::Fresh);
    }
}
