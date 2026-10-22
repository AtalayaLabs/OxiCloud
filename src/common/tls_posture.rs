//! Tier-1 posture pin for the native-only TLS strategy.
//!
//! Every outbound TLS surface in OxiCloud (sqlx → PG, reqwest → OIDC /
//! webhooks / Azure / WOPI, aws-sdk-s3, lettre → SMTP, extism plugin
//! HTTP) must consume the OS native cert store, never bundled Mozilla
//! roots. See `src/AGENTS.md` § "Outbound TLS and trust store" for the
//! full rule and `docs/config/deployment.md` § "Outbound TLS and Trust
//! Store" for the operator-facing contract.
//!
//! Rust feature flags re-add bundled roots silently when a dep bump
//! re-enables an upstream default — nothing in a code review surfaces
//! that unless we look at the lockfile with intent. This test parses
//! `Cargo.toml` directly and fails if any of the known-bad feature
//! strings reappear.
//!
//! It is NOT a replacement for a full-stack HTTPS integration test
//! (Tier 3 in the discussion that preceded this file) — it only
//! catches the Cargo.toml side of the invariant. Full-stack coverage
//! lands only when there is a specific regression it would have caught
//! (per the "every regression gets its own coverage" rule in
//! `AGENTS.md`).

#[cfg(test)]
mod tests {
    /// The feature-string substrings that would silently reintroduce
    /// `webpki-roots` as a trust source if they appeared in
    /// `Cargo.toml`. Keep this list in sync with the per-crate rules
    /// in `src/AGENTS.md` § "Outbound TLS and trust store".
    const FORBIDDEN_FEATURE_STRINGS: &[&str] = &[
        // reqwest: `rustls-tls-webpki-roots` enables webpki as the
        // trust source. Our only acceptable rustls feature is
        // `rustls-tls-native-roots`.
        "\"rustls-tls-webpki-roots\"",
        // sqlx: `tls-rustls` is an alias for `tls-rustls-ring` which
        // in turn aliases `tls-rustls-ring-webpki`. Our only
        // acceptable sqlx TLS feature is
        // `tls-rustls-ring-native-roots` (or the aws-lc equivalent
        // if we ever adopt it).
        "\"tls-rustls-ring\"",
        "\"tls-rustls-ring-webpki\"",
        // The bare `tls-rustls` alias — same resolution chain, same
        // outcome, must stay out of Cargo.toml. (The quote-and-comma
        // suffix pins the match to a feature-list element so a
        // substring in a comment or a longer feature name doesn't
        // false-positive.)
        "\"tls-rustls\",",
        "\"tls-rustls\"]",
    ];

    /// Parse `Cargo.toml` and assert none of the known-bad feature
    /// strings appear. The failure message points the reviewer at the
    /// AGENTS.md rule.
    ///
    /// Why string-matching the raw TOML instead of parsing with
    /// `toml`: this test must run WITHOUT a dependency on `toml` as a
    /// dev-dep, and the invariant is literal — the exact substring
    /// `"rustls-tls-webpki-roots"` must never appear in a features
    /// array. A richer parse would also need per-crate rules
    /// (sqlx features vs reqwest features), which is more code for
    /// zero added catch — a bad string anywhere in Cargo.toml is a
    /// regression regardless of which crate opted into it.
    #[test]
    fn outbound_tls_crates_never_enable_webpki_roots() {
        let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("Cargo.toml at CARGO_MANIFEST_DIR is readable");

        let mut hits = Vec::new();
        for forbidden in FORBIDDEN_FEATURE_STRINGS {
            if toml.contains(forbidden) {
                hits.push(*forbidden);
            }
        }

        assert!(
            hits.is_empty(),
            "Cargo.toml enables one or more forbidden TLS feature flags: {hits:?}. \
             Each of these silently reintroduces bundled Mozilla roots \
             (webpki-roots) as a trust source. OxiCloud's posture requires the \
             OS native cert store ONLY — see src/AGENTS.md § 'Outbound TLS and \
             trust store' for the per-crate rules + docs/config/deployment.md \
             § 'Outbound TLS and Trust Store' for the operator-facing contract. \
             If this test is in your way, you are reintroducing a bug \
             (RUSTSEC-2026-0381 was exactly this failure mode: a distrusted \
             root in webpki-roots overriding the OS's correct distrust)."
        );
    }

    /// Belt-and-braces: confirm the acceptable native-roots feature
    /// strings DO appear. If a dep bump restructures the features and
    /// silently drops our opt-in, this fails loudly instead of
    /// defaulting the trust source back to webpki without a visible
    /// Cargo.toml diff.
    #[test]
    fn outbound_tls_crates_opt_into_native_roots() {
        let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("Cargo.toml at CARGO_MANIFEST_DIR is readable");

        let required: &[(&str, &str)] = &[
            ("sqlx", "\"tls-rustls-ring-native-roots\""),
            ("reqwest", "\"rustls-tls-native-roots\""),
            ("lettre", "\"rustls-native-certs\""),
        ];

        let missing: Vec<&str> = required
            .iter()
            .filter(|(_, s)| !toml.contains(s))
            .map(|(crate_name, _)| *crate_name)
            .collect();

        assert!(
            missing.is_empty(),
            "Cargo.toml is missing the native-roots opt-in for: {missing:?}. \
             These crates must explicitly request the OS native cert store \
             via their 'native-roots' / 'native-certs' feature; otherwise \
             they default back to webpki-roots. See src/AGENTS.md § \
             'Outbound TLS and trust store' for the exact feature strings."
        );
    }
}
