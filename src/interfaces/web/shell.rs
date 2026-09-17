//! The SPA shell (`index.html`), prepared once at boot.
//!
//! The frontend build carries no deployment prefix: asset URLs are relative and
//! the document's `<base href>` is what anchors them. That tag is the only place
//! the prefix has to reach the browser, and it cannot be discovered client-side
//! — the browser resolves `<link rel="modulepreload">` and the bootstrap's
//! `import()` before any script of ours runs. So the server fills it in here,
//! from `OXICLOUD_BASE_PATH`, and the same build serves any prefix.
//!
//! Preparing the shell once also lets us drop the source comments: the shell is
//! `no-cache`, so it travels on every page load.

use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::Response;

use super::StaticSource;

/// Opening of the placeholder `frontend/src/app.html` ships; the whole tag is
/// replaced, however it was serialized.
const BASE_TAG_OPEN: &str = "<base href=\"";

/// The served `index.html`: base path injected, comments stripped.
#[derive(Debug, Clone)]
pub struct AppShell {
    html: String,
}

impl AppShell {
    /// Read the shell from the resolved static source and prepare it.
    ///
    /// `Ok(None)` means there is no built frontend — a dev setup serving the SPA
    /// from Vite, which boots fine without a shell.
    pub fn prepare(source: &StaticSource, base_path: &str) -> Result<Option<Self>, String> {
        let Some(raw) = read_shell(source) else {
            return Ok(None);
        };
        let html = inject_base(&raw, base_path)?;
        Ok(Some(Self {
            html: strip_comments(&html),
        }))
    }

    /// An `index.html` response. Compression is left to the layer in
    /// [`super::create_web_routes`], as for any other asset.
    pub fn response(&self) -> Response {
        let mut res = Response::new(Body::from(self.html.clone()));
        res.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        res
    }
}

fn read_shell(source: &StaticSource) -> Option<String> {
    match source {
        StaticSource::Filesystem(path) => std::fs::read_to_string(path.join("index.html")).ok(),
        #[cfg(feature = "bundled-assets")]
        StaticSource::Embedded => super::embedded::EmbeddedAssets::get("index.html")
            .and_then(|f| String::from_utf8(f.data.into_owned()).ok()),
    }
}

/// Point the shell's `<base href>` at the deployment prefix.
///
/// A missing placeholder means the shell was built by a frontend that predates
/// this mechanism, and every asset URL in it would be wrong — refuse to boot
/// rather than serve a page that cannot load itself.
fn inject_base(html: &str, base_path: &str) -> Result<String, String> {
    let Some(start) = html.find(BASE_TAG_OPEN) else {
        return Err(
            "SPA shell carries no <base href> tag — rebuild the frontend (npm run build) \
             with a version that supports OXICLOUD_BASE_PATH"
                .to_string(),
        );
    };
    let Some(len) = html[start..].find('>') else {
        return Err("SPA shell has an unterminated <base> tag".to_string());
    };
    let mut out = String::with_capacity(html.len() + base_path.len());
    out.push_str(&html[..start]);
    out.push_str(&format!("<base href=\"{base_path}/\">"));
    out.push_str(&html[start + len + 1..]);
    Ok(out)
}

/// Drop HTML comments, leaving `<script>` and `<style>` bodies alone — their
/// bytes feed the CSP hashes.
fn strip_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let comment = rest.find("<!--");
        let verbatim = ["<script", "<style"]
            .iter()
            .filter_map(|tag| rest.find(tag))
            .min();
        let comment_first = match (comment, verbatim) {
            (Some(c), Some(v)) => c < v,
            (Some(_), None) => true,
            (None, _) => false,
        };

        if comment_first {
            let c = comment.expect("comment_first implies a comment");
            out.push_str(&rest[..c]);
            match rest[c..].find("-->") {
                Some(end) => rest = &rest[c + end + 3..],
                // Unterminated comment: keep the remainder verbatim.
                None => {
                    out.push_str(&rest[c..]);
                    return out;
                }
            }
        } else if let Some(v) = verbatim {
            let close = if rest[v..].starts_with("<script") {
                "</script>"
            } else {
                "</style>"
            };
            match rest[v..].find(close) {
                Some(end) => {
                    let stop = v + end + close.len();
                    out.push_str(&rest[..stop]);
                    rest = &rest[stop..];
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            }
        } else {
            out.push_str(rest);
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_tag_carries_the_prefix() {
        let html = r#"<head><base href="/" /><title>x</title></head>"#;
        assert_eq!(
            inject_base(html, "/cloud").unwrap(),
            r#"<head><base href="/cloud/"><title>x</title></head>"#
        );
    }

    /// No prefix leaves the tag as the build shipped it.
    #[test]
    fn empty_prefix_keeps_the_root_base() {
        assert_eq!(
            inject_base(r#"<head><base href="/" /></head>"#, "").unwrap(),
            r#"<head><base href="/"></head>"#
        );
    }

    #[test]
    fn a_shell_without_the_placeholder_is_rejected() {
        let err = inject_base("<head></head>", "/cloud").unwrap_err();
        assert!(err.contains("rebuild the frontend"), "unhelpful: {err}");
    }

    #[test]
    fn comments_go_but_script_bodies_stay() {
        let html = "<head><!-- note --><script>const a = '<!-- not a comment -->';</script>\
                    <style>/* <!-- --> */</style><!-- tail --></head>";
        assert_eq!(
            strip_comments(html),
            "<head><script>const a = '<!-- not a comment -->';</script>\
             <style>/* <!-- --> */</style></head>"
        );
    }

    #[test]
    fn unterminated_markup_is_left_alone() {
        assert_eq!(strip_comments("<head><!-- open"), "<head><!-- open");
        assert_eq!(strip_comments("<head><script>x"), "<head><script>x");
    }
}
