//! Conditional-request handling (RFC 7232) for CalDAV / CardDAV object
//! writes — `PUT` and `DELETE` on `.ics` / `.vcf` resources.
//!
//! RFC 4791 §5.3.2 and RFC 6352 §6.3.2 rely on `If-Match` (overwrite only
//! the version the client last saw) and `If-None-Match: *` (create only,
//! never overwrite). Without them a client replaying a stale local copy
//! silently clobbers newer server data, so both handlers evaluate the
//! headers here before touching storage. Requests carrying neither header
//! are unaffected (no extra lookup, unchanged write semantics).
//!
//! ETag comparison reuses the WebDAV handler's RFC 7232 helpers so every
//! DAV surface agrees on the rules: quoted or bare tags, comma-separated
//! lists (and repeated header lines), `*`; `If-Match` is a strong
//! comparison, so a `W/` validator is parsed but never satisfies it
//! (OxiCloud only ever emits strong ETags, so a conforming client never
//! sends one), while `If-None-Match` compares weakly.

use axum::http::{HeaderMap, HeaderName, header};

use super::webdav_handler::{if_match_precondition_fails, if_none_match_precondition_fails};
use crate::interfaces::errors::AppError;

/// The conditional headers of one request, captured before the body is
/// consumed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct DavPreconditions {
    if_match: Option<String>,
    if_none_match: Option<String>,
}

impl DavPreconditions {
    pub(super) fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            if_match: joined_header(headers, header::IF_MATCH),
            if_none_match: joined_header(headers, header::IF_NONE_MATCH),
        }
    }

    /// `true` when the request carries no conditional header — the caller
    /// can then skip resolving the current ETag entirely.
    pub(super) fn is_empty(&self) -> bool {
        self.if_match.is_none() && self.if_none_match.is_none()
    }

    /// Evaluate against the target object's current ETag (`None` when the
    /// object does not exist). Order per RFC 7232 §6: `If-Match` first,
    /// then `If-None-Match`; for the unsafe methods handled here either
    /// failure is `412 Precondition Failed`.
    pub(super) fn check(&self, current_etag: Option<&str>) -> Result<(), AppError> {
        if let Some(if_match) = &self.if_match
            && if_match_precondition_fails(if_match, current_etag)
        {
            return Err(AppError::precondition_failed(match current_etag {
                None => "If-Match: the target resource does not exist",
                Some(_) => "If-Match: the resource has been modified (ETag mismatch)",
            }));
        }
        if let Some(if_none_match) = &self.if_none_match
            && if_none_match_precondition_fails(if_none_match, current_etag)
        {
            return Err(AppError::precondition_failed(
                "If-None-Match: the target resource already exists",
            ));
        }
        Ok(())
    }
}

/// All values of a (list-valued) header joined into one comma-separated
/// string, so a client splitting the list over several header lines is
/// handled like a single line. A value that is not visible ASCII is kept
/// (lossily) — it can never match a stored ETag, so `If-Match` then fails
/// closed instead of being ignored.
fn joined_header(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

/// The iCalendar UID of the calendar object in a PUT body: the `UID` of the
/// first top-level `VEVENT` / `VTODO` (nested components such as `VALARM`
/// may carry their own UID per RFC 9074 and are skipped). Content lines are
/// unfolded first (RFC 5545 §3.1) so a long, folded UID is read whole.
///
/// This is the UID the storage layer keys the object on, so it identifies
/// the object a PUT actually overwrites.
pub(super) fn ical_object_uid(ical: &str) -> Option<String> {
    let mut stack: Vec<String> = Vec::new();
    for line in unfold_lines(ical) {
        let (name, value) = match split_content_line(&line) {
            Some(parts) => parts,
            None => continue,
        };
        if name.eq_ignore_ascii_case("BEGIN") {
            stack.push(value.trim().to_ascii_uppercase());
        } else if name.eq_ignore_ascii_case("END") {
            stack.pop();
        } else if name.eq_ignore_ascii_case("UID")
            && matches!(stack.last().map(String::as_str), Some("VEVENT" | "VTODO"))
        {
            let uid = value.trim();
            if !uid.is_empty() {
                return Some(uid.to_string());
            }
        }
    }
    None
}

/// Unfold RFC 5545 / RFC 6350 content lines: a line starting with a space
/// or tab continues the previous one (the single whitespace is dropped).
fn unfold_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match raw.as_bytes().first() {
            Some(b' ' | b'\t') if !lines.is_empty() => {
                if let Some(last) = lines.last_mut() {
                    last.push_str(&raw[1..]);
                }
            }
            _ => lines.push(raw.to_string()),
        }
    }
    lines
}

