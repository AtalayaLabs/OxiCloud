//! Prometheus session-liveness gauges — periodic polling of
//! `auth.sessions` to publish three gauges the `/metrics` scraper
//! reads:
//!
//! - `oxicloud_sessions_online` — non-revoked rows observed in the
//!   last [`ONLINE_WINDOW`](crate::application::dtos::session_dto::ONLINE_WINDOW).
//!   **Per-session count**, not per-user — one user with three
//!   devices contributes three.
//! - `oxicloud_sessions_online_users` — DISTINCT `user_id` behind
//!   those online sessions. The multi-device factor is exactly
//!   `sessions_online / sessions_online_users`.
//! - `oxicloud_sessions_total_non_revoked` — long-tail total,
//!   including mobile clients still holding a refresh token they
//!   haven't used in weeks. Useful sanity signal on the dashboard.
//!
//! **Naming — "online" vs "active".** The word "active" is already
//! spoken for by the session *lifecycle* (Active | Expired |
//! Revoked in the admin panel). Presence (recently-seen) is
//! orthogonal and uses "online" throughout the UI, DTO
//! (`SessionSummaryDto::is_online`), and these gauges — so a
//! dashboard graph and a per-row green-dot badge have the same
//! label root. Terminology decided 2026-08-18; see
//! `docs/plan/sessions.md`.
//!
//! **Cadence.** Poller ticks every [`POLL_INTERVAL`] (30 s). Three
//! `COUNT(*)` reads on the maintenance pool per tick — negligible
//! load on tens-of-thousands-of-rows tables thanks to the partial
//! index `idx_sessions_last_seen_at` (partial on `revoked = FALSE`,
//! which every query below filters on).
//!
//! **When it runs.** Spawned from DI only when auth is enabled AND
//! `OXICLOUD_METRICS_LISTEN` is set (recorder installed). Without
//! the recorder, `metrics::gauge!(...)` is a no-op — spawning
//! anyway would still hit PG every 30 s for values nobody reads.
//!
//! See `docs/plan/sessions.md` for the full design.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::application::dtos::session_dto::ONLINE_WINDOW;

/// Poll cadence. Matches the [`LastSeenTracker`](super::last_seen_tracker)
/// flush cadence so the gauges converge one tick after the tracker
/// flushes — no need to sync the two.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the session-liveness poller. Detached — the task lives
/// for the runtime's lifetime; there's no mid-process reason to
/// stop reporting gauges.
///
/// Emits an initial poll on spawn so the very first `/metrics`
/// scrape after boot returns real values instead of the recorder's
/// zero-initialised default.
pub fn spawn(maintenance_pool: Arc<PgPool>) {
    tokio::spawn(async move {
        // Immediate first tick — a scraper hitting `/metrics` in
        // the first 30 s otherwise sees `oxicloud_sessions_online
        // 0` even on a busy server. Warmup query is cheap.
        if let Err(err) = poll_once(&maintenance_pool).await {
            tracing::warn!(
                target: "oxicloud::sessions",
                error = %err,
                "initial session-liveness poll failed",
            );
        }

        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the first tick — `interval` fires immediately on
        // creation and we've already done the warmup above.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            if let Err(err) = poll_once(&maintenance_pool).await {
                tracing::warn!(
                    target: "oxicloud::sessions",
                    error = %err,
                    "session-liveness poll failed; keeping last-known gauge values",
                );
            }
        }
    });
    tracing::info!(
        target: "oxicloud::sessions",
        poll_interval_secs = POLL_INTERVAL.as_secs(),
        online_window_secs = ONLINE_WINDOW.as_secs(),
        "📊 session-liveness gauges spawned",
    );
}

/// One poll cycle. Three lightweight `COUNT` reads → three gauge
/// updates. Errors propagate to the caller (loop logs + retries
/// next tick; gauges keep their last-known value in the interim,
/// which is the honest thing to publish — a temporary PG blip is
/// not a "sessions dropped to zero" event).
async fn poll_once(pool: &PgPool) -> Result<(), sqlx::Error> {
    // NOTE: `ONLINE_WINDOW` is a Duration; PG expects the interval
    // in seconds via `make_interval` (portable across sqlx driver
    // versions). Casting once at bind time is cheaper than an
    // `INTERVAL '$1 seconds'` string interp and keeps the query
    // parameterised.
    let online_secs: f64 = ONLINE_WINDOW.as_secs_f64();

    let online: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM auth.sessions
        WHERE revoked = FALSE
          AND last_seen_at > NOW() - make_interval(secs => $1)
        "#,
    )
    .bind(online_secs)
    .fetch_one(pool)
    .await?;

    let online_users: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT user_id) FROM auth.sessions
        WHERE revoked = FALSE
          AND last_seen_at > NOW() - make_interval(secs => $1)
        "#,
    )
    .bind(online_secs)
    .fetch_one(pool)
    .await?;

    let total_non_revoked: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM auth.sessions WHERE revoked = FALSE
        "#,
    )
    .fetch_one(pool)
    .await?;

    // metrics-exporter-prometheus takes f64 gauges; the raw COUNT
    // fits into f64 precisely up to 2^53, well past any realistic
    // session-row count. `describe_gauge!` is called once at first
    // emission and cached in the recorder — the second/third tick
    // just updates the value.
    metrics::describe_gauge!(
        "oxicloud_sessions_online",
        "Non-revoked sessions observed in the last ONLINE_WINDOW."
    );
    metrics::gauge!("oxicloud_sessions_online").set(online as f64);

    metrics::describe_gauge!(
        "oxicloud_sessions_online_users",
        "Distinct users behind sessions observed in the last ONLINE_WINDOW."
    );
    metrics::gauge!("oxicloud_sessions_online_users").set(online_users as f64);

    metrics::describe_gauge!(
        "oxicloud_sessions_total_non_revoked",
        "Total non-revoked sessions regardless of last-seen recency."
    );
    metrics::gauge!("oxicloud_sessions_total_non_revoked").set(total_non_revoked as f64);

    tracing::debug!(
        target: "oxicloud::sessions",
        online,
        online_users,
        total_non_revoked,
        "session-liveness gauges updated",
    );
    Ok(())
}
