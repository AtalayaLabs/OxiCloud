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

use axum::{
    Router,
    extract::{MatchedPath, Request, State},
    http::header,
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::get,
};
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

const REQUEST_DURATION: &str = "oxicloud_http_request_duration_seconds";

fn recorder_builder() -> Result<PrometheusBuilder, BuildError> {
    // Explicit buckets produce aggregatable Prometheus histograms, rather
    // than the exporter's default per-process summaries. Seconds throughout.
    PrometheusBuilder::new().set_buckets_for_metric(
        Matcher::Full(REQUEST_DURATION.to_owned()),
        &[
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
        ],
    )
}

/// Wrap the completed main router once, only when metrics are enabled.
///
/// Duration ends when the inner service returns response headers: streamed
/// downloads, SSE and WebSocket lifetimes are not included. Requests cancelled
/// before a response is produced do not contribute a sample. This layer neither
/// polls nor buffers the body and leaves existing access/audit logs unchanged.
pub fn instrument_requests<S>(router: Router<S>, enabled: bool) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    if enabled {
        router.layer(from_fn(record_request_duration))
    } else {
        // No clock reads, label allocation or additional layer when disabled.
        router
    }
}

async fn record_request_duration(request: Request, next: Next) -> Response {
    let start = Instant::now();
    // MatchedPath is the registered template, including nested prefixes. Never
    // fall back to the URI: filenames, share tokens and arbitrary 404 paths are
    // both sensitive and unbounded. Clone its Arc before Next consumes request.
    let route = request.extensions().get::<MatchedPath>().cloned();
    let method = metric_method(request.method().as_str());
    let response = next.run(request).await;
    let duration = start.elapsed().as_secs_f64();
    metrics::histogram!(
        REQUEST_DURATION,
        "method" => method,
        "route" => route.as_ref().map_or("unmatched", MatchedPath::as_str).to_owned(),
        "status" => response.status().as_str().to_owned(),
    )
    .record(duration);
    response
}

