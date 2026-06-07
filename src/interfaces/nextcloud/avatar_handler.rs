use axum::{
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use std::sync::Arc;

use crate::common::di::AppState;

/// Re-encode WebP image bytes as PNG. Returns `None` on decode/encode
/// failure (treated upstream as "fall through to SVG" — a bad stored
/// blob shouldn't break the rendering pipeline). PNG is universal:
/// every NC client surface (Qt desktop without `qtimageformats`,
/// older Android/iOS image stacks) decodes it. WebP, JPEG and GIF
/// pass through this function unchanged because they're either
/// already PNG-equivalent for compat purposes (JPEG/GIF) or the only
/// problematic format we know to convert (WebP).
fn webp_to_png(webp_bytes: &[u8]) -> Option<Vec<u8>> {
    use image::ImageFormat;
    use std::io::Cursor;
    let img = image::load_from_memory_with_format(webp_bytes, ImageFormat::WebP).ok()?;
    let mut out = Vec::with_capacity(webp_bytes.len());
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// Parse an `auth.users.image` data URI of the form
/// `data:<mime>;base64,<payload>` into its `(mime, decoded_bytes)`
/// parts.
///
/// Returns `None` when the input is not a data URI (e.g. an external
/// URL, a relative path, or anything we don't know how to render
/// inline) or when the base64 payload is malformed. Callers fall back
/// to the SVG initials avatar in that case — silently, since a bad
/// stored value shouldn't break the rendering pipeline for the
/// requesting client.
fn parse_data_uri(uri: &str) -> Option<(String, Vec<u8>)> {
    // `data:image/png;base64,iVBORw0KGgo…` — split at the first comma
    // because the payload can contain `=` padding which we must not
    // accidentally include in the header chunk.
    let rest = uri.strip_prefix("data:")?;
    let (header_part, payload) = rest.split_once(',')?;
    // We require the explicit `;base64` flag — Nextcloud's standard
    // image storage format. Plain (url-encoded) data URIs are
    // technically valid per RFC 2397 but in practice we never see
    // them from any OxiCloud write path.
    let header_part = header_part.strip_suffix(";base64")?;
    let mime = if header_part.is_empty() {
        "application/octet-stream".to_string()
    } else {
        header_part.to_string()
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()?;
    Some((mime, bytes))
}

/// GET /remote.php/dav/avatars/{user}/{size}.png
///
/// NextCloud-DAV-shaped avatar URL used by the NC desktop client
/// (and several mobile clients). When the user has a stored image
/// in `auth.users.image` (typically a `data:image/...;base64,…`
/// URI written by the profile-picture upload flow), the decoded
/// bytes are returned verbatim with their original MIME type.
/// Otherwise we fall through to the same SVG initials payload as
/// [`handle_avatar`] — Qt's `QImage` and the NC mobile image
/// stacks auto-detect format from `Content-Type`, so SVG-via-`.png`
/// URL is harmless in practice.
///
/// The `size` segment arrives as `"128.png"` so we strip the
/// suffix before parsing — a missing suffix is tolerated so this
/// also works if a client requests `/avatars/admin/128` against
/// the DAV route (some older clients omit the extension).
pub async fn handle_dav_avatar(
    state: State<Arc<AppState>>,
    Path((username, size_with_ext)): Path<(String, String)>,
) -> Response {
    let size_str = size_with_ext.strip_suffix(".png").unwrap_or(&size_with_ext);
    let size: u32 = size_str.parse().unwrap_or(64);
    handle_avatar(state, Path((username, size))).await
}

/// GET /index.php/avatar/{user}/{size}
///
/// Resolution order:
///   1. Look up the user; if `auth.users.image` holds a `data:…;base64,…`
///      URI, decode and serve the original bytes verbatim. This is the
///      profile-picture path used by NC desktop / mobile.
///   2. Otherwise (no row, no image, or unparseable value), generate an
///      SVG tile with the username's initials on a deterministic color.
///
/// Auth lookup failures fall through to the SVG so a transient DB
/// hiccup doesn't break the rendering pipeline for the requesting
/// client — the avatar is decorative, not security-critical.
pub async fn handle_avatar(
    State(state): State<Arc<AppState>>,
    Path((username, size)): Path<(String, u32)>,
) -> Response {
    let size = size.clamp(16, 1024);

    // ── Stored profile image — preferred when present ───────────
    if let Some(auth_svc) = state.auth_service.as_ref()
        && let Ok(user) = auth_svc
            .auth_application_service
            .get_user_by_username(&username)
            .await
        && let Some(image_uri) = user.image.as_deref()
        && let Some((mime, bytes)) = parse_data_uri(image_uri)
    {
        // WebP is OxiCloud's storage format of choice (smaller files,
        // better quality at a given size) but NextCloud clients have
        // patchy WebP support — older Qt-based desktop builds, some
        // mobile image stacks. Transcode to PNG before serving on the
        // NC surface so every client renders it. PNG is bigger on the
        // wire but small enough at avatar dimensions that the
        // tradeoff is worth it. Decode failure falls through to SVG.
        let (final_mime, final_bytes): (&str, Vec<u8>) = if mime == "image/webp" {
            match webp_to_png(&bytes) {
                Some(png) => ("image/png", png),
                None => return svg_initials_response(&username, size),
            }
        } else {
            // Whatever MIME we stored (`image/png`, `image/jpeg`,
            // `image/gif`) is universally supported by NC clients.
            // The `mime` String is moved out via `.as_str()` here, so
            // bind it locally to keep the borrow alive for the response.
            (mime_as_static_str(&mime), bytes)
        };
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, final_mime),
                // Shorter cache than the SVG fallback because users can
                // re-upload their picture at any time — the URL is the
                // same so a long immutable cache would pin the old one.
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            final_bytes,
        )
            .into_response();
    }

    svg_initials_response(&username, size)
}

/// Map a runtime MIME string to one of the static `&'static str`
/// variants we expose so the response header can borrow it for the
/// lifetime of the response.
///
/// Unknown / non-image MIMEs fall through to `application/octet-stream`
/// — clients still render the bytes via format-sniffing, but they
/// also know not to mis-trust the value.
fn mime_as_static_str(mime: &str) -> &'static str {
    match mime {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Build the SVG-initials avatar response. Extracted so the stored-image
/// path can fall through to the same payload on parse / decode failure
/// without duplicating the rendering code.
fn svg_initials_response(username: &str, size: u32) -> Response {
    let size = size.clamp(16, 1024);
    let initials = extract_initials(username);
    let color = pick_color(username);
    let font_size = (size as f32 * 0.45) as u32;
    let safe_initials = xml_escape(&initials);

    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{s}" height="{s}" viewBox="0 0 {s} {s}">
  <rect width="{s}" height="{s}" rx="{r}" fill="{c}"/>
  <text x="50%" y="50%" dy="0.36em" fill="#fff" font-family="-apple-system,BlinkMacSystemFont,sans-serif" font-size="{fs}" font-weight="600" text-anchor="middle">{i}</text>
</svg>"##,
        s = size,
        r = size / 2,
        c = color,
        fs = font_size,
        i = safe_initials,
    );

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'",
            ),
        ],
        svg,
    )
        .into_response()
}

/// Escape XML special characters to prevent XSS in SVG output.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn extract_initials(username: &str) -> String {
    let parts: Vec<&str> = username.split_whitespace().collect();
    match parts.len() {
        0 => "?".to_string(),
        1 => parts[0]
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string(),
        _ => {
            let first = parts[0].chars().next().unwrap_or('?');
            let last = parts[parts.len() - 1].chars().next().unwrap_or('?');
            format!("{}{}", first.to_uppercase(), last.to_uppercase())
        }
    }
}

fn pick_color(username: &str) -> &'static str {
    const PALETTE: [&str; 10] = [
        "#0082c9", "#e9322d", "#2d8a0f", "#c37200", "#6c2d9e", "#007a87", "#b02e7c", "#465a64",
        "#a65d00", "#3b5998",
    ];
    let hash: u32 = username
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hash as usize) % PALETTE.len()]
}
