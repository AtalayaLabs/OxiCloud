//! DPoP proof verifier (RFC 9449) — pure functions over a compact JWS.
//!
//! Consumes a DPoP header value produced by
//! `frontend/src/lib/auth/dpop-proof.ts` (or the `dpop-hurl-helper`
//! test binary — Gate 6b) and returns a typed verdict.
//!
//! Nothing here touches the DB or the request extractor pipeline —
//! that's the middleware's job (see `src/interfaces/middleware/dpop.rs`).
//! Keeping the verifier pure makes the failure-mode matrix trivially
//! unit-testable: `verify(proof, method, htu, expected_jkt, now)`.
//!
//! **Nonce validation is a caller responsibility for now** — the
//! nonce claim is extracted and returned as part of the OK verdict,
//! but the caller (middleware) validates it against the nonce
//! service. Wiring lands in Gate 5b; at Gate 5 the middleware
//! ignores the nonce field (opportunistic path).
//!
//! Ciphersuite: **ES256 ONLY** (ECDSA P-256 + SHA-256). Any other
//! `alg` or `crv` is a hard reject — RFC 9449 §4 mandates support
//! for ES256 and we don't accept anything looser.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64_URL_NO_PAD;
use sha2::{Digest, Sha256};

/// Per-request context the verifier compares proof claims against.
#[derive(Debug, Clone)]
pub struct DpopRequestContext<'a> {
    /// Uppercase HTTP method (e.g. `"POST"`).
    pub htm: &'a str,
    /// Canonical target URL: `scheme://authority/path` — NO query, NO
    /// fragment. Middleware builds this from the external scheme +
    /// host (`X-Forwarded-*`-aware) + `OriginalUri` path.
    pub htu: &'a str,
    /// Server clock (unix seconds). Injected so tests can pin it
    /// deterministically.
    pub now_secs: i64,
    /// Session's stored thumbprint — set at login (`session.dpop_jkt`).
    /// When present, the proof's JWK thumbprint MUST match.
    pub expected_jkt: Option<&'a str>,
}

/// Successful verify outcome — the middleware may still need to
/// validate the nonce (Gate 5b) and jti (Gate 6, replay cache).
#[derive(Debug, Clone)]
pub struct DpopVerified {
    /// RFC 7638 JWK thumbprint of the proof's public key.
    /// Middleware compares to `session.dpop_jkt` (already done here
    /// when `expected_jkt` was set) and may audit-log this value.
    pub jkt: String,
    /// Nonce claim from the proof, if any. Bootstrap-only branch has
    /// `None` — the very first request per session precedes the
    /// server-issued nonce, so the client can't include it.
    pub nonce: Option<String>,
    /// Unique proof id — the replay cache in Gate 6 keys off this.
    pub jti: String,
    /// Claimed issue time (unix seconds) — informational when nonce
    /// is present (server clock is authoritative via nonce validity),
    /// bounded ±30s when no nonce yet (bootstrap branch).
    pub iat: i64,
}

/// Machine-readable failure reasons. Stringly matched by the middleware
/// for the audit `reason=` field — DO NOT rename variants without
/// coordinating with dashboards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpopVerifyError {
    /// JWS isn't three base64url segments, or a segment fails to
    /// decode, or the JSON header/claims fail to parse.
    Malformed,
    /// `typ` header field is not `"dpop+jwt"`.
    WrongTyp,
    /// `alg` header field is not `"ES256"`.
    WrongAlg,
    /// `jwk` header member is missing or not an EC/P-256 public key.
    WrongJwk,
    /// ECDSA signature does not verify over `header.payload`.
    SignatureInvalid,
    /// `htm` claim doesn't match the request method.
    WrongHtm,
    /// `htu` claim doesn't match the canonical request URL.
    WrongHtu,
    /// `iat` claim is missing / non-numeric.
    IatMissing,
    /// `iat` claim is outside the ±30s bootstrap window (no nonce yet).
    IatOutOfWindow,
    /// `jti` claim is missing / empty.
    JtiMissing,
    /// Proof's JWK thumbprint doesn't match `expected_jkt`.
    JktMismatch,
}

