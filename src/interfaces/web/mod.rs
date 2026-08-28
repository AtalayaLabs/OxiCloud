use crate::common::config::AppConfig;
use crate::common::di::AppState;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get_service;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

#[cfg(feature = "bundled-assets")]
pub mod embedded;

/// Where the SPA + immutable assets are served from — filesystem
/// (default and also the fallback on bundled builds when
/// `OXICLOUD_STATIC_PATH` points at real files) or the compile-time
/// embedded corpus (bundled-assets feature only).
///
/// Returned by [`resolve_static_source`]; matched at each of the four
/// consumer sites (SPA `ServeDir`, `_app/immutable` `ServeDir`,
/// CSP inline-script scan, and the locale-loader picker in `main.rs`).
#[derive(Debug, Clone)]
pub enum StaticSource {
    Filesystem(PathBuf),
    /// Serve from the `EmbeddedAssets` corpus in the [`embedded`] module.
    /// Only reachable under `--features bundled-assets` — the variant is
    /// cfg-gated so match arms in non-bundled builds stay exhaustive on
    /// a single variant, giving zero runtime cost.
    #[cfg(feature = "bundled-assets")]
    Embedded,
}

/// Resolve where static assets come from.
///
/// Order of precedence (highest first):
///  1. `<OXICLOUD_STATIC_PATH>/../static-dist/` when it exists — matches
///     the SvelteKit adapter-static output at the repo root.
///  2. `OXICLOUD_STATIC_PATH` itself when it exists — Docker image path
///     (assets copied straight to `/app/static/`).
///  3. Bundled-assets fallback (only when the feature is on) — the
///     compile-time embedded corpus.
///  4. Non-bundled fallback — return the configured path anyway, letting
///     downstream `ServeDir` fail predictably at request time.
///
/// Rule (2) exists so ops running a bundled binary can still point
/// `OXICLOUD_STATIC_PATH` at a live directory (locale patch, theme
/// override) and see it win over the embedded copy without a rebuild.
pub fn resolve_static_source(config: &AppConfig) -> StaticSource {
    let dist = config
        .static_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("static-dist");
    if dist.exists() {
        tracing::info!(
            source = %dist.display(),
            "static-assets: serving from filesystem (Vite build output at <static>/../static-dist/)"
        );
        return StaticSource::Filesystem(dist);
    }
    if config.static_path.exists() {
        tracing::info!(
            source = %config.static_path.display(),
            "static-assets: serving from filesystem (OXICLOUD_STATIC_PATH)"
        );
        return StaticSource::Filesystem(config.static_path.clone());
    }
    #[cfg(feature = "bundled-assets")]
    {
        tracing::info!(
            configured_static_path = %config.static_path.display(),
            "static-assets: no filesystem source found, serving embedded corpus \
             (bundled-assets feature). Set OXICLOUD_STATIC_PATH to override with a \
             live directory."
        );
        StaticSource::Embedded
    }
    #[cfg(not(feature = "bundled-assets"))]
    {
        // Non-bundled build with no on-disk source. Return the configured
        // path anyway — downstream `ServeDir` will fail predictably at
        // request time. A separate boot-time warning wouldn't help; the
        // real fix is to build the SPA or set OXICLOUD_STATIC_PATH.
        StaticSource::Filesystem(config.static_path.clone())
    }
}

/// Backwards-compat helper: resolve to a `PathBuf` directly.
///
/// Preserved for callers that predate the `StaticSource` enum. Only
/// callable in configurations where a filesystem path exists — a
/// bundled build whose `resolve_static_source` returned `Embedded`
/// would panic here, so new code should always match on
/// `resolve_static_source(...)` instead.
pub fn resolve_static_path(config: &AppConfig) -> PathBuf {
    match resolve_static_source(config) {
        StaticSource::Filesystem(p) => p,
        #[cfg(feature = "bundled-assets")]
        StaticSource::Embedded => {
            // Every migrated caller matches on StaticSource directly;
            // this branch means someone called the legacy helper from
            // a bundled build. Fix the caller, not the shim.
            panic!(
                "resolve_static_path() called on a bundled build with no filesystem \
                 assets — migrate the caller to resolve_static_source() and match on \
                 StaticSource::Embedded"
            )
        }
    }
}

