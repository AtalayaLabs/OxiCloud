//! Small allocation-free text predicates shared across the hot parse paths.

/// ASCII case-insensitive substring test — the allocation-free equivalent of
/// `haystack_lower.contains(needle_lower)` when both are ASCII.
///
/// Callers pass an already-upper/lower-cased `needle` and get the same boolean
/// `haystack.to_ascii_uppercase().contains(NEEDLE)` would, without the
/// throwaway per-call `String`. Used by the search name-match classifier and by
/// `ContactService::parse_vcard`'s per-line `TYPE=` routing
/// (benches/ROUND20.md §A3).
pub fn ascii_ci_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Normalize an email address for **linking-equivalence comparison**
/// (NOT for storage — never modify what the user typed when persisting
/// or displaying).
///
/// Rules:
/// - Case-fold to ASCII lowercase (email addresses are treated as
///   case-insensitive in practice per RFC 5321 §2.4).
/// - Strip `+alias` sub-addressing from the local part:
///   `alice+github@example.com` → `alice@example.com`. Supported by
///   Gmail / Google Workspace, Outlook/O365 (since 2018), Fastmail
///   (since ~2020), and most modern providers. Safe for a 1:1
///   comparison — two same-user addresses normalise to the same value.
///
/// NOT doing:
/// - Dot-stripping (Gmail-only: `a.lice@gmail.com == alice@gmail.com`).
///   Applying universally would false-positive on providers that treat
///   dots as significant.
/// - Unicode normalisation — email addresses compare as ASCII already.
///
/// Load-bearing for `POST /api/auth/oidc/link/start` → callback and
/// for the auto-link decision on OIDC login. See
/// docs/plan/oidc-account-linking.md § Safety checks.
pub fn normalize_email_for_link(email: &str) -> String {
    let lower = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = lower.split_once('@') else {
        // Malformed — return the lowercased form; caller's comparison
        // will fail naturally.
        return lower;
    };
    let local_base = local.split_once('+').map(|(b, _)| b).unwrap_or(local);
    format!("{}@{}", local_base, domain)
}