impl DpopVerifyError {
    /// Stable machine-readable reason string for audit lines.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Malformed => "malformed_jws",
            Self::WrongTyp => "wrong_typ",
            Self::WrongAlg => "wrong_alg",
            Self::WrongJwk => "wrong_jwk",
            Self::SignatureInvalid => "signature_invalid",
            Self::WrongHtm => "wrong_htm",
            Self::WrongHtu => "wrong_htu",
            Self::IatMissing => "iat_missing",
            Self::IatOutOfWindow => "iat_out_of_window",
            Self::JtiMissing => "jti_missing",
            Self::JktMismatch => "jkt_mismatch",
        }
    }
}

pub type DpopVerifyResult = Result<DpopVerified, DpopVerifyError>;

/// ±30s tolerance on the `iat` claim when NO nonce is present
/// (bootstrap branch). Once Gate 5b lands, requests carrying a
/// server-issued nonce bypass this check — nonce validity acts as
/// the authoritative freshness signal.
const IAT_BOOTSTRAP_TOLERANCE_SECS: i64 = 30;

/// Verify a DPoP proof against a request context. See file doc for
/// scope: nonce/jti/replay checks are the caller's responsibility.
pub fn verify(proof: &str, ctx: &DpopRequestContext<'_>) -> DpopVerifyResult {
    // ── 1. Split the compact JWS into three segments ─────────────
    let mut parts = proof.split('.');
    let (h_b64, p_b64, s_b64) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err(DpopVerifyError::Malformed),
    };

    let header_bytes = B64_URL_NO_PAD
        .decode(h_b64)
        .map_err(|_| DpopVerifyError::Malformed)?;
    let payload_bytes = B64_URL_NO_PAD
        .decode(p_b64)
        .map_err(|_| DpopVerifyError::Malformed)?;
    let signature = B64_URL_NO_PAD
        .decode(s_b64)
        .map_err(|_| DpopVerifyError::Malformed)?;

    // ── 2. Parse header + validate typ/alg/jwk ────────────────────
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).map_err(|_| DpopVerifyError::Malformed)?;

    if header.get("typ").and_then(|v| v.as_str()) != Some("dpop+jwt") {
        return Err(DpopVerifyError::WrongTyp);
    }
    if header.get("alg").and_then(|v| v.as_str()) != Some("ES256") {
        return Err(DpopVerifyError::WrongAlg);
    }
    let jwk = header.get("jwk").ok_or(DpopVerifyError::WrongJwk)?;
    if jwk.get("kty").and_then(|v| v.as_str()) != Some("EC") {
        return Err(DpopVerifyError::WrongJwk);
    }
    if jwk.get("crv").and_then(|v| v.as_str()) != Some("P-256") {
        return Err(DpopVerifyError::WrongJwk);
    }
    let x_b64 = jwk
        .get("x")
        .and_then(|v| v.as_str())
        .ok_or(DpopVerifyError::WrongJwk)?;
    let y_b64 = jwk
        .get("y")
        .and_then(|v| v.as_str())
        .ok_or(DpopVerifyError::WrongJwk)?;

    // ── 3. Verify the ECDSA signature ─────────────────────────────
    // JWS ES256 signature is raw R||S (64 bytes for P-256), NOT DER
    // — RFC 7515 A.3. `p256::ecdsa::Signature::from_slice` accepts
    // exactly that layout.
    let x_bytes = B64_URL_NO_PAD
        .decode(x_b64)
        .map_err(|_| DpopVerifyError::WrongJwk)?;
    let y_bytes = B64_URL_NO_PAD
        .decode(y_b64)
        .map_err(|_| DpopVerifyError::WrongJwk)?;
    if x_bytes.len() != 32 || y_bytes.len() != 32 {
        return Err(DpopVerifyError::WrongJwk);
    }
    // SEC1 uncompressed point: 0x04 || X || Y.
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x_bytes);
    sec1.extend_from_slice(&y_bytes);

    use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
    let vkey = VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| DpopVerifyError::WrongJwk)?;
    let sig = Signature::from_slice(&signature).map_err(|_| DpopVerifyError::SignatureInvalid)?;

    // Signing input is EXACT bytes: base64url(header) || '.' ||
    // base64url(payload). Preserve the caller's encoding — do NOT
    // re-encode, since serde_json re-serialisation may reorder
    // members and break signature.
    let signing_input = format!("{h_b64}.{p_b64}");
    vkey.verify(signing_input.as_bytes(), &sig)
        .map_err(|_| DpopVerifyError::SignatureInvalid)?;

    // ── 4. Parse claims + validate htm/htu/iat/jti ────────────────
    let claims: serde_json::Value =
        serde_json::from_slice(&payload_bytes).map_err(|_| DpopVerifyError::Malformed)?;

    let htm = claims.get("htm").and_then(|v| v.as_str()).unwrap_or("");
    if !htm.eq_ignore_ascii_case(ctx.htm) {
        return Err(DpopVerifyError::WrongHtm);
    }
    let htu = claims.get("htu").and_then(|v| v.as_str()).unwrap_or("");
    if htu != ctx.htu {
        return Err(DpopVerifyError::WrongHtu);
    }
    let iat = claims
        .get("iat")
        .and_then(|v| v.as_i64())
        .ok_or(DpopVerifyError::IatMissing)?;
    let jti = claims
        .get("jti")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or(DpopVerifyError::JtiMissing)?
        .to_string();
    let nonce = claims
        .get("nonce")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    // iat freshness check: authoritative only when NO nonce is
    // present (bootstrap branch). Gate 5b will bypass this when a
    // nonce is available — nonce validity is server-clock-based, so
    // it moots any client-clock skew.
    if nonce.is_none() && (iat - ctx.now_secs).abs() > IAT_BOOTSTRAP_TOLERANCE_SECS {
        return Err(DpopVerifyError::IatOutOfWindow);
    }

    // ── 5. Compute JWK thumbprint (RFC 7638 §3.2 EC members) ──────
    let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x_b64}","y":"{y_b64}"}}"#,);
    let jkt = B64_URL_NO_PAD.encode(Sha256::digest(canonical.as_bytes()));

    if let Some(expected) = ctx.expected_jkt
        && expected != jkt
    {
        return Err(DpopVerifyError::JktMismatch);
    }

    Ok(DpopVerified {
        jkt,
        nonce,
        jti,
        iat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};

    /// Deterministic-ish signing key for tests — derives 32 bytes from
    /// a seed byte so each test can hold its own without pulling in
    /// `rand` as a dev-dep. Any value 1..=127 works (P-256 scalar
    /// must be non-zero and < curve order); we spread bytes over the
    /// buffer so keys with adjacent seeds don't share high bits.
    fn test_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8).wrapping_add(1);
        }
        SigningKey::from_bytes(&bytes.into()).expect("valid P-256 scalar")
    }

    /// Build a signed DPoP proof for testing — mirrors what
    /// `frontend/src/lib/auth/dpop-proof.ts` produces on the client.
    #[allow(clippy::too_many_arguments)]
    fn make_proof(
        signing_key: &SigningKey,
        htm: &str,
        htu: &str,
        iat: i64,
        jti: &str,
        nonce: Option<&str>,
        override_alg: Option<&str>,
        override_typ: Option<&str>,
    ) -> (String, String) {
        let vkey = signing_key.verifying_key();
        let encoded = vkey.to_encoded_point(false); // uncompressed
        let x = encoded.x().unwrap();
        let y = encoded.y().unwrap();
        let x_b64 = B64_URL_NO_PAD.encode(x);
        let y_b64 = B64_URL_NO_PAD.encode(y);

        let header = serde_json::json!({
            "typ": override_typ.unwrap_or("dpop+jwt"),
            "alg": override_alg.unwrap_or("ES256"),
            "jwk": { "crv": "P-256", "kty": "EC", "x": x_b64, "y": y_b64 },
        });
        let mut claims = serde_json::json!({
            "htm": htm,
            "htu": htu,
            "iat": iat,
            "jti": jti,
        });
        if let Some(n) = nonce {
            claims.as_object_mut().unwrap().insert(
                "nonce".to_string(),
                serde_json::Value::String(n.to_string()),
            );
        }

        let h_b64 = B64_URL_NO_PAD.encode(header.to_string());
        let p_b64 = B64_URL_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{h_b64}.{p_b64}");
        let sig: Signature = signing_key.sign(signing_input.as_bytes());
        let s_b64 = B64_URL_NO_PAD.encode(sig.to_bytes());
        let proof = format!("{h_b64}.{p_b64}.{s_b64}");

        // Compute canonical thumbprint the same way verify() does
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x_b64}","y":"{y_b64}"}}"#,);
        let jkt = B64_URL_NO_PAD.encode(Sha256::digest(canonical.as_bytes()));

        (proof, jkt)
    }

    fn ctx<'a>(
        htm: &'a str,
        htu: &'a str,
        now: i64,
        expected_jkt: Option<&'a str>,
    ) -> DpopRequestContext<'a> {
        DpopRequestContext {
            htm,
            htu,
            now_secs: now,
            expected_jkt,
        }
    }

    #[test]
    fn accepts_happy_path() {
        let sk = test_key(1);
        let (proof, jkt) = make_proof(
            &sk,
            "GET",
            "https://oxi.example/api/me",
            1_000_000,
            "jti-1",
            None,
            None,
            None,
        );
        let out = verify(
            &proof,
            &ctx("GET", "https://oxi.example/api/me", 1_000_000, Some(&jkt)),
        )
        .unwrap();
        assert_eq!(out.jkt, jkt);
        assert_eq!(out.jti, "jti-1");
        assert_eq!(out.nonce, None);
    }

    #[test]
    fn rejects_wrong_htm() {
        let sk = test_key(1);
        let (proof, jkt) = make_proof(&sk, "POST", "https://x/a", 1_000_000, "j", None, None, None);
        let err = verify(&proof, &ctx("GET", "https://x/a", 1_000_000, Some(&jkt))).unwrap_err();
        assert_eq!(err, DpopVerifyError::WrongHtm);
        assert_eq!(err.reason(), "wrong_htm");
    }

    #[test]
    fn rejects_wrong_htu() {
        let sk = test_key(1);
        let (proof, jkt) = make_proof(&sk, "GET", "https://x/a", 1_000_000, "j", None, None, None);
        let err = verify(&proof, &ctx("GET", "https://x/b", 1_000_000, Some(&jkt))).unwrap_err();
        assert_eq!(err, DpopVerifyError::WrongHtu);
    }

    #[test]
    fn rejects_wrong_alg() {
        let sk = test_key(1);
        let (proof, _jkt) = make_proof(
            &sk,
            "GET",
            "https://x/a",
            1_000_000,
            "j",
            None,
            Some("RS256"),
            None,
        );
        // Wrong alg reject fires BEFORE signature verify (alg is a
        // header field we check first). No expected_jkt needed —
        // we don't get that far.
        let err = verify(&proof, &ctx("GET", "https://x/a", 1_000_000, None)).unwrap_err();
        assert_eq!(err, DpopVerifyError::WrongAlg);
    }

    #[test]
    fn rejects_wrong_typ() {
        let sk = test_key(1);
        let (proof, _jkt) = make_proof(
            &sk,
            "GET",
            "https://x/a",
            1_000_000,
            "j",
            None,
            None,
            Some("jwt"),
        );
        let err = verify(&proof, &ctx("GET", "https://x/a", 1_000_000, None)).unwrap_err();
        assert_eq!(err, DpopVerifyError::WrongTyp);
    }

    #[test]
    fn rejects_expired_iat_when_no_nonce() {
        let sk = test_key(1);
        let (proof, jkt) = make_proof(&sk, "GET", "https://x/a", 1_000_000, "j", None, None, None);
        // now = iat + 60 → outside ±30s tolerance
        let err = verify(&proof, &ctx("GET", "https://x/a", 1_000_060, Some(&jkt))).unwrap_err();
        assert_eq!(err, DpopVerifyError::IatOutOfWindow);
    }

    #[test]
    fn accepts_stale_iat_when_nonce_present() {
        let sk = test_key(1);
        let (proof, jkt) = make_proof(
            &sk,
            "GET",
            "https://x/a",
            1_000_000,
            "j",
            Some("srv-nonce"),
            None,
            None,
        );
        // now = iat + 10 minutes → would fail bootstrap check, but
        // nonce is present → freshness check delegated to nonce
        // validity (Gate 5b), so we accept here.
        let out = verify(&proof, &ctx("GET", "https://x/a", 1_000_600, Some(&jkt))).unwrap();
        assert_eq!(out.nonce.as_deref(), Some("srv-nonce"));
    }

    #[test]
    fn rejects_jkt_mismatch() {
        let sk = test_key(1);
        let (proof, _jkt) = make_proof(&sk, "GET", "https://x/a", 1_000_000, "j", None, None, None);
        let err = verify(
            &proof,
            &ctx(
                "GET",
                "https://x/a",
                1_000_000,
                Some("some-other-thumbprint-value"),
            ),
        )
        .unwrap_err();
        assert_eq!(err, DpopVerifyError::JktMismatch);
    }

    #[test]
    fn rejects_bad_signature_when_payload_tampered() {
        let sk = test_key(1);
        let (proof, jkt) = make_proof(&sk, "GET", "https://x/a", 1_000_000, "j", None, None, None);
        // Corrupt the middle segment (payload) — signature will no
        // longer verify against the tampered signing-input bytes.
        let mut parts: Vec<&str> = proof.split('.').collect();
        parts[1] = "bm90LWEtcmVhbC1wYXlsb2Fk"; // "not-a-real-payload"
        let tampered = parts.join(".");
        let err = verify(&tampered, &ctx("GET", "https://x/a", 1_000_000, Some(&jkt))).unwrap_err();
        assert_eq!(err, DpopVerifyError::SignatureInvalid);
    }

    #[test]
    fn rejects_malformed_jws() {
        // Only 2 segments
        let err = verify("aa.bb", &ctx("GET", "https://x/a", 0, None)).unwrap_err();
        assert_eq!(err, DpopVerifyError::Malformed);
        // 4 segments
        let err = verify("a.b.c.d", &ctx("GET", "https://x/a", 0, None)).unwrap_err();
        assert_eq!(err, DpopVerifyError::Malformed);
        // Bad base64
        let err = verify("!!.??.@@", &ctx("GET", "https://x/a", 0, None)).unwrap_err();
        assert_eq!(err, DpopVerifyError::Malformed);
    }

    #[test]
    fn missing_jti_is_rejected() {
        // Build a proof with an empty jti — verify() rejects because
        // it's essential for the replay cache to key on.
        let sk = test_key(1);
        let (proof, jkt) = make_proof(&sk, "GET", "https://x/a", 1_000_000, "", None, None, None);
        let err = verify(&proof, &ctx("GET", "https://x/a", 1_000_000, Some(&jkt))).unwrap_err();
        assert_eq!(err, DpopVerifyError::JtiMissing);
    }
}