/// Serves the SvelteKit single-page app.
///
/// The frontend is built by Vite into `static-dist/` (repo root). Real files are
/// served from disk; any unmatched client route (deep links such as
/// `/files/<id>`, `/s/<token>`, `/login`) falls back to the SPA shell
/// `index.html`, which boots the client router.
///
/// Caching: content-hashed assets under `/_app/immutable` are cached forever;
/// everything else — crucially the `index.html` shell — is `no-cache` so a deploy
/// can't leave a stale app pinned in browsers.
pub fn create_web_routes(
    app_state: Arc<AppState>,
    source: StaticSource,
) -> Router<Arc<AppState>> {
    // `source` is resolved ONCE at boot in `main.rs::run()` and passed
    // in — see the sequence there. Previously this fn called
    // `AppConfig::from_env()` + `resolve_static_source(&config)` itself
    // (duplicating the env parse + storage-summary log). Threading the
    // resolved value through as an arg keeps this fn pure and eliminates
    // both duplicates in the boot log (bug fixed 2026-08-28).

    // Build the router — two shapes depending on `StaticSource`, but both
    // wear the SAME outer layers below (compression fallback, no-cache
    // default for the shell, OIDC login short-circuit). Keeping the
    // layers common means the filesystem and embedded paths behave
    // identically at the wire boundary.
    let inner = match source {
        StaticSource::Filesystem(static_path) => web_routes_filesystem(&static_path),
        #[cfg(feature = "bundled-assets")]
        StaticSource::Embedded => web_routes_embedded(),
    };

    inner
        // Fallback compression for assets without a precompressed sibling
        // (filesystem) or for embedded assets that were compressed at
        // compile time and decompressed on read (bundled). Quality 4,
        // NOT the default: the default maps to Brotli q11 — ~1.3 s of
        // CPU per 700 KiB bundle per request (benches/STATIC-PRECOMPRESSED.md;
        // the .br siblings on the filesystem path carry the real q11
        // bytes, paid once at build time).
        .layer(
            CompressionLayer::new()
                .quality(tower_http::CompressionLevel::Precise(4))
                .br(true)
                .gzip(true),
        )
        // `if_not_present` so the immutable assets above keep their long cache;
        // the shell itself must always revalidate so a deploy can't pin a stale
        // app in browsers.
        .layer(SetResponseHeaderLayer::if_not_present(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        // Short-circuit `GET /login` to the OIDC authorize endpoint when
        // the AutoRedirectIfStandaloneOidc policy resolves. Runs BEFORE
        // the SPA shell is served, so there's no form-then-redirect flash.
        // The SPA carries the same predicate as belt-and-suspenders for
        // deep links / browser-cache hits that skip this hop.
        .layer(axum::middleware::from_fn_with_state(
            app_state,
            oidc_standalone_login_redirect,
        ))
}

/// Filesystem-served SPA — the historical shape. Two `ServeDir` instances
/// with tower-http's `precompressed_br().precompressed_gzip()` picking up
/// Vite's precompressed siblings when present.
fn web_routes_filesystem(static_path: &Path) -> Router<Arc<AppState>> {
    // SPA fallback: serve the file if it exists, else the app shell.
    //
    // `precompressed_*`: if the frontend build emitted a sibling `.br`/`.gz`
    // (frontend/scripts/precompress.mjs runs at build time), serve those
    // bytes directly with the right Content-Encoding instead of re-running
    // Brotli over the same immutable bundle on EVERY request — the
    // `CompressionLayer` in `create_web_routes` then skips the already-encoded
    // response and remains only the fallback for assets without a
    // precompressed sibling (benches/STATIC-PRECOMPRESSED.md).
    let spa = ServeDir::new(static_path)
        .precompressed_br()
        .precompressed_gzip()
        .fallback(ServeFile::new(static_path.join("index.html")));

    // Hashed, immutable assets (SvelteKit emits these under /_app/immutable).
    let app_immutable = ServeDir::new(static_path.join("_app").join("immutable"))
        .precompressed_br()
        .precompressed_gzip();

    Router::new()
        .nest_service(
            "/_app/immutable",
            get_service(app_immutable).layer(SetResponseHeaderLayer::overriding(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            )),
        )
        .fallback_service(spa)
}

/// Embedded-assets SPA — mirror of `web_routes_filesystem` using the
/// [`embedded`] module's handlers instead of `ServeDir`. Same URL shape,
/// same cache-header layers, same SPA-shell fallback semantics.
#[cfg(feature = "bundled-assets")]
fn web_routes_embedded() -> Router<Arc<AppState>> {
    use axum::routing::get;
    Router::new()
        .route(
            "/_app/immutable/{*path}",
            get(embedded::serve_immutable).layer(SetResponseHeaderLayer::overriding(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            )),
        )
        .route("/", get(embedded::serve_root_index))
        .fallback(get(embedded::serve_root))
}

/// Intercept `GET /login` and 302 to `/api/auth/oidc/authorize` when OIDC is
/// the only working method (see `AuthApplicationService::auto_redirect_to_oidc`).
///
/// Loop-guards mirror the SPA:
/// - `?error=…` — the IdP bounced us back; falling through lets the SPA render
///   the error rather than looping straight back to the failing IdP.
/// - `?oidc_code=…` — the callback landing carries the exchange code; the SPA
///   must handle it, not another authorize round-trip.
async fn oidc_standalone_login_redirect(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() == axum::http::Method::GET && req.uri().path() == "/login" {
        let has_loop_guard_param = req
            .uri()
            .query()
            .map(|q| {
                q.split('&')
                    .any(|p| p.starts_with("error=") || p.starts_with("oidc_code="))
            })
            .unwrap_or(false);

        let should_redirect = !has_loop_guard_param
            && state
                .auth_service
                .as_ref()
                .map(|svc| svc.auth_application_service.auto_redirect_to_oidc())
                .unwrap_or(false);

        if should_redirect {
            return Redirect::temporary("/api/auth/oidc/authorize").into_response();
        }
    }
    next.run(req).await
}

/// Build the `content-security-policy` header value served on every response.
///
/// `script-src` stays strict — `'self'` with **no** `'unsafe-inline'` — and
/// additionally lists a `'sha256-…'` source for each inline `<script>` found in
/// the served HTML shells: the anti-FOUC theme init in `app.html` and
/// SvelteKit's hydration bootstrap. Without those hashes the browser blocks the
/// bootstrap and the SPA never mounts (a blank page behind the splash spinner).
/// Hashes are recomputed from the built assets on every startup, so a frontend
/// rebuild needs no edit here.
///
/// Other directives:
/// - `style-src` keeps `'unsafe-inline'` because the frontend sets inline styles
///   (`element.style.*`) for UI state — impractical to migrate to classes.
/// - `frame-src` lists `blob:` explicitly (`*` only matches network schemes) for
///   inline PDF/document viewers; `media-src` lists `blob:` for blob video/audio.
/// - `worker-src` lists `blob:` because MapLibre GL (the Places map) spawns its
///   web worker from a blob URL; `'self'` covers same-origin workers like the
///   delta-upload worker.
pub fn content_security_policy(config: &AppConfig) -> String {
    let source = resolve_static_source(config);
    let hashes = match &source {
        StaticSource::Filesystem(p) => inline_script_csp_hashes(p),
        #[cfg(feature = "bundled-assets")]
        StaticSource::Embedded => inline_script_csp_hashes_embedded(),
    };
    if hashes.is_empty() {
        match &source {
            StaticSource::Filesystem(p) => {
                tracing::warn!(
                    static_path = %p.display(),
                    "CSP: no inline <script> hashes computed — if the SPA shell ships \
                     inline scripts they will be blocked by script-src 'self'. Check the \
                     static asset path (OXICLOUD_STATIC_PATH)."
                );
            }
            #[cfg(feature = "bundled-assets")]
            StaticSource::Embedded => {
                tracing::warn!(
                    "CSP: no inline <script> hashes computed from embedded corpus — \
                     the SPA shell may boot with a blocked script-src. Rebuild with \
                     an up-to-date static-dist/."
                );
            }
        }
    }

    // `'wasm-unsafe-eval'` is required for WebAssembly compilation/instantiation
    // under a strict CSP (Chromium blocks `WebAssembly.instantiate` otherwise with
    // "Wasm code generation disallowed by embedder"). The frontend instantiates the
    // vendored BLAKE3/FastCDC WASM both on the main thread (instant by-hash uploads)
    // and inside the delta-upload worker; without this they throw and every large
    // file silently falls back to a plain byte upload. It is the WASM-only, safe
    // variant — it does NOT permit `eval()`/`new Function()` (no `'unsafe-eval'`).
    let mut script_src = String::from("script-src 'self' 'wasm-unsafe-eval'");
    for hash in &hashes {
        script_src.push(' ');
        script_src.push_str(hash);
    }

    format!(
        "default-src 'self'; \
         {script_src}; \
         worker-src 'self' blob:; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob: https:; \
         media-src 'self' blob:; \
         connect-src 'self'; \
         font-src 'self' data:; \
         frame-src * blob:; \
         frame-ancestors 'none'; \
         base-uri 'self'; \
         form-action 'self'"
    )
}

/// SHA-256 CSP source expressions (`'sha256-…'`) for every inline `<script>` in
/// the root-level HTML shells under `static_path`.
///
/// The browser hashes the exact bytes between `<script …>` and `</script>`, so
/// each shell is read verbatim and that slice hashed. Scripts carrying a `src`
/// attribute are external (already allowed by `'self'`) and skipped. Only the
/// directory root is scanned — the SPA is client-rendered (SSR/prerender off),
/// so the only inline-script shell is `index.html`. Returns a deduplicated,
/// sorted list; empty when the dir is unreadable
/// (e.g. a Vite dev server serving HTML on its own port instead).
fn inline_script_csp_hashes(static_path: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(static_path) else {
        return Vec::new();
    };

    let mut hashes = BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let Ok(html) = std::fs::read_to_string(&path) else {
            continue;
        };
        for script in inline_scripts(&html) {
            hashes.insert(csp_hash(script));
        }
    }
    hashes.into_iter().collect()
}

/// Embedded-corpus twin of [`inline_script_csp_hashes`].
///
/// Same arithmetic — iterate root-level `.html` shells, extract every
/// inline `<script>`, hash each — but pulls bytes from
/// [`embedded::EmbeddedAssets`] instead of the filesystem. The two
/// functions produce identical output for the same source tree, so
/// `content_security_policy` can pick either without callers seeing a
/// difference.
#[cfg(feature = "bundled-assets")]
fn inline_script_csp_hashes_embedded() -> Vec<String> {
    let mut hashes = BTreeSet::new();
    for (_name, bytes) in embedded::embedded_html_shells() {
        let Ok(html) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for script in inline_scripts(html) {
            hashes.insert(csp_hash(script));
        }
    }
    hashes.into_iter().collect()
}

/// The CSP `'sha256-<base64>'` source expression for one inline script body.
fn csp_hash(script: &str) -> String {
    let digest = Sha256::digest(script.as_bytes());
    let encoded = base64::engine::general_purpose::STANDARD.encode(digest);
    format!("'sha256-{encoded}'")
}

/// Text content of every inline `<script>` (no `src`) in `html`, returned as
/// byte-exact slices suitable for CSP hashing.
///
/// Skips HTML comments (`<!-- ... -->`) before matching `<script`. Without this,
/// a comment containing the literal string `<script>` (e.g. the theme-init
/// explanatory block in the SvelteKit shell) causes the scanner to match the
/// comment first, consume through the real script's `</script>`, and emit the
/// wrong hash — the real inline script then fails CSP with `script-src 'self'`.
fn inline_scripts(html: &str) -> Vec<&str> {
    let mut scripts = Vec::new();
    let mut cursor = 0;
    while cursor < html.len() {
        let tail = &html[cursor..];
        // Skip past HTML comments — they may contain the literal
        // string `<script>` in prose and would otherwise poison the
        // scanner. Comment-nesting is not a spec concern.
        let next_comment = find_ci(tail, "<!--");
        let next_script = find_ci(tail, "<script");
        match (next_comment, next_script) {
            (Some(c), Some(s)) if c < s => {
                let end_rel = find_ci(&tail[c + 4..], "-->").map(|r| c + 4 + r + 3);
                cursor = match end_rel {
                    Some(e) => cursor + e,
                    None => break, // unterminated comment; give up
                };
                continue;
            }
            (Some(c), None) => {
                let end_rel = find_ci(&tail[c + 4..], "-->").map(|r| c + 4 + r + 3);
                cursor = match end_rel {
                    Some(e) => cursor + e,
                    None => break,
                };
                continue;
            }
            (None, None) => break,
            _ => {} // next thing is a real <script
        }
        let rel = next_script.unwrap();
        let tag_start = cursor + rel;
        // End of the opening tag.
        let Some(gt) = html[tag_start..].find('>') else {
            break;
        };
        let open_tag = &html[tag_start..tag_start + gt + 1];
        let content_start = tag_start + gt + 1;
        // Matching close tag.
        let Some(close_rel) = find_ci(&html[content_start..], "</script>") else {
            break;
        };
        let content_end = content_start + close_rel;
        if !opening_tag_has_src(open_tag) {
            scripts.push(&html[content_start..content_end]);
        }
        cursor = content_end + "</script>".len();
    }
    scripts
}

/// Whether a `<script …>` opening tag carries a `src` attribute (i.e. it loads
/// an external file rather than inlining code).
fn opening_tag_has_src(open_tag: &str) -> bool {
    open_tag
        .to_ascii_lowercase()
        .split(|c: char| c.is_whitespace() || c == '/')
        .any(|token| token == "src" || token.starts_with("src="))
}

/// ASCII-case-insensitive substring search. The returned byte offset is a valid
/// `str` boundary because `needle` (and therefore every matched byte) is ASCII.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (hay, ndl) = (haystack.as_bytes(), needle.as_bytes());
    if ndl.is_empty() || hay.len() < ndl.len() {
        return None;
    }
    (0..=hay.len() - ndl.len())
        .find(|&start| hay[start..start + ndl.len()].eq_ignore_ascii_case(ndl))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_inline_script_content_verbatim() {
        // Leading/trailing whitespace inside the tag is part of what the browser
        // hashes, so it must be preserved exactly.
        let html = "<head><script>\n  alert(1);\n</script></head>";
        assert_eq!(inline_scripts(html), vec!["\n  alert(1);\n"]);
    }

    #[test]
    fn skips_external_src_scripts() {
        let html = r#"<script src="/app.js"></script><script>boot();</script>"#;
        assert_eq!(inline_scripts(html), vec!["boot();"]);
    }

    #[test]
    fn keeps_inline_module_skips_module_with_src() {
        let html =
            r#"<script type="module" src="/x.js"></script><script type="module">go();</script>"#;
        assert_eq!(inline_scripts(html), vec!["go();"]);
    }

    #[test]
    fn case_insensitive_tag_matching() {
        let html = "<SCRIPT>run();</SCRIPT>";
        assert_eq!(inline_scripts(html), vec!["run();"]);
    }

    #[test]
    fn empty_inline_script_hash_matches_known_sha256_vector() {
        // SHA-256 of the empty string, base64 — the canonical empty digest.
        assert_eq!(
            csp_hash(""),
            "'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='"
        );
    }

    #[test]
    fn identical_scripts_produce_one_deduplicated_hash() {
        let html = "<script>x()</script><script>x()</script>";
        let mut set = BTreeSet::new();
        for s in inline_scripts(html) {
            set.insert(csp_hash(s));
        }
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn html_comment_mentioning_script_does_not_poison_scanner() {
        // The SvelteKit shell has an explanatory comment referring to
        // `<script>` in its prose (see static-dist/index.html theme-init
        // block). Without comment skipping the scanner matches the
        // comment's substring first, consumes through the real script's
        // close tag, and emits the wrong hash — the real script then
        // fails CSP with `script-src 'self'`.
        let html = concat!(
            "<!-- svelte.config.js finds this <script> by id and adds its hash -->\n",
            "<script id=\"theme-init\">alert(1);</script>\n",
            "<script>boot();</script>\n",
        );
        let scripts = inline_scripts(html);
        assert_eq!(scripts, vec!["alert(1);", "boot();"]);
    }

    #[test]
    fn unterminated_comment_bails_out_gracefully() {
        // Malformed input: `<!--` never closed. Must not loop forever
        // and must not falsely capture anything downstream.
        let html = "<!-- unterminated <script>evil()</script>";
        assert!(inline_scripts(html).is_empty());
    }

    #[test]
    fn distinct_scripts_produce_distinct_hashes() {
        assert_ne!(csp_hash("a()"), csp_hash("b()"));
    }
}
