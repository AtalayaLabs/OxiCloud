//! DPoP replay cache — remembers `(nonce, jti)` tuples we've
//! already verified, and rejects duplicates as replays.
//!
//! Nonce-scoped by design (see `docs/plan/dpop.md` Gate 6). A `jti`
//! is only guaranteed unique WITHIN the lifetime of a nonce; a
//! naive global-`jti` cache would falsely reject the second use of
//! a `jti` value the client happened to reuse across two nonces
//! (statistically negligible for 128-bit UUIDs but semantically
//! wrong per the spec).
//!
//! Two-scope invariant, tested below:
//!   * same `jti` under DIFFERENT nonces → both accepted
//!   * same `(nonce, jti)` seen twice → second is a replay
//!
//! TTL is aligned with [`super::dpop_nonce_service::NONCE_LIFETIME`]
//! (5 minutes) — once a nonce ages out of the nonce pool it cannot
//! validate anyway, so replay-cache entries against that nonce are
//! moot the moment the outer freshness check fires. Both caches
//! bounded at ~100k entries.

use moka::sync::Cache;
use std::time::Duration;

/// Same as `NONCE_LIFETIME` — see file doc.
const REPLAY_ENTRY_TTL: Duration = Duration::from_secs(300);

/// Cap. Roughly 100 bytes/entry (two short strings + moka
/// bookkeeping) → ~10 MB ceiling under sustained attack, per plan.
const MAX_ENTRIES: u64 = 100_000;

/// In-memory nonce-scoped replay tracker.
pub struct DpopReplayCache {
    seen: Cache<(String, String), ()>,
}

impl Default for DpopReplayCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DpopReplayCache {
    pub fn new() -> Self {
        Self {
            seen: Cache::builder()
                .max_capacity(MAX_ENTRIES)
                .time_to_live(REPLAY_ENTRY_TTL)
                .build(),
        }
    }

    /// Record a fresh `(nonce, jti)` pair, returning `true` if this
    /// is the first time we've seen it (accept the proof) and
    /// `false` if we've already recorded it (replay — reject).
    ///
    /// Uses moka's atomic `entry` API so two racing verify calls
    /// for the same `(nonce, jti)` — the pathological concurrent-
    /// replay window — resolve to exactly one `true` and one
    /// `false`, never both `true`.
    pub fn check_and_record(&self, nonce: &str, jti: &str) -> bool {
        let key = (nonce.to_string(), jti.to_string());
        // `entry().or_insert_with(...)` is atomic across concurrent
        // callers; the returned `Entry` exposes `is_fresh()` to
        // distinguish "we just wrote this" from "already existed".
        let entry = self.seen.entry(key).or_insert_with(|| ());
        entry.is_fresh()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_is_accepted() {
        let c = DpopReplayCache::new();
        assert!(c.check_and_record("nonce-A", "jti-1"));
    }

    #[test]
    fn duplicate_same_scope_is_replay() {
        let c = DpopReplayCache::new();
        assert!(c.check_and_record("nonce-A", "jti-1"));
        assert!(
            !c.check_and_record("nonce-A", "jti-1"),
            "second insert of same (nonce, jti) must be flagged as replay"
        );
    }

    #[test]
    fn same_jti_different_nonces_both_accepted() {
        // Nonce-scoped invariant: `jti` uniqueness is only meaningful
        // within a single nonce lifetime. Reusing a `jti` across
        // different nonces is legitimate (the second nonce is a
        // fresh replay scope) and MUST NOT trip replay detection.
        let c = DpopReplayCache::new();
        assert!(c.check_and_record("nonce-A", "jti-1"));
        assert!(c.check_and_record("nonce-B", "jti-1"));
    }

    #[test]
    fn different_jtis_same_nonce_both_accepted() {
        let c = DpopReplayCache::new();
        assert!(c.check_and_record("nonce-A", "jti-1"));
        assert!(c.check_and_record("nonce-A", "jti-2"));
    }
}
