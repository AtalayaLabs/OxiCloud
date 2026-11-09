//! Global response security headers, in one pass.
//!
//! Every response (API, static, DAV, file bodies) gets the four static
//! headers; the Content-Security-Policy is decided per response because
//! three kinds of body need three different policies:
//!
//! | Body | Policy |
//! |---|---|
//! | user-uploaded file (has `Content-Disposition`) | [`crate::interfaces::user_content`] — sandboxed unless it is media |
//! | `text/html` | none of ours; the SPA shell's hash-based `<meta>` governs, and a handler that set its own keeps it |
//! | everything else | [`APP_CSP`], unless the handler set one |
//!
//! Lived in `main.rs` until it outgrew the composition root. Moving it
//! here also made the decisions unit-testable, which they were not
//! before — the only coverage was Hurl against a live server.
//!
//! ## Why one middleware and not five layers
//!
//! The four static headers used to be four separate
//! `SetResponseHeaderLayer`s stacked on top of this one (5 tower layers
//! per response). Folding them in measured 1.43x per request / −26
//! allocs with a byte-identical header set, including on 304s
//! (`benches/ROUND12.md` §M3). Keep them folded, and keep any
//! per-response work here allocation-free — this runs on every request.

use axum::extract::Request;
use axum::http::header::HeaderName;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

/// The app's own policy, for every response that is neither a file body
/// nor an HTML document.
///
/// Mirrors the SPA meta policy in `frontend/svelte.config.js` — keep the
/// two in step, since a document served under one and navigated from the
/// other should not behave differently.
///
/// Notes on the loose-looking entries:
///   * `style-src 'unsafe-inline'` — the frontend sets inline styles at
///     runtime (`element.style.display`); hashes cannot cover those.
///   * `media-src 'blob:'` — `blob:` video/audio playback.
///   * `worker-src 'blob:'` — `maplibre-gl` spawns its worker from a
///     blob URL, so the Places map does not load without it
///     (upstream `e0ecf96a`, "allow blob workers in the SPA CSP so
///     Places loads"). It arrived while this policy was being moved
///     out of `main.rs`, and taking either side of that conflict
///     wholesale would have dropped it — hence the test pin below.
///
/// `frame-src` is `'self' blob:` and `form-action` is `'self'`: both used
/// to be wide open (`*`, and `https:`) so the SPA could iframe and POST
/// a token form to an admin-configured office-editor host it could not
/// name at build time. That handshake now happens in the server-rendered
/// `/wopi/edit/{id}` host page, which emits its own CSP naming the one
/// origin from the discovery document's `urlsrc`, so nothing here needs
/// a wildcard. A cross-origin editor belongs in that page's policy.
const APP_CSP: &str = "default-src 'self'; \
     script-src 'self'; \
     worker-src 'self' blob:; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data: blob: https:; \
     media-src 'self' blob:; \
     connect-src 'self'; \
     font-src 'self' data:; \
     frame-src 'self' blob:; \
     frame-ancestors 'none'; \
     base-uri 'self'; \
     form-action 'self'";