/// Escape the five markup-significant characters, for interpolating an
/// untrusted value into XML or HTML — element text or a quoted
/// attribute, either one.
///
/// The one copy. There were three: `nextcloud::uploads_handler`,
/// `nextcloud::avatar_handler` and the WOPI host page each had a private
/// `xml_escape`/`escape_html_attr`, and two of them had already drifted
/// — one emitted `&apos;` for the apostrophe, the other `&#39;`.
///
/// `&#39;` is the form here: a numeric reference is valid in both XML
/// and HTML, whereas `&apos;` is undefined in HTML 4 and only became
/// legal in HTML5. Since these helpers escape values destined for SVG,
/// WebDAV XML and HTML alike, the universally-safe spelling is the one
/// worth standardising on.
///
/// Ampersand MUST be replaced first, or the `&` of a later replacement
/// gets re-escaped into `&amp;lt;`.
pub fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_uppercase_contains() {
        // Parity with the `to_ascii_uppercase().contains(NEEDLE)` shape it
        // replaced, across mixed case and the empty/oversize edge cases.
        let cases: &[(&str, &str)] = &[
            ("EMAIL;TYPE=home:a@b.com", "TYPE=HOME"),
            ("EMAIL;type=Work:a@b.com", "TYPE=WORK"),
            ("TEL;TYPE=CELL:+1", "TYPE=CELL"),
            ("TEL;TYPE=voice:+1", "TYPE=CELL"),
            ("ADR;TYPE=Home:;;x", "TYPE=WORK"),
            ("", "TYPE=HOME"),
            ("short", "a-very-long-needle"),
        ];
        for (hay, needle) in cases {
            let reference = hay.to_ascii_uppercase().contains(needle);
            assert_eq!(
                ascii_ci_contains(hay.as_bytes(), needle.as_bytes()),
                reference,
                "mismatch for haystack={hay:?} needle={needle:?}"
            );
        }
    }

    #[test]
    fn empty_needle_is_true() {
        assert!(ascii_ci_contains(b"anything", b""));
    }

    #[test]
    fn normalize_email_for_link_matrix() {
        // Behaviour matrix from docs/plan/oidc-account-linking.md
        // § Email normalization. Left = raw, right = expected normalized.
        let cases: &[(&str, &str)] = &[
            // Identity
            ("alice@example.com", "alice@example.com"),
            // Case fold
            ("Alice@Example.COM", "alice@example.com"),
            // +alias stripped
            ("alice+github@example.com", "alice@example.com"),
            ("alice+oidc@example.com", "alice@example.com"),
            // Both sides of a match normalise the same way
            ("alice+work@example.com", "alice@example.com"),
            // Empty +alias suffix is still stripped
            ("alice+@example.com", "alice@example.com"),
            // Multiple + in local: everything after the FIRST + is dropped
            ("alice+work+extra@example.com", "alice@example.com"),
            // Trim leading/trailing whitespace
            ("  alice@example.com  ", "alice@example.com"),
            // Different local parts stay different
            ("bob@example.com", "bob@example.com"),
            // Different domains stay different (no cross-domain equivalence)
            ("alice@corp.com", "alice@corp.com"),
            // Domain case-folded too
            ("alice@Example.COM", "alice@example.com"),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                normalize_email_for_link(raw),
                *expected,
                "normalize_email_for_link({raw:?}) should equal {expected:?}"
            );
        }
    }

    #[test]
    fn normalize_email_link_equivalence_pairs() {
        // Anti-drift: pairs that MUST compare equal after normalization
        // (the "auto-link email match" cases the plan doc lists as ✅).
        let equivalent: &[(&str, &str)] = &[
            ("alice@example.com", "alice@example.com"),
            ("alice@example.com", "Alice@example.com"),
            ("alice@example.com", "alice+oidc@example.com"),
            ("alice+work@example.com", "alice@example.com"),
            ("alice+work@example.com", "alice+home@example.com"),
        ];
        for (left, right) in equivalent {
            assert_eq!(
                normalize_email_for_link(left),
                normalize_email_for_link(right),
                "{left:?} should equal {right:?} under linking normalization"
            );
        }

        // Pairs that MUST NOT match — the plan's ❌ cases.
        let distinct: &[(&str, &str)] = &[
            ("alice@example.com", "bob@example.com"),
            ("alice@example.com", "alice@corp.com"),
            // Dot-stripping deliberately NOT applied — dots stay significant.
            ("a.lice@gmail.com", "alice@gmail.com"),
        ];
        for (left, right) in distinct {
            assert_ne!(
                normalize_email_for_link(left),
                normalize_email_for_link(right),
                "{left:?} MUST NOT equal {right:?} — dot-stripping is Gmail-only, we don't apply it"
            );
        }
    }

    #[test]
    fn normalize_email_malformed_returns_lowercased() {
        // No `@` → return lowercased trimmed form; the caller's
        // downstream comparison will fail naturally.
        assert_eq!(normalize_email_for_link("not-an-email"), "not-an-email");
        assert_eq!(normalize_email_for_link("  MIXED-Case  "), "mixed-case");
    }

    #[test]
    fn escape_markup_covers_all_five() {
        assert_eq!(
            escape_markup(r#"<a href="x" title='y'>&</a>"#),
            "&lt;a href=&quot;x&quot; title=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
        );
    }

    #[test]
    fn escape_markup_replaces_the_ampersand_first() {
        // The ordering bug this guards: if `&` were replaced LAST, each
        // earlier substitution's own `&` would be re-escaped — `<` would
        // come out as `&amp;lt;` instead of `&lt;`, and the markup the
        // escape exists to produce would render as literal text.
        //
        // Every character that expands to an entity is listed, not just
        // `<`: an asymmetric test reads as though the others are
        // unhandled, and the next person to add a character here should
        // see all of them in one place.
        for (raw, escaped) in [
            ("&", "&amp;"),
            ("<", "&lt;"),
            (">", "&gt;"),
            ("\"", "&quot;"),
            ("'", "&#39;"),
        ] {
            assert_eq!(escape_markup(raw), escaped, "escaping {raw:?}");
            // Feeding the OUTPUT back in must escape exactly one more
            // level — the direct check that no pass re-escapes another
            // pass's output.
            assert_eq!(
                escape_markup(escaped),
                escaped.replacen('&', "&amp;", 1),
                "re-escaping {escaped:?} must only touch its ampersand"
            );
        }
    }

    #[test]
    fn escape_markup_neutralises_an_attribute_breakout() {
        // The shape that matters for the WOPI host page: a value
        // interpolated into a quoted attribute must not be able to
        // close it and add its own.
        let hostile = r#"" onload="alert(1)"#;
        let escaped = escape_markup(hostile);
        assert!(!escaped.contains('"'));
        assert_eq!(escaped, "&quot; onload=&quot;alert(1)");
    }

    #[test]
    fn escape_markup_uses_the_numeric_apostrophe() {
        // `&apos;` is undefined in HTML 4; the numeric form is valid in
        // both XML and HTML. One of the three merged copies used
        // `&apos;` — this pins which spelling won.
        assert_eq!(escape_markup("'"), "&#39;");
    }

    #[test]
    fn escape_markup_leaves_ordinary_text_alone() {
        assert_eq!(
            escape_markup("Rapport 2026 (final).docx"),
            "Rapport 2026 (final).docx"
        );
        assert_eq!(escape_markup(""), "");
    }
}
