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
// Encoding is selected from precomputed bytes, so GET and HEAD agree even
// when the outer compression layer would otherwise see an empty HEAD body.
fn representation<'a>(
    theme: &'a PreparedTheme,
    headers: &HeaderMap,
) -> Option<(&'static str, &'a bytes::Bytes)> {
    let raw = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    let quality = |encoding: &str| -> f32 {
        let mut explicit = None;
        let mut wildcard = None;
        for item in raw.split(',') {
            let mut fields = item.trim().split(';');
            let name = fields.next().unwrap_or("").trim();
            let q = fields
                .next()
                .map(|q| {
                    q.trim()
                        .strip_prefix("q=")
                        .and_then(|v| v.parse::<f32>().ok())
                        .filter(|q| (0.0..=1.0).contains(q))
                        .unwrap_or(0.0)
                })
                .unwrap_or(1.0);
            if name.eq_ignore_ascii_case(encoding) {
                explicit = Some(q);
            }
            if name == "*" {
                wildcard = Some(q);
            }
        }
        explicit.unwrap_or_else(|| {
            if encoding == "identity" {
                if wildcard == Some(0.0) { 0.0 } else { 1.0 }
            } else {
                wildcard.unwrap_or(0.0)
            }
        })
    };
    // Identity's implicit q=1 does not override an explicitly accepted encoding.
    let br = quality("br");
    let gzip = quality("gzip");
    if br > 0.0 && br >= gzip {
        Some(("br", &theme.brotli))
    } else if gzip > 0.0 {
        Some(("gzip", &theme.gzip))
    } else if quality("identity") > 0.0 {
        Some(("identity", &theme.css))
    } else {
        None
    }
}
fn response(theme: &PreparedTheme, headers: &HeaderMap, method: &Method) -> Response<Body> {
    let Some((encoding, bytes)) = representation(theme, headers) else {
        return Response::builder()
            .status(StatusCode::NOT_ACCEPTABLE)
            .header(header::VARY, "Accept-Encoding")
            .body(Body::empty())
            .expect("static response");
    };
    let cached = matches(headers, &theme.etag);
    let body = if cached || *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes.clone())
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
        .header(header::CONTENT_ENCODING, encoding)
        .header(header::VARY, "Accept-Encoding")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if !cached {
        response = response.header(header::CONTENT_LENGTH, bytes.len());
    }
    response
        .body(body)
        .expect("prepared theme headers are valid ASCII")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn compressed_get_and_head_share_validators_and_lengths() {
        use std::io::Read;
        let theme = crate::infrastructure::services::theme_assets::prepare_theme(
            "default".into(),
            ":root { --color-accent: red; }".repeat(100),
        );
        for encoding in ["br", "gzip", "identity"] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, encoding.parse().unwrap());
            let get = response(&theme, &headers, &Method::GET);
            let head = response(&theme, &headers, &Method::HEAD);
            assert_eq!(get.headers(), head.headers());
            assert_eq!(get.headers()[header::CONTENT_ENCODING], encoding);
            let body = axum::body::to_bytes(get.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let mut decoded = Vec::new();
            match encoding {
                "gzip" => {
                    flate2::read::GzDecoder::new(body.as_ref())
                        .read_to_end(&mut decoded)
                        .unwrap();
                }
                "br" => {
                    brotli::Decompressor::new(body.as_ref(), 4096)
                        .read_to_end(&mut decoded)
                        .unwrap();
                }
                _ => decoded.extend_from_slice(&body),
            }
            assert_eq!(decoded.as_slice(), theme.css.as_ref());
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, "*;q=0".parse().unwrap());
        assert_eq!(
            response(&theme, &headers, &Method::GET).status(),
            StatusCode::NOT_ACCEPTABLE
        );
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