/// Stamp the security headers on the way out.
pub async fn security_headers_layer(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    {
        let h = res.headers_mut();
        h.insert(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        // `X-Frame-Options` is a DEFAULT, not a mandate: set it only
        // when the handler did not. A handler that must be framed says
        // so by setting its own value, and this layer runs after the
        // handler, so an unconditional `insert` would silently overrule
        // it. The WOPI host page (`/wopi/edit/{id}`) needs
        // `SAMEORIGIN` — the SPA frames it — and `FileViewer.svelte`
        // already documents working around this exact clobber for the
        // PDF viewer by round-tripping through a `blob:`.
        //
        // Weakening is therefore possible, but only from our own code,
        // deliberately, in one visible place per handler — and
        // `frame-ancestors` in that handler's CSP is the modern
        // enforcement regardless.
        let xfo = HeaderName::from_static("x-frame-options");
        if !h.contains_key(&xfo) {
            h.insert(xfo, HeaderValue::from_static("DENY"));
        }
        h.insert(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        );
        h.insert(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        );
    }

    // A 304 Not Modified carries no entity headers (no Content-Type) since
    // there's no body — `is_html` would read `None` and misclassify it as
    // "not html", attaching the strict headerless CSP below. Browsers merge
    // a 304's headers into the cached document's effective response, so
    // that stray header would then stack with (and defeat) the SPA's own
    // hash-based `<meta>` CSP on every revalidated repeat visit — this was
    // a real bug (see git blame): a browser tab reopened at `/login` after
    // the first, freshly-fetched visit got permanently stuck behind the
    // boot spinner because its now-conditionally-cached `200` picked up an
    // extra hash-less `script-src 'self'` header from the 304 that
    // revalidated it, blocking the app's own inline hydration script.
    // Nothing to add on a 304 regardless — its headers must only carry
    // caching metadata, never a fresh policy decision.
    if res.status() == StatusCode::NOT_MODIFIED {
        return res;
    }

    // ── USER-UPLOADED CONTENT: neutralise it ────────────────────────
    // Checked BEFORE the `is_html` branch, and that order is the whole
    // point. That branch sends NO CSP on `text/html` so the SPA's own
    // <meta> policy can govern the shell — but it keys on Content-Type,
    // which cannot tell our shell from a file a user uploaded. An
    // uploaded .html served inline therefore came back as a
    // fully-privileged same-origin document with zero CSP, and its
    // script ran with the victim's session (user-reported stored XSS).
    //
    // `Content-Disposition` is the discriminator because it already
    // means "this response is a file body" here — the compression
    // predicate in `main.rs` uses the same test. That makes it
    // structural: a new download handler is covered the day it is
    // written, which an enumerated route list never is.
    if let Some(mime) = user_content_mime(&res) {
        res.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(crate::interfaces::user_content::csp_for(&mime)),
        );
        return res;
    }

    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));

    if is_html {
        // `no-store` (not just `no-cache`) on the SPA shell: Chrome/Firefox/
        // Safari all treat `no-store` as an explicit opt-out of the
        // back-forward cache (bfcache), which is a full in-memory snapshot
        // of the page that bypasses HTTP revalidation entirely — `no-cache`
        // alone does NOT prevent it. Without this, a shell instance loaded
        // before a deploy can be resurrected byte-for-byte (old inline
        // hydration script + old CSP hash) after navigating away and back —
        // e.g. the OIDC login round-trip's two full-page navigations — and
        // the resurrected page's old CSP `<meta>` no longer matches assets
        // referenced by the current build, leaving the app permanently
        // stuck behind the boot spinner until a hard reload.
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

        // No CSP header on the SPA shell — the shell's own hash-based
        // <meta> policy governs it (see `kit.csp` in
        // frontend/svelte.config.js), and a static `script-src 'self'`
        // here would block SvelteKit's hashed inline bootstrap and blank
        // the app.
        //
        // An earlier cut of this appended a runtime policy here to narrow
        // the shell's `form-action`/`frame-src` down to the office
        // editor's origin, because the build could not know an operator's
        // editor host. That is no longer needed: the editor handshake
        // moved into the server-rendered `/wopi/edit/{id}` host page
        // (which emits its own CSP naming the origin from the discovery
        // document's `urlsrc`), so the SPA frames only `'self'` and
        // `blob:` and posts nowhere cross-origin. Both directives are
        // therefore build-time knowable and were tightened in
        // `svelte.config.js` instead — one policy, no config dependency,
        // and no divergence between the discovery URL's origin and the
        // `urlsrc` origin the POST actually targets (the Hurl mock has
        // those two deliberately different).
    } else if !res.headers().contains_key(header::CONTENT_SECURITY_POLICY) {
        // Same rule as `X-Frame-Options` above: the app policy is the
        // DEFAULT for a response that did not state one. A handler that
        // set its own CSP meant it, and it is invariably tighter than
        // `APP_CSP` — the NC avatar endpoint ships `default-src 'none';
        // style-src 'unsafe-inline'` for a generated SVG, and the WOPI
        // host page names the editor origin. An unconditional `insert`
        // replaced both with the broad app policy, which is how a
        // handler's deliberate hardening becomes decoration.
        //
        // The user-content branch above still FORCES its policy, on
        // purpose: a handler must not be able to opt a user-uploaded
        // body out of sandboxing.
        res.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(APP_CSP),
        );
    }
    res
}

