// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Replay-Nonce Store (Audit 002 §4.17)
//!
//! A SEAL envelope is fresh for at most 30 seconds (see
//! [`crate::infrastructure::seal::envelope::SealEnvelope::signed_message`]).
//! Without per-envelope nonce tracking an attacker who passively captures a
//! signed envelope can replay it any number of times within that window.
//!
//! This module provides a small, dependency-free in-memory store keyed by the
//! envelope's [`crate::domain::seal_session::EnvelopeVerifier::replay_nonce`]
//! value (the base64-encoded Ed25519 signature). Each entry expires
//! automatically after the configured TTL (30 s by default) and a background
//! sweep prunes the map so memory stays bounded.
//!
//! Expected wire-up: the orchestrator constructs one [`InMemoryNonceStore`]
//! per process and shares it with [`crate::infrastructure::seal::SealMiddleware`].

use std::sync::Arc;
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
pub trait NonceStore: Send + Sync {
    /// Record `nonce`. Returns [`NonceOutcome::Replay`] if `nonce` is already
    /// present and unexpired; otherwise [`NonceOutcome::Fresh`].
    fn record(&self, nonce: &str) -> NonceOutcome;
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

impl NonceStore for InMemoryNonceStore {
    fn record(&self, nonce: &str) -> NonceOutcome {
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

impl<T: NonceStore + ?Sized> NonceStore for Arc<T> {
    fn record(&self, nonce: &str) -> NonceOutcome {
        (**self).record(nonce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn first_seen_is_fresh() {
        let store = InMemoryNonceStore::new();
        assert_eq!(store.record("sig-a"), NonceOutcome::Fresh);
    }

    #[test]
    fn second_seen_within_window_is_replay() {
        let store = InMemoryNonceStore::new();
        assert_eq!(store.record("sig-a"), NonceOutcome::Fresh);
        assert_eq!(store.record("sig-a"), NonceOutcome::Replay);
    }

    #[test]
    fn distinct_nonces_do_not_collide() {
        let store = InMemoryNonceStore::new();
        assert_eq!(store.record("sig-a"), NonceOutcome::Fresh);
        assert_eq!(store.record("sig-b"), NonceOutcome::Fresh);
    }

    #[test]
    fn nonce_is_accepted_again_after_ttl_elapses() {
        let store = InMemoryNonceStore::with_ttl(Duration::from_millis(50));
        assert_eq!(store.record("sig-a"), NonceOutcome::Fresh);
        sleep(Duration::from_millis(75));
        assert_eq!(store.record("sig-a"), NonceOutcome::Fresh);
    }
}
