//! How a response carrying USER-UPLOADED bytes may be rendered.
//!
//! One classification, two consumers — deliberately:
//!
//!   * [`build_content_disposition`](crate::interfaces::api::handlers::file_handler)
//!     decides `inline` vs `attachment`.
//!   * the security-header middleware in `main.rs` decides which
//!     Content-Security-Policy the body gets.
//!
//! They are the same policy asked twice ("is this safe to render?"), so
//! they must not be two lists. A type added to one and missed in the
//! other is how the reported stored-XSS existed in the first place: the
//! disposition allowlist said `image/*` was safe to inline while the
//! CSP layer had no opinion about user content at all.
//!
//! ## The vulnerability this closes
//!
//! A user reported that an uploaded `.html` opened inline executed its
//! script. `GET /api/files/{id}?inline=true` returned `Content-Type:
//! text/html` + `Content-Disposition: inline` + **no CSP**, because the
//! global middleware skips CSP on `text/html` so the SPA's own
//! hash-based `<meta>` policy can govern the app shell. That test keys
//! on Content-Type, which cannot tell the shell from a file a user
//! uploaded, so the document ran with the victim's session on the app
//! origin.
//!
//! `image/svg+xml` was the quieter half: it matched the `image/*`
//! inline allowlist, so it needed no `?inline=true` at all. An SVG is a
//! scriptable document, not a raster image.

/// Mime types that are scriptable DOCUMENTS — a browser performing a
/// top-level navigation to one will execute code inside it.
///
/// Never served `inline`, and always sandboxed. The empty string is
/// included because an untyped body is the one a browser is most likely
/// to sniff into something executable.
pub fn is_scriptable_document(mime: &str) -> bool {
    let m = normalize(mime);
    m.is_empty()
        || m == "text/html"
        || m == "text/html-sandboxed"
        || m == "application/xhtml+xml"
        || m.starts_with("image/svg")
        || m == "text/xml"
        || m == "application/xml"
        || m == "application/xslt+xml"
        || m == "text/xsl"
}

