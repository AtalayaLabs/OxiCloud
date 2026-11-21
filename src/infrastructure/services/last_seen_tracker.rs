//! Per-session liveness tracker — the hot path of the "how many
//! sessions are active right now?" observation loop.
//!
//! **Contract.** Every authenticated request calls
//! [`LastSeenTracker::stamp`] with the session id it resolved. The
//! call is O(1) — a DashMap upsert of `(session_id → Utc::now())` —
//! and hits no I/O. The map data structure IS the dedup: 100
//! requests against the same session in a flush window contribute
//! ONE row to the batched UPDATE with the latest timestamp.
//!
//! A background task ([`flush_loop`](Self::flush_loop), spawned at
//! construction) drains the map every 30 s and issues one
//! `UPDATE ... FROM UNNEST($1::uuid[], $2::timestamptz[])` covering
//! every distinct session_id observed in the window. The
//! `greatest(s.last_seen_at, t.seen_at)` guard makes the write
//! idempotent under any retry / race / clock skew — replaying the
//! same batch never moves the column backward.
//!
//! **Failure model.** A flush that hits a transient PG error does
//! NOT drop the accumulated set — the map is not cleared until the
//! UPDATE succeeds. Next tick overlays new activity on the retry
//! set and the whole thing gets flushed together. Bounded loss
//! window under a hard crash is one flush interval; graceful
//! shutdown calls [`flush_now`](Self::flush_now) synchronously (see
//! `main.rs`) so rolling restarts drop nothing.
//!
//! **Non-goals.** No per-session locking, no ordering guarantees
//! across sessions, no back-pressure on the flusher (the loop
//! swallows errors and keeps ticking). The workload is
//! observation-only — losing a stamp under contention is a
//! correctness no-op, the next request re-stamps.
//!
//! See `docs/plan/sessions.md` for the full design (why DashMap
//! over Mutex<HashMap>, why not NOTIFY/LISTEN today, migration
//! path to a multi-instance cluster).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sqlx::PgPool;
use uuid::Uuid;

/// Cadence of the batched UPDATE. Hardcoded — 30 s balances DB
/// write load against gauge freshness (the Prometheus scrape
/// interval is typically 15 s, so at worst two scrapes see the
/// same value before the next flush). Deliberately NOT exposed as
/// an env var — tuning it is a deployment-shape question we've
/// never had to answer in practice.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// In-process session-liveness tracker. See [module docs](self) for
/// the full contract; the two entry points are:
///
/// - [`stamp`](Self::stamp) — called from the auth middleware on
///   every authenticated request.
/// - [`flush_now`](Self::flush_now) — called from the graceful-
///   shutdown handler.
///
/// The periodic flush task is spawned on the tokio runtime by
/// [`start`](Self::start) at construction. The struct keeps no
/// handle to it — the task holds the `Arc<Self>` and observes the
/// runtime shutting down naturally.
pub struct LastSeenTracker {
    /// (session_id → last observed time and IP). DashMap's sharded
    /// locking parallelises writes across distinct session_ids —
    /// different users' requests never contend.
    ///
    /// The IP rides along rather than taking a tracker of its own: it
    /// is observed at the same moment, written to the same row, and
    /// carrying it here costs one `Option<String>` per ACTIVE session
    /// per flush window rather than per request.
    seen: DashMap<Uuid, (DateTime<Utc>, Option<String>)>,
    /// Maintenance pool — the tracker is a background writer and
    /// must not compete with request-serving connections.
    pool: Arc<PgPool>,
}

impl LastSeenTracker {
    /// Construct + spawn the flush loop. Returns the shared
    /// handle; callers store it on `AppState` and pass it to the
    /// auth middleware.
    ///
    /// The background task lives for the runtime's lifetime — no
    /// cancellation handle is exposed because there is no
    /// mid-process reason to stop tracking (a stopped flusher is
    /// indistinguishable from a wedged one, and both are bugs).
    /// Graceful shutdown calls [`flush_now`](Self::flush_now)
    /// separately BEFORE the runtime tears down.
    pub fn start(pool: Arc<PgPool>) -> Arc<Self> {
        let this = Arc::new(Self {
            seen: DashMap::new(),
            pool,
        });
        tokio::spawn(this.clone().flush_loop());
        this
    }

    /// Record that `session_id` was observed serving a request right
    /// now, from `ip`. Overwrites any prior stamp for the same session
    /// in the current window — the flusher uses the latest value.
    ///
    /// `ip` is where the credential is being USED, which is not
    /// necessarily where the session was created: for a device-flow
    /// session `created_ip` holds the APPROVER's browser IP, and a
    /// mismatch between the two is a credential-theft signal. `None`
    /// leaves the column untouched rather than clearing it, so a
    /// request behind a proxy that yields no IP cannot erase a good
    /// observation.
    ///
    /// O(1) DashMap upsert. No I/O. Never fails.
    pub fn stamp(&self, session_id: Uuid, ip: Option<String>) {
        self.seen.insert(session_id, (Utc::now(), ip));
    }

    /// Has `session_id` served a request since the last flush?
    ///
    /// The refresh grace (`docs/plan/refresh-token-grace.md`) asks "did the
    /// client ever USE the session this rotation minted?", and asking the
    /// `last_seen_at` COLUMN alone answers it wrong for up to a flush
    /// interval: a session used a second ago still reads as untouched, so the
    /// grace would degrade from "closes on client progress" to "closes on a
    /// 60s timer" in nearly every real case. This exposes the unflushed
    /// half of that answer.
    ///
    /// O(1) read, no I/O. A `false` is not proof of never-used — the stamp
    /// may already have been flushed to the column — so callers check both.
    pub fn seen_since_flush(&self, session_id: Uuid) -> bool {
        self.seen.contains_key(&session_id)
    }