/// Lowercased mime of a response that carries a FILE BODY, or `None` for
/// everything else (API JSON, the SPA shell, static assets).
///
/// Keyed on `Content-Disposition` — present on downloads, inline
/// previews and ZIP exports, absent everywhere else.
///
/// A file body with no `Content-Type` returns `Some("")`, which matches
/// no safe prefix and is therefore sandboxed. That is the intended
/// answer: an untyped body is exactly the one a browser might sniff.
fn user_content_mime<B>(res: &axum::http::Response<B>) -> Option<String> {
    let h = res.headers();
    if !h.contains_key(header::CONTENT_DISPOSITION) {
        return None;
    }
    Some(
        h.get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                // Strip any `; charset=…` parameter before matching.
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::user_content::{CSP_USER_CONTENT_MEDIA, CSP_USER_CONTENT_SANDBOXED};

    /// Source expressions of one CSP directive, as a set.
    ///
    /// A directive's sources are UNORDERED, so any assertion that
    /// spells two of them in sequence pins an accident of formatting
    /// rather than the policy. Panics when the directive is absent —
    /// "the directive vanished" should fail the test, not silently
    /// compare an empty set.
    fn sources(policy: &str, directive: &str) -> std::collections::BTreeSet<String> {
        policy
            .split(';')
            .map(str::trim)
            .find_map(|d| {
                // The name guard lives INSIDE the closure on purpose: as a
                // `.filter()` after `find_map` it would abort the search at
                // the first partial-name hit instead of moving on to the
                // next directive.
                d.strip_prefix(directive)
                    .filter(|rest| rest.is_empty() || rest.starts_with(' '))
            })
            .unwrap_or_else(|| panic!("directive {directive:?} missing from {policy:?}"))
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    fn set(items: &[&str]) -> std::collections::BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn res_with(headers: &[(HeaderName, &str)]) -> axum::http::Response<()> {
        let mut b = axum::http::Response::builder();
        for (name, value) in headers {
            b = b.header(name.clone(), *value);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn a_file_body_is_recognised_by_its_disposition() {
        let res = res_with(&[
            (header::CONTENT_DISPOSITION, "inline; filename=\"x.html\""),
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        ]);
        // Charset stripped and case folded, so the classifier sees the
        // bare type — `Text/HTML; charset=…` is what an upload carries.
        assert_eq!(user_content_mime(&res).as_deref(), Some("text/html"));
    }

    #[test]
    fn the_spa_shell_is_not_a_file_body() {
        // No Content-Disposition ⇒ not user content, whatever the type.
        // This is the distinction the reported XSS turned on: keying on
        // Content-Type alone cannot tell the shell from an upload.
        let res = res_with(&[(header::CONTENT_TYPE, "text/html")]);
        assert_eq!(user_content_mime(&res), None);
    }

    #[test]
    fn an_untyped_file_body_classifies_as_empty_and_is_sandboxed() {
        let res = res_with(&[(header::CONTENT_DISPOSITION, "attachment")]);
        let mime = user_content_mime(&res).expect("is a file body");
        assert_eq!(mime, "");
        assert_eq!(
            crate::interfaces::user_content::csp_for(&mime),
            CSP_USER_CONTENT_SANDBOXED
        );
    }

    #[test]
    fn a_media_file_body_is_not_sandboxed() {
        // The half that keeps preview working.
        let res = res_with(&[
            (header::CONTENT_DISPOSITION, "inline"),
            (header::CONTENT_TYPE, "image/png"),
        ]);
        let mime = user_content_mime(&res).unwrap();
        assert_eq!(
            crate::interfaces::user_content::csp_for(&mime),
            CSP_USER_CONTENT_MEDIA
        );
    }

    #[test]
    fn the_source_parser_is_order_independent_and_directive_scoped() {
        // This helper is the reason the assertions below are trustworthy,
        // so it gets its own pin.
        let p = "default-src 'none'; frame-src 'self' blob:; frame-ancestors 'none'; \
                 form-action https: 'self'";
        // Order must not matter — the whole point.
        assert_eq!(sources(p, "form-action"), set(&["'self'", "https:"]));
        // A directive must not bleed into its neighbour: `frame-src` and
        // `frame-ancestors` share a prefix, and `frame-src` comes first,
        // so a sloppy parser returns the wrong one for `frame-ancestors`.
        assert_eq!(sources(p, "frame-src"), set(&["'self'", "blob:"]));
        assert_eq!(sources(p, "frame-ancestors"), set(&["'none'"]));
    }

    #[test]
    #[should_panic(expected = "missing from")]
    fn the_source_parser_fails_loudly_on_a_missing_directive() {
        // An absent directive must fail the test, not quietly compare an
        // empty set and pass.
        sources("default-src 'none'", "form-action");
    }

    #[test]
    fn the_app_policy_carries_no_wildcard_for_the_office_editor() {
        // Both of these were wide open so the SPA could iframe and POST
        // a token form to an admin-configured editor host the build
        // cannot name. The handshake moved to the server-rendered
        // `/wopi/edit/{id}` page, which names the one origin from
        // discovery `urlsrc` in its own CSP — so a wildcard reappearing
        // here means someone put the cross-origin POST back in the SPA.
        // Compare the directive's TOKEN SET, never a substring of the
        // whole policy. A CSP source list is unordered, so
        // `contains("form-action 'self' https:")` catches one spelling
        // and misses `form-action https: 'self'` — the same policy. A
        // bare `!contains("https:")` is worse still: it goes red on
        // `img-src 'self' data: blob: https:`, which is a deliberate
        // allowance for remote avatars. (The first two cuts of this test
        // made both mistakes.)
        assert_eq!(sources(APP_CSP, "form-action"), set(&["'self'"]));
        assert_eq!(sources(APP_CSP, "frame-src"), set(&["'self'", "blob:"]));
        // `blob:` is load-bearing: the PDF viewer iframes a same-origin
        // blob (`pdfUrl` in FileViewer.svelte). Drop it and preview
        // breaks with nothing but a console error to say why.
        assert!(sources(APP_CSP, "frame-src").contains("blob:"));
        // img-src keeps `https:` on purpose — asserted positively so a
        // future "tighten everything" sweep has to notice it is a
        // choice.
        assert!(sources(APP_CSP, "img-src").contains("https:"));
        // `worker-src blob:` is load-bearing for the Places map
        // (`maplibre-gl` spawns its worker from a blob URL). It landed
        // upstream in `e0ecf96a` while this constant was being moved out
        // of `main.rs`, and resolving that conflict by taking either
        // side wholesale would have silently reverted it — the page
        // would just stop loading, with no test to say why. Hence a pin
        // rather than a comment.
        assert!(
            sources(APP_CSP, "worker-src").contains("blob:"),
            "worker-src must keep blob: — the Places map will not load without it"
        );
        // `blob:` is load-bearing: the PDF viewer iframes a same-origin
        // blob (`pdfUrl` in FileViewer.svelte). Drop it and preview
        // breaks with nothing but a console error to say why.
        assert!(APP_CSP.contains("blob:"));
    }
}
