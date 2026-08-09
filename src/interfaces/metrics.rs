//! Prometheus `/metrics` exporter — opt-in, isolated listener.
//!
//! Enabled iff `OXICLOUD_METRICS_LISTEN` is set (see
//! [`crate::common::config::AppConfig::metrics_listen`]). When unset,
//! no recorder is installed and every `metrics::counter!(…)` call
//! across the codebase compiles to a no-op — no runtime cost, no
//! endpoint bound. When set, this module:
//!
//! 1. Installs the process-global Prometheus recorder (once — panics
//!    if called twice, so [`spawn`] MUST be a single-call site).
//! 2. Binds a fresh `axum` `Router` on the configured address exposing
//!    only `GET /metrics`. Deliberately **not merged** into the main
//!    API router — operators bind to loopback / a private interface
//!    (typical: `127.0.0.1:9090` for a node_exporter-adjacent scrape)
//!    without any auth, CSRF, or DPoP layer in front. Public exposure
//!    is an operator choice via the bind address, not an app default.
//! 3. Spawns the listener on a detached tokio task — the metrics
//!    endpoint's lifetime tracks the runtime, and a listener error
//!    logs but doesn't take the main server down.
//!
//! Counter naming follows Prometheus conventions:
//! `oxicloud_<subsystem>_<verb>_total{label=…}`. Emission is
//! **duplicated** with existing audit `tracing::info!(target: "audit", …)`
//! lines — logs stay authoritative for incident forensics; counters
//! are for rate / rollup dashboards. Never remove one when adding the
//! other.
//!
//! Starter counter surface (extend as needed):
//! * `oxicloud_dpop_verify_failed_total{reason}`
//! * `oxicloud_dpop_proof_missing_total`
//! * `oxicloud_dpop_header_missing_on_bound_session_total`
//! * `oxicloud_dpop_replay_detected_total`
//! * `oxicloud_dpop_nonce_challenges_issued_total`

use axum::{Router, extract::State, http::header, response::IntoResponse, routing::get};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;

/// Error type returned by [`spawn`]. Uses the same `Box<dyn Error>`
/// shape `main` already threads for setup failures — one less crate
/// dep (`anyhow`) and no coupling to a specific error framework.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Install the Prometheus recorder and spawn the `/metrics` listener.
///
/// Idempotent-unsafe: MUST be called at most once per process (the
/// recorder is a process-global singleton). Caller (main.rs) checks
/// `config.metrics_listen.is_some()` — no runtime guard here.
///
/// Returns immediately after `bind` succeeds; the listener runs on a
/// detached tokio task. A bind failure returns the error so main can
/// decide whether to abort (recommended) or continue without metrics.
pub async fn spawn(bind: SocketAddr) -> Result<(), BoxError> {
    let handle: PrometheusHandle =
        PrometheusBuilder::new()
            .install_recorder()
            .map_err(|err| -> BoxError {
                format!("failed to install Prometheus recorder: {err}").into()
            })?;

    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(handle);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|err| -> BoxError {
            format!("failed to bind metrics listener on {bind}: {err}").into()
        })?;
    let actual = listener.local_addr()?;
    tracing::info!(
        target: "oxicloud::metrics",
        "📊 Prometheus /metrics listening on http://{actual}/metrics",
    );

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(
                target: "oxicloud::metrics",
                "metrics listener terminated with error: {err}",
            );
        }
    });
    Ok(())
}

/// Render the current Prometheus text-format snapshot. Content-type
/// per spec: `text/plain; version=0.0.4`; scrapers parse strictly.
async fn scrape(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        handle.render(),
    )
}