fn metric_method(method: &str) -> &'static str {
    // HTTP allows arbitrary extension tokens. Keep known HTTP/DAV methods
    // useful without letting a client create one time series per invented verb.
    match method {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "CONNECT" => "CONNECT",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "PATCH" => "PATCH",
        "PROPFIND" => "PROPFIND",
        "PROPPATCH" => "PROPPATCH",
        "MKCOL" => "MKCOL",
        "COPY" => "COPY",
        "MOVE" => "MOVE",
        "LOCK" => "LOCK",
        "UNLOCK" => "UNLOCK",
        "REPORT" => "REPORT",
        "MKCALENDAR" => "MKCALENDAR",
        "ACL" => "ACL",
        "SEARCH" => "SEARCH",
        _ => "OTHER",
    }
}

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
        recorder_builder()?
            .install_recorder()
            .map_err(|err| -> BoxError {
                format!("failed to install Prometheus recorder: {err}").into()
            })?;
    metrics::describe_histogram!(
        REQUEST_DURATION,
        metrics::Unit::Seconds,
        "Time until the main HTTP service returns response headers; excludes response body streaming"
    );

    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(handle.clone());

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

    // install_recorder does not start the exporter's upkeep task. Drain raw
    // histogram samples even if Prometheus stops scraping, or they grow for
    // the lifetime of the process. Keep upkeep alive if the listener fails:
    // the main server still records requests through the global recorder.
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            handle.run_upkeep();
        }
    });

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, Bytes, to_bytes},
        http::{Method, StatusCode},
        routing::any,
    };
    use metrics_exporter_prometheus::PrometheusRecorder;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tower::ServiceExt;

    fn recorder() -> PrometheusRecorder {
        recorder_builder().unwrap().build_recorder()
    }

    async fn request(app: Router, method: &str, uri: &str) -> Response {
        app.oneshot(
            Request::builder()
                .method(Method::from_bytes(method.as_bytes()).unwrap())
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    fn assert_count(scrape: &str, method: &str, route: &str, status: u16, count: u64) {
        let lines: Vec<_> = scrape
            .lines()
            .filter(|line| {
                line.starts_with(&format!("{REQUEST_DURATION}_count{{"))
                    && line.contains(&format!("method=\"{method}\""))
                    && line.contains(&format!("route=\"{route}\""))
                    && line.contains(&format!("status=\"{status}\""))
            })
            .collect();
        assert_eq!(lines.len(), 1, "{scrape}");
        assert!(lines[0].ends_with(&format!(" {count}")), "{scrape}");
    }

    // Local recorders are thread-local. Current-thread runtimes keep isolation
    // across awaits without installing global state or serializing the suite.
    #[tokio::test(flavor = "current_thread")]
    async fn nested_templates_hide_ids_queries_and_wildcard_values() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = instrument_requests(
            Router::new().nest(
                "/api",
                Router::new()
                    .route("/files/{id}", get(|| async { "file" }))
                    .route("/dav/{*path}", any(|| async { StatusCode::MULTI_STATUS })),
            ),
            true,
        );
        for path in [
            "/api/files/alice?token=secret-one",
            "/api/files/bob?token=secret-two",
        ] {
            assert_eq!(
                request(app.clone(), "GET", path).await.status(),
                StatusCode::OK
            );
            recorder.handle().run_upkeep();
        }
        for path in ["/api/dav/alice/private.txt", "/api/dav/bob/secrets.txt"] {
            assert_eq!(
                request(app.clone(), "PROPFIND", path).await.status(),
                StatusCode::MULTI_STATUS
            );
        }
        let scrape = recorder.handle().render();
        // Regression pin: emitting actual IDs/paths creates unbounded, sensitive
        // labels; nested routes must retain their full registered template.
        assert_count(&scrape, "GET", "/api/files/{id}", 200, 2);
        assert_count(&scrape, "PROPFIND", "/api/dav/{*path}", 207, 2);
        for secret in ["alice", "bob", "secret", "private.txt", "token"] {
            assert!(!scrape.contains(secret), "{scrape}");
        }
        assert!(scrape.contains(&format!("# TYPE {REQUEST_DURATION} histogram")));
        assert!(scrape.contains("le=\"0.005\""));
        assert!(scrape.contains("le=\"+Inf\""));
        assert!(!scrape.contains("quantile="));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_paths_and_extension_methods_share_bounded_labels() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = instrument_requests(Router::new(), true);
        for (method, path) in [("ATTACK_ONE", "/secret-one"), ("ATTACK_TWO", "/secret-two")] {
            assert_eq!(
                request(app.clone(), method, path).await.status(),
                StatusCode::NOT_FOUND
            );
        }
        let scrape = recorder.handle().render();
        // Both unknown method strings and unmatched URIs are attacker-controlled.
        assert_count(&scrape, "OTHER", "unmatched", 404, 2);
        assert!(!scrape.contains("ATTACK"));
        assert!(!scrape.contains("secret"));
        assert_eq!(
            scrape
                .lines()
                .filter(|line| line.starts_with(&format!("{REQUEST_DURATION}_count{{")))
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deployment_prefix_redirect_and_outer_fallback_are_included() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = instrument_requests(
            Router::new()
                .route(
                    "/cloud/",
                    get(|| async { axum::response::Redirect::permanent("/cloud") }),
                )
                .nest(
                    "/cloud",
                    Router::new().route("/files/{id}", get(|| async { "file" })),
                ),
            true,
        );
        assert_eq!(
            request(app.clone(), "GET", "/cloud/files/secret")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            request(app.clone(), "GET", "/cloud/").await.status(),
            StatusCode::PERMANENT_REDIRECT
        );
        assert_eq!(
            request(app, "GET", "/outside-prefix").await.status(),
            StatusCode::NOT_FOUND
        );
        let scrape = recorder.handle().render();
        // Regression pin: wrapping the inner router misses the redirect and
        // outside-prefix fallback added by the final base-path composition.
        assert_count(&scrape, "GET", "/cloud/files/{id}", 200, 1);
        assert_count(&scrape, "GET", "/cloud/", 308, 1);
        assert_count(&scrape, "GET", "unmatched", 404, 1);
        assert!(!scrape.contains("secret"));
        assert!(!scrape.contains("outside-prefix"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn includes_middleware_rejections_and_method_not_allowed_once() {
        async fn rejected_handler() -> StatusCode {
            panic!("rejected request reached handler")
        }
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let protected = Router::new()
            .route("/private/{id}", get(rejected_handler))
            .layer(from_fn(|_: Request, _: Next| async {
                StatusCode::UNAUTHORIZED
            }));
        let app = instrument_requests(
            Router::new()
                .merge(protected)
                .route("/read-only", get(|| async { StatusCode::OK }))
                .route(
                    "/failure",
                    get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
                ),
            true,
        );
        assert_eq!(
            request(app.clone(), "GET", "/private/123").await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(app.clone(), "POST", "/read-only").await.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            request(app, "GET", "/failure").await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let scrape = recorder.handle().render();
        // Wrapping only successful handlers would silently omit these failures.
        assert_count(&scrape, "GET", "/private/{id}", 401, 1);
        assert_count(&scrape, "POST", "/read-only", 405, 1);
        assert_count(&scrape, "GET", "/failure", 503, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn records_before_streaming_without_polling_or_changing_body() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let polled = Arc::new(AtomicBool::new(false));
        let body_polled = polled.clone();
        let app = instrument_requests(
            Router::new().route(
                "/stream",
                get(move || {
                    let polled = body_polled.clone();
                    async move {
                        Body::from_stream(futures::stream::once(async move {
                            polled.store(true, Ordering::SeqCst);
                            Ok::<_, std::io::Error>(Bytes::from_static(b"streamed content"))
                        }))
                    }
                }),
            ),
            true,
        );
        let response = request(app, "GET", "/stream").await;
        let before_body = recorder.handle().render();
        // A streaming download must produce a header-latency sample immediately;
        // collecting it must not consume/buffer the body or wait for its end.
        assert!(!polled.load(Ordering::SeqCst));
        assert_count(&before_body, "GET", "/stream", 200, 1);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "streamed content"
        );
        assert!(polled.load(Ordering::SeqCst));
        assert_eq!(recorder.handle().render(), before_body);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duration_is_in_seconds_and_includes_handler_wait() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let delay = std::time::Duration::from_millis(10);
        let app = instrument_requests(
            Router::new().route(
                "/wait",
                get(move || async move {
                    tokio::time::sleep(delay).await;
                    StatusCode::OK
                }),
            ),
            true,
        );
        let before = Instant::now();
        assert_eq!(request(app, "GET", "/wait").await.status(), StatusCode::OK);
        let total_seconds = before.elapsed().as_secs_f64();
        let scrape = recorder.handle().render();
        let sum: f64 = scrape
            .lines()
            .find(|line| line.starts_with(&format!("{REQUEST_DURATION}_sum{{")))
            .unwrap()
            .rsplit_once(' ')
            .unwrap()
            .1
            .parse()
            .unwrap();
        // Compare against the enclosing observed interval, not a host-speed
        // deadline. Milliseconds in a seconds metric would exceed this bound.
        assert!(sum >= delay.as_secs_f64(), "{scrape}");
        assert!(sum <= total_seconds, "{scrape}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_response_does_not_invent_a_sample() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = instrument_requests(
            Router::new().route("/pending", get(std::future::pending::<StatusCode>)),
            true,
        );
        let mut future = Box::pin(request(app, "GET", "/pending"));
        assert!(futures::poll!(future.as_mut()).is_pending());
        drop(future);
        assert!(!recorder.handle().render().contains(REQUEST_DURATION));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disabled_instrumentation_emits_nothing_even_with_a_recorder() {
        let recorder = recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = instrument_requests(Router::new().route("/ok", get(|| async { "ok" })), false);
        let response = request(app, "GET", "/ok").await;
        assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), "ok");
        // Configuration must omit the layer, not merely rely on a missing global
        // recorder: embedding/tests may already have one installed.
        assert!(!recorder.handle().render().contains(REQUEST_DURATION));
    }
}