/// Strip any `; charset=…` parameter and fold case, so
/// `Text/HTML; charset=utf-8` classifies as `text/html`.
fn normalize(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Policy for a user-content body that must NOT execute — everything
/// [`is_scriptable_document`] matches.
///
/// `sandbox` with NO `allow-*` tokens applies iframe-sandbox semantics
/// to a TOP-LEVEL document: scripts disabled and an opaque origin, so
/// even if execution were somehow reached the document cannot touch
/// app-origin cookies or storage.
///
/// `default-src 'none'` is safe to pair with it here precisely because
/// these types must not render anyway — unlike the media policy below,
/// nothing is lost if the browser also refuses to fetch subresources.
///
/// Adding `allow-scripts` re-opens the hole; `allow-scripts` together
/// with `allow-same-origin` restores the reported vulnerability in
/// full. `tests/api/user_content_xss_headers.hurl` asserts both tokens
/// absent for exactly that reason — a future "make HTML preview render
/// again" patch is the likeliest way this regresses, and the answer to
/// such a report is a viewer that fetches bytes into a pane, never a
/// loosened sandbox.
pub const CSP_USER_CONTENT_SANDBOXED: &str = "default-src 'none'; \
     sandbox; \
     base-uri 'none'; \
     form-action 'none'; \
     frame-ancestors 'none'";

/// Policy for a user-content body a browser renders natively — images,
/// video, audio, PDF, and anything else not classified as a scriptable
/// document.
///
/// ## Why this is not simply the sandboxed policy
///
/// Sandboxing these too would be simpler and is probably harmless, but
/// "probably" is doing real work in that sentence and the cost of being
/// wrong is broken file preview for every user:
///
///   * A top-level navigation to an image or PDF makes the browser
///     synthesise a document around the body. Whether `sandbox` and
///     `default-src 'none'` leave that document able to render is
///     browser-specific; the built-in PDF viewer is the known-fragile
///     case, and this has NOT been verified in a browser.
///   * Chrome requires the `allow-downloads` sandbox token for a
///     sandboxed context to start a download. An attachment response
///     almost certainly never becomes a sandboxed document, so this
///     almost certainly does not apply — again unverified.
///
/// So the restrictions here are the ones that cannot interfere with
/// rendering: no framing, no `<base>` rewriting, no form posts. No
/// `default-src`, deliberately — the one directive that could stop a
/// natively-rendered body from displaying.
///
/// This is NOT the old `renders_safely` allowlist returning. The split
/// is driven by the single [`is_scriptable_document`] classifier that
/// `build_content_disposition` also uses, so there is one list, used
/// twice, and no second list to drift out of step.
///
/// If someone verifies that `sandbox` is harmless for these in the
/// browsers OxiCloud targets, collapsing both constants into one is a
/// strict improvement — delete this and route everything through the
/// sandboxed policy.
pub const CSP_USER_CONTENT_MEDIA: &str = "base-uri 'none'; \
     form-action 'none'; \
     frame-ancestors 'none'";

/// The policy for a user-content body of this mime.
///
/// EVERY user-content response gets one. The app's own CSP exists for
/// the SPA (`script-src 'self'` and friends) and says nothing useful
/// about a file someone uploaded, so letting media fall through to it
/// was never right either.
pub fn csp_for(mime: &str) -> &'static str {
    if is_scriptable_document(mime) {
        CSP_USER_CONTENT_SANDBOXED
    } else {
        CSP_USER_CONTENT_MEDIA
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_payload_is_a_scriptable_document() {
        // The exact case from the user report: an uploaded .html served
        // inline. Must never get an `inline` disposition.
        assert!(is_scriptable_document("text/html"));
    }

    #[test]
    fn svg_is_a_document_not_an_image() {
        // ⚠ The quieter half: `image/svg+xml` matched the `image/*`
        // inline allowlist in `build_content_disposition`, so it reached
        // a top-level navigation with no `?inline=true` at all. It is a
        // scriptable document that merely looks like a picture.
        assert!(is_scriptable_document("image/svg+xml"));
        assert!(is_scriptable_document("image/svg+xml; charset=utf-8"));
    }

    #[test]
    fn real_images_and_media_are_not_documents() {
        // These keep their `inline` disposition — the half of the fix
        // that preserves preview. (They are still sandboxed by the CSP;
        // see CSP_USER_CONTENT on why that costs nothing.)
        for m in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "video/mp4",
            "audio/mpeg",
            "application/pdf",
        ] {
            assert!(!is_scriptable_document(m), "{m} must stay inline-able");
        }
    }

    #[test]
    fn charset_and_case_do_not_defeat_classification() {
        // `text/html; charset=utf-8` is what an upload actually carries,
        // and a mime is case-insensitive per RFC 9110. An earlier cut of
        // `normalize` only trimmed and split — `Text/HTML` classified as
        // SAFE and would have been served inline. This test caught it.
        assert!(is_scriptable_document("text/html; charset=utf-8"));
        assert!(is_scriptable_document("Text/HTML"));
        assert!(is_scriptable_document("  TEXT/HTML ; charset=UTF-8"));
        assert!(is_scriptable_document("IMAGE/SVG+XML"));
    }

    #[test]
    fn an_untyped_body_is_treated_as_hostile() {
        // No Content-Type is the body a browser is most likely to sniff
        // into something executable.
        assert!(is_scriptable_document(""));
        assert!(is_scriptable_document("   "));
    }

    #[test]
    fn the_policy_never_carries_an_allow_token() {
        // Mirrors the Hurl pin at the unit level. `allow-scripts` alone
        // re-enables execution; with `allow-same-origin` it restores the
        // reported vulnerability in full.
        assert!(!CSP_USER_CONTENT_SANDBOXED.contains("allow-"));
        assert!(CSP_USER_CONTENT_SANDBOXED.contains("sandbox"));
        assert!(CSP_USER_CONTENT_SANDBOXED.contains("default-src 'none'"));
    }

    #[test]
    fn every_user_content_type_gets_a_policy_and_only_documents_are_sandboxed() {
        // The pairing, asserted together so neither half can drift:
        // a document is sandboxed, media is not, and NOTHING comes back
        // without a policy.
        assert_eq!(csp_for("text/html"), CSP_USER_CONTENT_SANDBOXED);
        assert_eq!(csp_for("image/svg+xml"), CSP_USER_CONTENT_SANDBOXED);
        assert_eq!(csp_for(""), CSP_USER_CONTENT_SANDBOXED);
        assert_eq!(csp_for("image/png"), CSP_USER_CONTENT_MEDIA);
        assert_eq!(csp_for("application/pdf"), CSP_USER_CONTENT_MEDIA);
        assert_eq!(csp_for("video/mp4"), CSP_USER_CONTENT_MEDIA);
        // The media policy must never carry `default-src` — that is the
        // one directive that could stop a natively-rendered body from
        // displaying, which is the whole reason the two differ.
        assert!(!CSP_USER_CONTENT_MEDIA.contains("default-src"));
        assert!(!CSP_USER_CONTENT_MEDIA.contains("sandbox"));
    }
}