/// Split a content line into property name (without parameters) and value.
fn split_content_line(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let head = &line[..colon];
    let name = head.split(';').next().unwrap_or(head).trim();
    Some((name, &line[colon + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, StatusCode};

    fn headers(pairs: &[(HeaderName, &str)]) -> DavPreconditions {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        DavPreconditions::from_headers(&map)
    }

    fn status(result: Result<(), AppError>) -> Option<StatusCode> {
        result.err().map(|e| e.status_code)
    }

    const ETAG: &str = "6f1c1b0e-3f8a-4c55-9f43-2f9d6c2a7b10";

    #[test]
    fn no_headers_never_blocks() {
        let pre = headers(&[]);
        assert!(pre.is_empty());
        assert!(pre.check(None).is_ok());
        assert!(pre.check(Some(ETAG)).is_ok());
    }

    #[test]
    fn if_match_with_current_etag_passes() {
        let quoted = format!("\"{ETAG}\"");
        assert!(
            headers(&[(header::IF_MATCH, &quoted)])
                .check(Some(ETAG))
                .is_ok()
        );
        // Bare (unquoted) tags, as some clients send them, also match.
        assert!(
            headers(&[(header::IF_MATCH, ETAG)])
                .check(Some(ETAG))
                .is_ok()
        );
    }

    #[test]
    fn if_match_with_stale_etag_is_412() {
        let pre = headers(&[(header::IF_MATCH, "\"old-etag\"")]);
        assert!(!pre.is_empty());
        assert_eq!(
            status(pre.check(Some(ETAG))),
            Some(StatusCode::PRECONDITION_FAILED)
        );
    }

    #[test]
    fn if_match_on_missing_object_is_412() {
        assert_eq!(
            status(headers(&[(header::IF_MATCH, "\"x\"")]).check(None)),
            Some(StatusCode::PRECONDITION_FAILED)
        );
        assert_eq!(
            status(headers(&[(header::IF_MATCH, "*")]).check(None)),
            Some(StatusCode::PRECONDITION_FAILED)
        );
    }

    #[test]
    fn if_match_star_and_lists() {
        assert!(
            headers(&[(header::IF_MATCH, "*")])
                .check(Some(ETAG))
                .is_ok()
        );
        let list = format!("\"a\", \"{ETAG}\"");
        assert!(
            headers(&[(header::IF_MATCH, &list)])
                .check(Some(ETAG))
                .is_ok()
        );
        // List split across header lines.
        let line = format!("\"{ETAG}\"");
        assert!(
            headers(&[(header::IF_MATCH, "\"a\""), (header::IF_MATCH, &line)])
                .check(Some(ETAG))
                .is_ok()
        );
        assert_eq!(
            status(headers(&[(header::IF_MATCH, "\"a\", \"b\"")]).check(Some(ETAG))),
            Some(StatusCode::PRECONDITION_FAILED)
        );
    }

    #[test]
    fn if_match_weak_validator_never_satisfies() {
        let weak = format!("W/\"{ETAG}\"");
        assert_eq!(
            status(headers(&[(header::IF_MATCH, &weak)]).check(Some(ETAG))),
            Some(StatusCode::PRECONDITION_FAILED)
        );
    }

    #[test]
    fn if_none_match_star_on_existing_object_is_412() {
        let pre = headers(&[(header::IF_NONE_MATCH, "*")]);
        assert_eq!(
            status(pre.check(Some(ETAG))),
            Some(StatusCode::PRECONDITION_FAILED)
        );
        assert!(pre.check(None).is_ok());
    }

    #[test]
    fn if_none_match_etag_list() {
        let quoted = format!("\"{ETAG}\"");
        assert_eq!(
            status(headers(&[(header::IF_NONE_MATCH, &quoted)]).check(Some(ETAG))),
            Some(StatusCode::PRECONDITION_FAILED)
        );
        // Weak comparison for If-None-Match.
        let weak = format!("W/\"{ETAG}\"");
        assert_eq!(
            status(headers(&[(header::IF_NONE_MATCH, &weak)]).check(Some(ETAG))),
            Some(StatusCode::PRECONDITION_FAILED)
        );
        assert!(
            headers(&[(header::IF_NONE_MATCH, "\"other\"")])
                .check(Some(ETAG))
                .is_ok()
        );
    }

    #[test]
    fn if_match_is_evaluated_before_if_none_match() {
        let quoted = format!("\"{ETAG}\"");
        let pre = headers(&[
            (header::IF_MATCH, &quoted),
            (header::IF_NONE_MATCH, "\"other\""),
        ]);
        assert!(pre.check(Some(ETAG)).is_ok());
    }

    #[test]
    fn ical_uid_from_vtodo_skips_valarm_and_unfolds() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nEND:VTIMEZONE\r\nBEGIN:VTODO\r\nBEGIN:VALARM\r\nUID:alarm-uid\r\nACTION:DISPLAY\r\nEND:VALARM\r\nUID:task-123456789012345678901234567890123456789012345678901234567890\r\n abcdef\r\nSUMMARY:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            ical_object_uid(body).as_deref(),
            Some("task-123456789012345678901234567890123456789012345678901234567890abcdef")
        );
    }

    #[test]
    fn ical_uid_from_vevent_with_params_and_lf_only() {
        let body =
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nuid;X-FOO=bar: ev-1 \nEND:VEVENT\nEND:VCALENDAR\n";
        assert_eq!(ical_object_uid(body).as_deref(), Some("ev-1"));
        assert_eq!(ical_object_uid("BEGIN:VCALENDAR\nEND:VCALENDAR\n"), None);
    }
}