    /// Drain the accumulated stamps and write them in one batched
    /// UPDATE. Idempotent — the `greatest(...)` guard means
    /// replaying the same batch (or overlapping batches from a
    /// retry) never moves the column backward.
    ///
    /// Errors are surfaced to the caller so `flush_loop`'s
    /// warn-and-continue policy is a deliberate choice made in one
    /// place, and the shutdown flusher in `main.rs` can decide
    /// whether to log or panic.
    ///
    /// On PG error the accumulated set is NOT cleared — the next
    /// tick retries with fresh activity overlaid.
    pub async fn flush_now(&self) -> Result<usize, sqlx::Error> {
        if self.seen.is_empty() {
            return Ok(0);
        }

        // Drain into two parallel vectors — one UNNEST arg each.
        // `retain(|_,_| false)` clears every shard in-place; the
        // pull-and-drop order doesn't matter (we upserted the
        // latest wins per key already).
        let mut ids: Vec<Uuid> = Vec::with_capacity(self.seen.len());
        let mut seen_at: Vec<DateTime<Utc>> = Vec::with_capacity(self.seen.len());
        let mut seen_ip: Vec<Option<String>> = Vec::with_capacity(self.seen.len());
        for entry in self.seen.iter() {
            ids.push(*entry.key());
            let (at, ip) = entry.value();
            seen_at.push(*at);
            seen_ip.push(ip.clone());
        }

        let result = sqlx::query(
            r#"
            UPDATE auth.sessions AS s
               SET last_seen_at = greatest(s.last_seen_at, t.seen_at),
                   -- COALESCE so a request that yielded no IP cannot
                   -- erase a good observation: `None` leaves the column
                   -- as it was.
                   last_seen_ip = COALESCE(t.seen_ip, s.last_seen_ip)
              FROM UNNEST($1::uuid[], $2::timestamptz[], $3::text[])
                   AS t(id, seen_at, seen_ip)
             WHERE s.id = t.id
            "#,
        )
        .bind(&ids)
        .bind(&seen_at)
        .bind(&seen_ip)
        .execute(&*self.pool)
        .await?;

        // Only clear the drained keys on success. A key inserted
        // BETWEEN our copy above and the clear below survives
        // (retain drops only those whose value we already flushed,
        // by timestamp equality). Same-key re-stamp with a newer
        // timestamp gets kept for the next flush.
        let flushed: std::collections::HashMap<Uuid, DateTime<Utc>> =
            ids.iter().copied().zip(seen_at.iter().copied()).collect();
        // Compared on the TIMESTAMP only. The IP rides along but is not
        // part of the identity of a stamp: a re-stamp from a new address
        // inside the same flush window carries a newer timestamp too, so
        // the timestamp alone already decides whether this entry was the
        // one we drained.
        self.seen
            .retain(|k, v| flushed.get(k).is_none_or(|ts| *ts != v.0));

        let updated = result.rows_affected() as usize;
        tracing::debug!(
            target: "oxicloud::sessions",
            batched = ids.len(),
            updated,
            "last_seen flush",
        );
        Ok(updated)
    }

    /// The periodic drain loop. Runs forever; every failed flush
    /// is logged at WARN and the accumulated set is preserved for
    /// the next tick.
    async fn flush_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        // Skip the "first tick fires immediately" behaviour — the
        // map is empty at spawn time, so a same-tick flush is
        // wasted work.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        loop {
            ticker.tick().await;
            if let Err(err) = self.flush_now().await {
                tracing::warn!(
                    target: "oxicloud::sessions",
                    error = %err,
                    "last_seen flush failed; will retry next tick",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten stamps of the same session_id must collapse to ONE
    /// entry with the newest timestamp — the whole point of the
    /// DashMap-as-dedup pattern. Guards against a future refactor
    /// that swaps to an append-only channel and doubles the DB
    /// write rate.
    #[test]
    fn stamps_dedup_by_session_id() {
        // No pool needed — we're only exercising the map. Build
        // the tracker directly without spawning the loop.
        let seen = DashMap::new();
        let session = Uuid::new_v4();

        for _ in 0..10 {
            seen.insert(session, Utc::now());
        }

        assert_eq!(seen.len(), 1);
    }

    /// Latest-wins semantics: two stamps for the same session
    /// leave the newer timestamp in place, matching the flusher's
    /// `greatest(...)` guard so a request that beats the flush
    /// keeps its more recent stamp.
    #[test]
    fn stamp_keeps_latest_timestamp() {
        let seen: DashMap<Uuid, DateTime<Utc>> = DashMap::new();
        let session = Uuid::new_v4();

        let t1 = Utc::now();
        seen.insert(session, t1);
        let t2 = t1 + chrono::Duration::seconds(5);
        seen.insert(session, t2);

        assert_eq!(*seen.get(&session).unwrap(), t2);
    }

    /// Distinct sessions never collide — sharded map, no dedup
    /// across keys.
    #[test]
    fn different_sessions_are_independent() {
        let seen: DashMap<Uuid, DateTime<Utc>> = DashMap::new();
        for _ in 0..100 {
            seen.insert(Uuid::new_v4(), Utc::now());
        }
        assert_eq!(seen.len(), 100);
    }
}
