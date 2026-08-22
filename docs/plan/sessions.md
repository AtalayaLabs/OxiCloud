# Session Liveness Tracking — `last_seen_at` + Prometheus

Track per-session and per-user "currently active" signals cheaply, and
expose them as Prometheus gauges so an operator (demo instance,
production) can plot concurrency over time. Also unlocks future
features that need "when was this session last used" (idle-timeout
enforcement, per-user session-limit quotas, admin dashboard freshness).

Companion doc for the design decisions covered here — narrative on the
overall session model lives in
[docs/architecture/auth-model.md](../architecture/auth-model.md).

## Purpose — what we want to see

Two distinct signals, deliberately separate:

1. **Online sessions** (`oxicloud_sessions_online`) — count of
   non-revoked `auth.sessions` rows that had a request within the last
   N minutes. One user with three devices (browser + phone + Nextcloud
   desktop) contributes **three** to this count. Useful for
   provisioning ("how many concurrent connections do I need to
   support?") and load-shape planning.

2. **Online users** (`oxicloud_sessions_online_users`) — count of
   DISTINCT `user_id` values behind those online sessions. Same
   three-device user contributes **one** to this count. Useful for
   billing shape ("how many humans are actually using the system?")
   and for the demo landing page's "N users online right now" widget.

The gap between the two IS the multi-device factor. A healthy system
where users routinely have web + desktop client should show `sessions
≈ 2 × users`. A sudden `sessions >> users × 3` is a signal — an app
that opens fresh sessions instead of reusing them, or a
credential-stuffing pattern.

### Terminology — "online" vs "active"

The admin sessions panel already has a lifecycle filter
`Active | Expired | Revoked` — where **active** means
`!revoked && !expired` (row is still usable). That's orthogonal to
"had a request lately", so both concepts fighting for the same word
was going to confuse admins reading the panel.

**Decision (Ed, 2026-08-18)** — "online" is the *presence* signal
throughout the stack:

- **UI**: green-dot badge next to each row when
  `SessionSummaryDto::is_online == true`; grey dot + "last seen X ago"
  otherwise. Lifecycle filter stays `Active | Expired | Revoked`
  unchanged.
- **DTO**: `is_online: bool` on `SessionSummaryDto`, computed
  server-side (avoids the SPA doing clock math and drifting from the
  server view). Guaranteed `false` on revoked / expired rows so an
  admin never sees "Online" on a row they just revoked.
- **Metrics**: `oxicloud_sessions_online` / `_online_users`.
- **Threshold**: single `pub const ONLINE_WINDOW` in
  `application/dtos/session_dto.rs` — DTO derivation AND gauge query
  read from the same constant so the per-row badge count and the
  gauge aggregate stay consistent by construction.

## Why the existing signals don't answer this — DECIDED

`auth.sessions.created_at` is the closest existing proxy. But it
moves on **session rotation**, not per-request:

- Sessions rotate on every silent refresh (`apiFetch`'s 401 → refresh
  path). Rotation cadence = `access_token_expiry_secs` (default
  3600, i.e. 1 h).
- So `WHERE created_at > NOW() - INTERVAL '1 hour'` catches everyone
  who refreshed in the last cycle — but a user who's actively clicking
  around for 45 min hasn't rotated yet, so their `created_at` is 45
  min old. Threshold `< 30 min` false-negatives them.
- Resolution is capped at the access-token TTL. At the recommended
  prod value of 15 min, `created_at` gives 15-min granularity. At the
  test value of 60 s it's near-real-time — but no operator wants
  to force 60 s token TTL just for observability.

So `created_at` is an OK first-pass proxy but bad enough that we
should add a dedicated column that moves per-request.

## Schema — `last_seen_at`

Migration `<TS>_sessions_last_seen_at.sql`:

```sql
ALTER TABLE auth.sessions
    ADD COLUMN last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

-- Partial index — the "active in the last N min" query is the only
-- reason to scan this column, and it always filters on revoked = FALSE.
-- Full index would double the write cost for zero read benefit.
CREATE INDEX idx_sessions_last_seen_at ON auth.sessions(last_seen_at)
    WHERE revoked = FALSE;
```

Default `NOW()` — existing rows on migration land at "just seen"
which is slightly optimistic but the alternative (NULL / epoch) makes
every historic session count as long-idle for the first N min after
deploy. `NOW()` matches "assume everyone's active" and the metric
converges to reality within one N-min bucket.

**Not in scope for this schema**: adding `last_seen_at` to
`auth.app_passwords`. Nextcloud-desktop clients authenticate through
that table; separate concern, separate PR if we want desktop-client
liveness.

## Coalescing writes — DECIDED: in-process DashMap + periodic flush

Naive shape (one `UPDATE` per authenticated request) would multiply
the DB write rate by every non-mutating request the SPA fires
(listing pages, thumbnails, delta-upload chunk PUTs). Untenable.

The pattern that scales:

- Middleware stamps `(session_id, Instant::now())` into a shared
  `DashMap<Uuid, DateTime<Utc>>`. **O(1)** per request, no I/O.
- Background task drains the map every N seconds (default 30) and
  emits ONE batched `UPDATE ... FROM UNNEST(...)` covering every
  distinct session seen in the window.
- The map data structure IS the dedup: same `session_id` → same key
  → last write wins. A session seen 100× in the window contributes
  ONE row to the batched update, with the latest timestamp.

The `UPDATE` uses `greatest()` to be idempotent under any
retry/race/clock-skew:

```sql
UPDATE auth.sessions AS s
SET last_seen_at = greatest(s.last_seen_at, t.seen_at)
FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t(id, seen_at)
WHERE s.id = t.id;
```

**Restart durability**: up to one flush interval of activity lost on
hard crash. On graceful shutdown (SIGTERM) run one final flush
synchronously before exit — zero loss on planned rolling restarts.

**Failure durability**: if the batched UPDATE fails (PG blip),
DON'T clear the DashMap; next tick retries with the accumulated set
overlaid on any new activity.

### Why not `Arc<Mutex<HashMap>>`

Every authenticated request writes. Under any concurrency (delta
uploads, thumbnail bursts, folder listing paginations firing in
parallel) a single mutex becomes the bottleneck. `DashMap`'s
per-shard locking (16-32 shards by default) parallelises writes
across distinct keys — different session_ids don't contend.

### Why not `Arc<RwLock<HashMap>>`

The workload is write-heavy (every auth'd request writes, reads only
fire in the flusher). RwLock would still serialize the writes for no
benefit.

### Why not PG `NOTIFY` / `LISTEN`

Considered. Trade-offs:

- **Pro**: cross-instance coalescing — multiple OxiCloud processes
  behind a load balancer push to one channel, single flusher owns
  writes.
- **Con**: every request pays a PG round-trip (`SELECT
  pg_notify(...)`) — ~1 ms per request vs ~50 ns for DashMap insert.
  On hot endpoints (delta chunk PUTs, thumbnails) this is
  measurable.
- **Con**: adds a persistent LISTEN connection to the pool.
- **Con**: OxiCloud is single-instance today. The multi-instance win
  doesn't apply.

**Deferred**: if OxiCloud ever grows a multi-instance deployment
story (Kubernetes, active-active behind a load balancer), migrate
the flusher to `NOTIFY`-based ingest — schema stays identical, only
the tracker implementation swaps. Document the migration path in
[Future — multi-instance](#future--multi-instance) below.

## Metric surface — Prometheus

Exposed via the existing `/metrics` endpoint (see
`src/interfaces/metrics.rs`; gated on `OXICLOUD_METRICS_LISTEN`).

### Gauges (polled every 30 s from a background task)

```
# HELP oxicloud_sessions_online Non-revoked sessions seen in the last N min.
# TYPE oxicloud_sessions_online gauge
oxicloud_sessions_online <value>

# HELP oxicloud_sessions_online_users Distinct users behind online sessions.
# TYPE oxicloud_sessions_online_users gauge
oxicloud_sessions_online_users <value>

# HELP oxicloud_sessions_total_non_revoked Total non-revoked sessions
# regardless of activity — the long tail (mobile clients still holding
# refresh tokens they haven't used in weeks).
# TYPE oxicloud_sessions_total_non_revoked gauge
oxicloud_sessions_total_non_revoked <value>
```

Queries powering each:

```sql
-- oxicloud_sessions_online
SELECT COUNT(*) FROM auth.sessions
WHERE revoked = FALSE
  AND last_seen_at > NOW() - $1::interval;   -- $1 = ONLINE_WINDOW

-- oxicloud_sessions_online_users
SELECT COUNT(DISTINCT user_id) FROM auth.sessions
WHERE revoked = FALSE
  AND last_seen_at > NOW() - $1::interval;

-- oxicloud_sessions_total_non_revoked
SELECT COUNT(*) FROM auth.sessions WHERE revoked = FALSE;
```

All three run on the maintenance pool (background polling shouldn't
compete with request-serving connections). Three lightweight
`COUNT(*)` reads every 30 s; measured cost negligible even on
tens-of-thousands-of-rows tables thanks to the partial index.

### Counters (already in-place shape)

`oxicloud_sessions_created_total` and
`oxicloud_sessions_revoked_total{reason}` — extend the existing
counter surface in the auth service (`session.created` audit line
sites) to also `metrics::counter!(...)`. Not strictly needed for the
"how many active" question but useful sanity signal on the
dashboard: rate of creation vs rate of revocation should be
approximately balanced at steady state.

## Config surface

**No new env var.** Ed's call (2026-08-18): tuning the online window
is a deployment-shape question we haven't had to answer in practice,
and adding an env knob invites premature customization. The three
knobs stay hardcoded:

- **Online window** — 5 min. Feels responsive for a demo
  landing page without over-fluctuating with tab-open-then-close
  blips. Lives at `pub const ONLINE_WINDOW` in
  `src/application/dtos/session_dto.rs`; the gauges module in
  `src/infrastructure/services/session_liveness_gauges.rs` reads
  from that constant so the DTO badge and the gauge aggregate
  can't drift.
- **Flush interval** — 30 s. Balances DB write load against gauge
  freshness (typical Prometheus scrape at 15 s sees the value
  refreshed after at most two scrapes). Lives at `FLUSH_INTERVAL`
  in `src/infrastructure/services/last_seen_tracker.rs`.
- **DashMap shard count** — crate default (16). Only worth
  surfacing when profiling shows shard contention.

## Middleware wiring

Auth extractor (`CurrentUserId`) already loads the session by
refresh-token cookie / bearer-token subject. Extend the post-load
path:

```rust
// After successful session lookup + auth checks:
state.last_seen_tracker.stamp(session.id);
```

`LastSeenTracker` shape:

```rust
pub struct LastSeenTracker {
    seen: Arc<DashMap<Uuid, DateTime<Utc>>>,
    pool: Arc<PgPool>,
}

impl LastSeenTracker {
    pub fn new(pool: Arc<PgPool>) -> Arc<Self> {
        let seen = Arc::new(DashMap::new());
        let this = Arc::new(Self { seen: seen.clone(), pool: pool.clone() });
        tokio::spawn(this.clone().flush_loop());
        this
    }

    /// Called from the auth middleware on every authenticated request.
    /// O(1); no I/O; no round-trip.
    pub fn stamp(&self, session_id: Uuid) {
        self.seen.insert(session_id, Utc::now());
    }

    /// Called from the graceful-shutdown handler.
    pub async fn flush_now(&self) -> Result<(), sqlx::Error> {
        /* drain + one batched UPDATE, same as the loop body */
    }

    async fn flush_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let _ = self.flush_now().await;   // errors logged, not propagated
        }
    }
}
```

Wired in `common/di.rs`; injected into `AppState` and referenced by
the auth extractor.

## Graceful shutdown

Hook into the existing SIGTERM handler in `main.rs` to call
`tracker.flush_now().await` before the runtime exits. Ensures rolling
restarts / container replacements don't lose the last 30 s of
liveness data.

## Admin dashboard integration (deferred, sibling PR)

Once `last_seen_at` exists, the admin sessions panel can render a
"last seen X min ago" column instead of only "created X ago". Small
follow-up — not part of this plan's scope, but the schema addition
unlocks it.

## Testing

Two hermetic units (no DB):

1. `DashMap` dedup test — insert same key 10× with different
   timestamps, drain, assert one entry with the latest timestamp.
2. `flush_now()` UPDATE-shape test — mock a `PgExecutor`, assert the
   batched UNNEST binds match the drained set. Uses `sqlx-mock` or
   equivalent.

One integration (real PG, gated on `integration_tests` cfg):

3. End-to-end — insert a session row, `stamp()` it, call `flush_now`,
   assert `last_seen_at > created_at`. Covers the whole write path
   including the `greatest()` guard.

## Phasing

1. **Migration** — add column + partial index. Ships alone; zero
   application-layer impact. Reversible via `DROP COLUMN`.  ✅ **2026-08-18**
   (`migrations/20261014000000_sessions_last_seen_at.sql`).
2. **`LastSeenTracker` service** — DashMap + flusher task. Wire into
   `AppState`. Middleware calls `stamp()`. Now `last_seen_at` moves
   in real time.  ✅ **2026-08-18** (`src/infrastructure/services/last_seen_tracker.rs`).
3. **JWT `sid` claim** — token minters carry the fresh session's
   id; auth middleware reads it and stamps with no DB round trip.
   New tokens carry it, old tokens still validate (Option → no-op).
   ✅ **2026-08-18** (extends `TokenClaims::sid` on
   `application/ports/auth_ports.rs`).
4. **Prometheus gauges** — background poller updates the three
   gauges every 30 s using the queries above. Gated on
   `OXICLOUD_METRICS_LISTEN` (no recorder → no periodic PG hits).
   ✅ **2026-08-18** (`src/infrastructure/services/session_liveness_gauges.rs`).
5. **Graceful-shutdown flush** — hook into SIGTERM handler.
   ✅ **2026-08-18** (added `shutdown_signal` +
   `with_graceful_shutdown` in `main.rs`).
6. **Session DTO exposes `last_seen_at` + `is_online`** —
   `GET /api/admin/sessions` returns both so the admin table + external
   monitors can read them. `is_online` is the server-side derivation
   against `ONLINE_WINDOW` (see terminology decision above). ✅
   **2026-08-18** (`application/dtos/session_dto.rs`).
7. **Admin dashboard "Online" column** (deferred to a sibling PR) —
   render `is_online` as a green/grey dot next to each row plus
   "last seen X ago" from `last_seen_at`.

## Future — multi-instance

If OxiCloud grows a multi-instance deployment story (K8s replicaset,
active-active behind a load balancer), the in-process DashMap becomes
insufficient — each process holds its own map, N flushers race
UPDATEs, coalescing across instances doesn't happen.

Migration path when that becomes real:

1. Keep the schema (`last_seen_at` column + partial index).
2. Replace the `LastSeenTracker::stamp()` in-process insert with a
   `SELECT pg_notify('oxicloud_session_seen', $session_id)` call.
3. Move the flusher into a **single elected worker** (leader election
   via advisory lock in PG). That worker `LISTEN`s the channel,
   accumulates into an in-process HashMap, flushes on the same 30 s
   ticker.
4. Every OxiCloud instance publishes; one instance consumes.

Trade-off: NOTIFY costs a PG round-trip per request (~1 ms) vs the
current ~50 ns DashMap insert. Only pay that when multi-instance
coalescing actually matters. Schema and gauge queries stay identical;
only the tracker implementation swaps.

## Open questions

1. ~~Definition of "active"~~ — DECIDED 2026-08-18: hardcoded to
   5 min, no env var. See [Config surface](#config-surface).
2. **Should the gauge query drop long-inactive sessions?** — a
   Nextcloud desktop client checking in every 6 h qualifies as
   "active" if the window is 6 h. Probably want two separate metrics
   (web-active < 5 min AND dav-active < 6 h) but scope creep for the
   initial ship.
3. **Multi-tab dedup on the UI side** — three browser tabs of the
   same user share ONE session (same cookies, same refresh token,
   same row). Naturally deduped at the DB layer — no action needed.
4. **App-password rows** — Nextcloud desktop / mobile clients use
   `auth.app_passwords` on top of the session model. Whether they
   should get their own `last_seen_at` (and a companion metric) is
   deferred; separate concern, separate PR.
