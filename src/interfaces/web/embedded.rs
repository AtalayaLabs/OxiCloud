//! Compile-time-embedded static assets — the `bundled-assets` feature.
//!
//! When the feature is on, the SvelteKit build output at `static-dist/`
//! (repo root, per `frontend/svelte.config.js`'s `adapter-static`) is
//! baked into the binary via `rust-embed` at compile time. The two axum
//! handlers below (`serve_root` for the SPA + fallback, `serve_immutable`
//! for the `_app/immutable` cache-forever subtree) parallel the two
//! `ServeDir` instances the filesystem path uses in `super::mod`.
//!
//! **Precedence** — this module is only invoked when
//! `resolve_static_source(config)` returns `StaticSource::Embedded`. If
//! `OXICLOUD_STATIC_PATH` (or the default `./static/static-dist/`)
//! points at a real directory, the filesystem `ServeDir` path is used
//! instead — ops can still override embedded bytes for locale patches
//! or theming without a full rebuild.
//!
//! **Compression** — `rust-embed`'s `compression` feature stores each
//! embedded file deflate-compressed. Lazy decompression on first access
//! keeps the binary small (~4-5 MB for the current corpus) and the
//! runtime cost negligible: after warmup every file is cached. Response
//! compression is handled by `CompressionLayer` in the parent module —
//! same wire behaviour as the filesystem path for `Content-Encoding:
//! br|gzip|identity` clients.
//!
//! **Vite's precompressed siblings** (`.br` / `.gz`) are excluded from
//! the embed via the `#[exclude]` attributes below — they'd be dead
//! weight because the response compression on the wire already handles
//! this negotiation.

use axum::body::Body;
use axum::extract::{Path, Request};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// SvelteKit SPA build output, baked into the binary at compile time.
///
/// The `#[folder]` path is relative to `Cargo.toml` (repo root), which
/// matches SvelteKit's `adapter-static` output in
/// `frontend/svelte.config.js` (`pages: '../static-dist'`).
#[derive(rust_embed::RustEmbed)]
#[folder = "static-dist/"]
// Default (no `include` attr) = include everything recursively. An
// explicit `include = "*"` was WRONG — the `*` glob is single-segment
// only, so `locales/*.json`, `_app/immutable/**`, and every other
// subdirectory got excluded and the boot-time `extract_embedded_locales`
// found 0 files (2026-08-28 bug fix).
//
// Vite's precompressed siblings — we serve raw and let the axum
// `CompressionLayer` on the wire negotiate br/gzip. Doubling storage
// would balloon the embed by ~50%.
#[exclude = "**/*.br"]
#[exclude = "**/*.gz"]
pub struct EmbeddedAssets;

/// Serve any embedded asset by request path, falling back to the SPA
/// shell (`index.html`) for unmatched client routes.
///
/// Mirror of the `spa` `ServeDir` in `super::create_web_routes` — same
/// fallback semantics so deep links like `/files/<id>` boot the SvelteKit
/// router. `Cache-Control` for the shell itself is left to the outer
/// layer in the parent module (`no-cache` so a deploy can't leave a
/// stale app pinned in browsers); assets carrying no cache header here
/// pick up the parent's default the same way filesystem-served assets do.
///
/// Wired as axum's `fallback` in `super::web_routes_embedded`, which
/// means there is NO route pattern to capture from — a `Path` extractor
/// would fail at runtime with "Wrong number of path arguments for
/// `Path`. Expected 1 but got 0." (real bug hit 2026-08-28). Pull the
/// URI path off the `Request` directly instead.
pub async fn serve_root(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    if path.is_empty() {
        return spa_shell_response();
    }
    match EmbeddedAssets::get(path) {
        Some(file) => asset_response(path, file.data),
        None => spa_shell_response(),
    }
}

/// Root path (no trailing capture) — always the SPA shell.
///
/// axum routes `/` separately from `/*path`, so this handles the
/// bare-slash case that `serve_root` never sees.
pub async fn serve_root_index() -> Response {
    spa_shell_response()
}

/// Serve an asset under the `/_app/immutable/*` prefix. The nested route
/// registration in `super::create_web_routes` already strips the
/// `/_app/immutable/` prefix from the captured path, so we look the
/// stripped path up with the prefix re-attached before hitting the embed.
///
/// Cache-Control (`public, max-age=31536000, immutable`) is applied by
/// the outer `SetResponseHeaderLayer::overriding` in the parent module,
/// same as the filesystem path — this handler just returns bytes + MIME.
pub async fn serve_immutable(Path(path): Path<String>) -> Response {
    let full = format!("_app/immutable/{}", path.trim_start_matches('/'));
    match EmbeddedAssets::get(&full) {
        Some(file) => asset_response(&path, file.data),
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

fn asset_response(path: &str, bytes: std::borrow::Cow<'static, [u8]>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut resp = Response::new(Body::from(bytes.into_owned()));
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    resp
}

fn spa_shell_response() -> Response {
    match EmbeddedAssets::get("index.html") {
        Some(shell) => {
            let mut resp = Response::new(Body::from(shell.data.into_owned()));
            resp.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            // Belt: the parent module also stamps this on unset,
            // but stamp it here too so the shell never accidentally
            // ends up cacheable in front of a deploy.
            resp.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => {
            // This means `static-dist/` was empty at build time — the
            // build.rs guard should have prevented us from ever getting
            // here. Surface as 500 rather than pretending the SPA works.
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "SPA shell missing from embedded assets; rebuild with an up-to-date static-dist/",
            )
                .into_response()
        }
    }
}

/// Iterate over the embedded `.html` files at the root of `static-dist/`
/// so the CSP inline-script scanner can hash them without a filesystem
/// read. Mirrors what `std::fs::read_dir(static_path)` yields on the
/// filesystem path, filtered to `.html` at the top level.
pub fn embedded_html_shells() -> Vec<(String, std::borrow::Cow<'static, [u8]>)> {
    EmbeddedAssets::iter()
        .filter(|p| {
            // Root-level `.html` only — SvelteKit emits `index.html` at
            // the root and everything else under `_app/`. Nested `.html`
            // (e.g. sourcemap tooling artefacts) doesn't inline-script,
            // so skip.
            let s: &str = p.as_ref();
            s.ends_with(".html") && !s.contains('/')
        })
        .filter_map(|p| {
            let name = p.to_string();
            EmbeddedAssets::get(&name).map(|f| (name, f.data))
        })
        .collect()
}
