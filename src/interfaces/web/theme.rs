//! A public, in-memory stylesheet endpoint; the on-disk catalogue is never served.
use crate::{
    application::{ports::theme_ports::PreparedTheme, services::theme_service::ThemeService},
    domain::repositories::settings_repository::SettingsRepository,
};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Response, StatusCode, header},
    routing::get,
};
use std::sync::Arc;

pub fn mount<S: SettingsRepository, T: Clone + Send + Sync + 'static>(
    router: Router<T>,
    service: Arc<ThemeService<S>>,
) -> Router<T> {
    // Explicit namespace routes prevent the parent's SPA fallback from handling
    // the bare /themes path, without Axum's nested fallback route conflicts.
    router
        .route(
            "/themes/active.css",
            get(move |headers: HeaderMap, method: Method| {
                let theme = service.active();
                async move { response(&theme, &headers, &method) }
            }),
        )
        .route("/themes", get(|| async { StatusCode::NOT_FOUND }))
        .route("/themes/", get(|| async { StatusCode::NOT_FOUND }))
        .route("/themes/{*path}", get(|| async { StatusCode::NOT_FOUND }))
}

fn matches(headers: &HeaderMap, etag: &str) -> bool {
    let normalized = etag.strip_prefix("W/").unwrap_or(etag);
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|raw| {
            raw.split(',')
                .map(str::trim)
                .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == normalized)
        })
}
fn response(theme: &PreparedTheme, headers: &HeaderMap, method: &Method) -> Response<Body> {
    let cached = matches(headers, &theme.etag);
    let body = if cached || *method == Method::HEAD {
        // An unknown size avoids a spurious Content-Length: 0 on 304 and lets
        // compression use HEAD's representation length before removing it.
        Body::from_stream(futures::stream::empty::<
            Result<bytes::Bytes, std::convert::Infallible>,
        >())
    } else {
        Body::from(theme.css.clone())
    };
    let mut response = Response::builder()
        .status(if cached {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "text/css; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, &theme.etag)
        .header(header::VARY, "Accept-Encoding")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if !cached {
        response = response.header(header::CONTENT_LENGTH, theme.css.len());
    }
    response
        .body(body)
        .expect("prepared theme headers are valid ASCII")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::middleware::has_representation_body;

    #[tokio::test]
    async fn existing_layers_compress_css_without_changing_content_or_validators() {
        use axum::http::Request;
        use tokio::io::AsyncReadExt;
        use tower::ServiceExt;
        use tower_http::compression::predicate::{DefaultPredicate, Predicate, SizeAbove};

        for css in [
            ":root {}".to_owned(),
            ":root { --color-accent: red; }".repeat(3),
            ":root { --color-accent: red; }".repeat(100),
        ] {
            let theme = Arc::new(
                crate::infrastructure::services::theme_assets::prepare_theme("default".into(), css),
            );
            let active = theme.clone();
            // Regression pin: exercise compression outside the route, as in production;
            // compressed bytes must decode to CSS and share the weak validator.
            // The web layer uses the default 32-byte threshold; the global layer
            // uses 256 bytes. Its MIME/download exclusions do not apply to CSS.
            let app = Router::new()
                .route(
                    "/themes/active.css",
                    get(move |headers: HeaderMap, method: Method| {
                        let theme = active.clone();
                        async move { response(&theme, &headers, &method) }
                    }),
                )
                .layer(
                    tower_http::compression::CompressionLayer::new()
                        .quality(tower_http::CompressionLevel::Precise(4))
                        .compress_when(DefaultPredicate::new().and(has_representation_body)),
                )
                .layer(
                    tower_http::compression::CompressionLayer::new()
                        .quality(tower_http::CompressionLevel::Precise(4))
                        .compress_when(SizeAbove::new(256).and(has_representation_body)),
                );
            for (encoding, expected_coding) in [
                ("", None),
                ("br", Some("br")),
                ("gzip", Some("gzip")),
                ("identity", None),
                ("gzip;q=0", None),
                ("br;q=0, gzip;q=1", Some("gzip")),
                ("gzip;q=0, br;q=1", Some("br")),
                ("br;q=0.5, gzip;q=1", Some("gzip")),
                ("br, gzip", Some("br")),
                ("identity;q=1, gzip;q=0.5", None),
                // tower-http ignores wildcard encodings and falls back to identity,
                // including when identity is refused. Keep the shared HTTP policy.
                ("*", None),
                ("*;q=0", None),
                ("identity;q=0, gzip;q=0, br;q=0", None),
            ] {
                // Regression pin: an empty HEAD body must not hide the CSS length
                // from compression and advertise an identity length for a gzip GET.
                for method in ["GET", "HEAD"] {
                    let res = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method(method)
                                .uri("/themes/active.css")
                                .header(header::ACCEPT_ENCODING, encoding)
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(res.status(), StatusCode::OK);
                    assert_eq!(res.headers()[header::ETAG], theme.etag);
                    assert_eq!(
                        res.headers()[header::CONTENT_TYPE],
                        "text/css; charset=utf-8"
                    );
                    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-cache");
                    assert_eq!(res.headers()[header::VARY], "Accept-Encoding");
                    assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
                    let coding = res.headers().get(header::CONTENT_ENCODING).cloned();
                    let expected = if theme.css.len() >= 32 {
                        expected_coding
                    } else {
                        None
                    };
                    assert_eq!(
                        coding.as_ref().and_then(|h| h.to_str().ok()),
                        expected,
                        "{method}, Accept-Encoding: {encoding}"
                    );
                    if coding.is_some() {
                        assert!(!res.headers().contains_key(header::CONTENT_LENGTH));
                    } else {
                        assert_eq!(
                            res.headers()[header::CONTENT_LENGTH],
                            theme.css.len().to_string()
                        );
                    }
                    let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
                        .await
                        .unwrap();
                    if method == "HEAD" {
                        assert!(body.is_empty());
                        continue;
                    }
                    let mut decoded = Vec::new();
                    match coding.as_ref().and_then(|h| h.to_str().ok()) {
                        Some("gzip") => {
                            async_compression::tokio::bufread::GzipDecoder::new(body.as_ref())
                                .read_to_end(&mut decoded)
                                .await
                                .unwrap();
                        }
                        Some("br") => {
                            async_compression::tokio::bufread::BrotliDecoder::new(body.as_ref())
                                .read_to_end(&mut decoded)
                                .await
                                .unwrap();
                        }
                        None => decoded.extend_from_slice(&body),
                        other => panic!("Unexpected encoding: {other:?}"),
                    }
                    assert_eq!(decoded.as_slice(), theme.css.as_ref());
                }
                for (method, validator) in [
                    ("GET", theme.etag.clone()),
                    ("HEAD", theme.etag.clone()),
                    ("GET", "\"different\"".into()),
                ] {
                    let res = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method(method)
                                .uri("/themes/active.css")
                                .header(header::ACCEPT_ENCODING, encoding)
                                .header(header::IF_NONE_MATCH, &validator)
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    let cached = validator == theme.etag;
                    assert_eq!(
                        res.status(),
                        if cached {
                            StatusCode::NOT_MODIFIED
                        } else {
                            StatusCode::OK
                        }
                    );
                    if cached {
                        assert_eq!(res.headers()[header::ETAG], theme.etag);
                        assert_eq!(res.headers()[header::VARY], "Accept-Encoding");
                        assert!(!res.headers().contains_key(header::CONTENT_ENCODING));
                        assert!(!res.headers().contains_key(header::CONTENT_LENGTH));
                        assert!(
                            axum::body::to_bytes(res.into_body(), 1024)
                                .await
                                .unwrap()
                                .is_empty()
                        );
                    }
                }
            }
        }
    }
    #[tokio::test]
    async fn validators_and_head_preserve_representation_headers() {
        let theme = crate::infrastructure::services::theme_assets::prepare_theme(
            "default".into(),
            ":root { --color-accent: red; }".into(),
        );
        let first = response(&theme, &HeaderMap::new(), &Method::GET);
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        let head = response(&theme, &HeaderMap::new(), &Method::HEAD);
        assert_eq!(head.headers(), first.headers());
        assert!(
            axum::body::to_bytes(head.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
        for validator in [
            theme.etag.clone(),
            theme.etag.trim_start_matches("W/").into(),
            format!("\"other\", {}", theme.etag),
            "*".into(),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_NONE_MATCH, validator.parse().unwrap());
            let cached = response(&theme, &headers, &Method::GET);
            assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
            assert!(
                axum::body::to_bytes(cached.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
