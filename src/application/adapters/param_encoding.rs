//! RFC 6868 parameter value encoding, shared by the vCard (RFC 6350) and
//! iCalendar (RFC 5545) generators/parsers.
//!
//! A vCard/iCalendar parameter value must be double-quoted when it
//! contains a COMMA, SEMICOLON, or COLON (the characters that would
//! otherwise be ambiguous with the surrounding content-line grammar).
//! But a quoted value's grammar (`QSTR = DQUOTE *QSAFE-CHAR DQUOTE`)
//! still can't contain a literal DQUOTE, and has no way to represent an
//! embedded newline — RFC 6868 defines three caret-escape sequences to
//! carry them inside a quoted value:
//!
//! - `^n` — newline
//! - `^^` — a literal `^`
//! - `^'` — a literal `"` (DQUOTE)
//!
//! Any other `^x` sequence is left unchanged when decoding (RFC 6868
//! §3.2: "any other occurrence of ^ ... MUST be treated as … literal").

/// Encode a raw parameter value's special characters per RFC 6868. Only
/// meaningful once the value is going to be quoted — see
/// [`render_param_value`].
pub fn encode_param_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '^' => out.push_str("^^"),
            '"' => out.push_str("^'"),
            '\n' => out.push_str("^n"),
            '\r' => {} // Bare CR is not itself encodable; CRLF collapses to the `\n` case above.
            _ => out.push(ch),
        }
    }
    out
}

/// Decode RFC 6868 caret-escape sequences back to raw characters.
/// Unrecognized `^x` sequences pass through with the caret kept literal.
pub fn decode_param_value(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '^' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('n') => {
                out.push('\n');
                chars.next();
            }
            Some('^') => {
                out.push('^');
                chars.next();
            }
            Some('\'') => {
                out.push('"');
                chars.next();
            }
            _ => out.push('^'),
        }
    }
    out
}

/// Whether `value` must be double-quoted per the vCard (RFC 6350 §3.3)
/// / iCalendar (RFC 5545 §3.2) `param-value` grammar.
fn needs_quoting(value: &str) -> bool {
    value.contains(',') || value.contains(';') || value.contains(':')
}

/// Render `value` for a content-line parameter position: quoted and
/// RFC 6868-encoded if it contains characters the bare-token grammar
/// can't carry (comma/semicolon/colon triggering quoting, or a
/// DQUOTE/caret/newline that would break even a quoted value),
/// otherwise returned unchanged.
/// Applies to any parameter whose value uses the `param-value` grammar
/// (RFC 5545 §3.1 / RFC 6350 §3.3), not just TYPE — wrap every
/// free-text parameter value built from user input, not property
/// values (those use backslash escaping, not this caret scheme).
pub fn render_param_value(value: &str) -> String {
    let needs_encoding = value.contains('"') || value.contains('^') || value.contains('\n');
    if needs_quoting(value) || needs_encoding {
        format!("\"{}\"", encode_param_value(value))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_escapes_all_three_sequences() {
        assert_eq!(encode_param_value("a^b"), "a^^b");
        assert_eq!(encode_param_value("say \"hi\""), "say ^'hi^'");
        assert_eq!(encode_param_value("line1\nline2"), "line1^nline2");
    }

    #[test]
    fn encode_leaves_plain_text_unchanged() {
        assert_eq!(encode_param_value("Work Phone"), "Work Phone");
    }

    #[test]
    fn decode_reverses_encode_roundtrip() {
        let originals = [
            "a^b",
            "say \"hi\"",
            "line1\nline2",
            "^n literal caret then n",
        ];
        for original in originals {
            let encoded = encode_param_value(original);
            assert_eq!(decode_param_value(&encoded), original);
        }
    }

    #[test]
    fn decode_leaves_unrecognized_caret_sequences_literal() {
        // `^x` is not one of the three defined sequences — caret stays.
        assert_eq!(decode_param_value("a^xb"), "a^xb");
        // Trailing caret with nothing after it.
        assert_eq!(decode_param_value("trailing^"), "trailing^");
    }

    #[test]
    fn needs_quoting_detects_grammar_special_chars() {
        assert!(needs_quoting("Smith, John"));
        assert!(needs_quoting("a;b"));
        assert!(needs_quoting("a:b"));
        assert!(!needs_quoting("plain-value"));
    }

    #[test]
    fn render_param_value_quotes_only_when_needed() {
        assert_eq!(render_param_value("WORK"), "WORK");
        assert_eq!(render_param_value("Smith, John"), "\"Smith, John\"");
    }

    #[test]
    fn render_param_value_quotes_and_encodes_embedded_dquote() {
        // No comma/semicolon/colon, but the embedded DQUOTE alone forces
        // quoting (otherwise the bare token would itself be invalid).
        assert_eq!(render_param_value("say \"hi\""), "\"say ^'hi^'\"");
    }

    #[test]
    fn render_param_value_quotes_and_encodes_when_both_triggers_present() {
        assert_eq!(
            render_param_value("Smith, \"Johnny\""),
            "\"Smith, ^'Johnny^'\""
        );
    }
}
