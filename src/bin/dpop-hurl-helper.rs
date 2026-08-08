//! DPoP wire-protocol test helper for the api-test suite.
//!
//! Hurl can't drive DPoP: every proof carries a fresh `jti`, a
//! current `iat`, an `htm`/`htu` matching the exact request, an
//! ES256 signature from a persistent browser-held keypair, and a
//! nonce threaded from the server's `DPoP-Nonce` response header.
//! A declarative `.hurl` template has no way to compute that per
//! request. Same problem OPAQUE has, same solution:
//! `opaque-hurl-helper.rs` (task #19) verifies OPAQUE end-to-end;
//! this binary does the same for DPoP.
//!
//! Invocation (from `tests/api/run.sh`):
//!
//! ```bash
//! OXICLOUD_DPOP_MODE=required
//! DPOP_HELPER_BASE_URL=$base_url \
//! DPOP_HELPER_USERNAME=$username \
//! DPOP_HELPER_PASSWORD=$password \
//!     ./target/debug/dpop-hurl-helper
//! ```
//!
//! Exit codes:
//!   * 0 — every scenario succeeded.
//!   * 1 — any scenario failed; diagnostic on stderr.
//!
//! Scope: this binary covers the wire contract for the currently-
//! integrated slice — verifier + nonce + replay + middleware in
//! opportunistic mode. Scenarios requiring session-context binding
//! (bound-vs-unbound enforcement per `session.dpop_jkt`, refresh
//! continuity, thumbprint mismatch vs stored) are deferred until
//! Gate 7 threads session context through the middleware.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64_URL_NO_PAD};
use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use rand_core::OsRng as OpaqueRng;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::process::ExitCode;

// Reuse the concrete ciphersuite the production `OpaqueService` uses —
// mismatching client + server suites would fail every handshake with
// a confusing error.
use oxicloud::infrastructure::services::opaque_service::OxiCloudSuite;

/// Server may emit URL_SAFE_NO_PAD *or* STANDARD base64; try both so
/// a future format flip doesn't silently break the round-trip.
fn decode_opaque_b64(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let s = input.trim();
    B64_URL_NO_PAD.decode(s).or_else(|_| B64.decode(s))
}

#[derive(serde::Deserialize)]
struct OpaqueParamsResp {
    enabled: bool,
    #[serde(rename = "ciphersuiteVersion")]
    _ciphersuite_version: i16,
    ksf: OpaqueKsfParams,
}

#[derive(serde::Deserialize)]
struct OpaqueKsfParams {
    #[serde(rename = "memoryKib")]
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

#[derive(serde::Deserialize)]
struct OpaqueKe1Resp {
    #[serde(rename = "exchangeId")]
    exchange_id: String,
    #[serde(rename = "loginResponse")]
    login_response: String,
}

const EXIT_FAIL: u8 = 1;

fn env_or_fail(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("dpop-hurl-helper: required env var {key} unset");
        std::process::exit(EXIT_FAIL as i32);
    })
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    // Bordered banner so a failure stands out against the interleaved
    // server audit log — the last line before this is usually the
    // server-side reject that caused it, which visually blends in.
    eprintln!();
    eprintln!("┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓");
    eprintln!("┃ dpop-hurl-helper: FAIL                                                      ┃");
    eprintln!("┃   {msg}");
    eprintln!("┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛");
    ExitCode::from(EXIT_FAIL)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_jti() -> String {
    // 16 bytes of OS randomness, base64url'd — plenty of entropy for
    // per-request uniqueness; the server's replay cache keys on this.
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    B64_URL_NO_PAD.encode(b)
}

/// A persistent-across-scenarios keypair — simulates one browser
/// tab whose IndexedDB entry survives every scenario in this run.
struct KeyBundle {
    signing_key: SigningKey,
    jwk_x_b64: String,
    jwk_y_b64: String,
}

impl KeyBundle {
    fn fresh() -> Self {
        // Deterministic-ish seed — this is a test tool, not a
        // security surface. Skipping `rand`-dep to keep the binary
        // dep footprint identical to what Gate 5 already added.
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i as u8).wrapping_mul(37)).wrapping_add(1);
        }
        let signing_key = SigningKey::from_bytes(&bytes.into()).expect("valid P-256 scalar");
        let vkey = signing_key.verifying_key();
        let enc = vkey.to_encoded_point(false);
        Self {
            signing_key,
            jwk_x_b64: B64_URL_NO_PAD.encode(enc.x().unwrap()),
            jwk_y_b64: B64_URL_NO_PAD.encode(enc.y().unwrap()),
        }
    }
}

/// Overrides for building malformed / tampered proofs, per scenario.
#[derive(Default, Clone)]
struct ProofOverrides<'a> {
    override_alg: Option<&'a str>,
    override_typ: Option<&'a str>,
    override_htm: Option<&'a str>,
    override_htu: Option<&'a str>,
    stale_iat: bool,
    /// If `Some`, use this exact string for the `jti` instead of a
    /// fresh random one — the replay scenario needs to reuse it.
    fixed_jti: Option<String>,
    /// If `Some`, include this literal `nonce` claim (even if it's
    /// wrong on purpose). If `None`, use whatever the caller
    /// tracked from `DPoP-Nonce` response headers.
    force_nonce: Option<String>,
}

/// Build + sign a DPoP proof against the given method + URL.
/// `nonce` is the current server-issued nonce (if any); `overrides`
/// let scenario code tamper with the proof shape.
fn build_proof(
    keys: &KeyBundle,
    method: &str,
    url: &str,
    nonce: Option<&str>,
    overrides: &ProofOverrides<'_>,
) -> String {
    let header = json!({
        "typ": overrides.override_typ.unwrap_or("dpop+jwt"),
        "alg": overrides.override_alg.unwrap_or("ES256"),
        "jwk": {
            "crv": "P-256",
            "kty": "EC",
            "x": keys.jwk_x_b64,
            "y": keys.jwk_y_b64,
        },
    });
    let iat = if overrides.stale_iat {
        now_secs() - 10_000
    } else {
        now_secs()
    };
    let mut claims = json!({
        "htm": overrides.override_htm.unwrap_or(method),
        "htu": overrides.override_htu.map(str::to_owned).unwrap_or_else(|| canonical_htu(url)),
        "iat": iat,
        "jti": overrides.fixed_jti.clone().unwrap_or_else(random_jti),
    });
    let nonce_to_use = overrides.force_nonce.as_deref().or(nonce);
    if let Some(n) = nonce_to_use {
        claims.as_object_mut().unwrap().insert(
            "nonce".to_string(),
            serde_json::Value::String(n.to_string()),
        );
    }

    let h_b64 = B64_URL_NO_PAD.encode(header.to_string());
    let p_b64 = B64_URL_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{h_b64}.{p_b64}");
    let sig: Signature = keys.signing_key.sign(signing_input.as_bytes());
    let s_b64 = B64_URL_NO_PAD.encode(sig.to_bytes());
    format!("{h_b64}.{p_b64}.{s_b64}")
}

fn canonical_htu(url: &str) -> String {
    let u = reqwest::Url::parse(url).expect("valid URL for htu");
    format!("{}://{}{}", u.scheme(), u.authority(), u.path())
}

/// Log in via OPAQUE, return the access + refresh tokens.
///
/// `opaque-hurl-helper` runs earlier in `tests/api/run.sh` and
/// mints the OPAQUE envelope for `admin`; from that point on the
/// account is migrated and legacy `POST /api/auth/login` refuses
/// with Phase-4 `opaque_migrated_use_opaque` (403). So this
/// helper drives the OPAQUE handshake directly — same ciphersuite
/// (`OxiCloudSuite`) and same `/params` KSF-fetch pattern as
/// `opaque-hurl-helper`. Long-term this is also what OPAQUE-only
/// mode (`docs/plan/opaque-only.md`) requires: legacy login is
/// on the way out entirely.
async fn opaque_login(
    http: &reqwest::Client,
    base: &str,
    username: &str,
    password: &str,
) -> Result<(String, String), String> {
    // Fetch server params so client-side Argon2 matches. Per Phase B
    // the envelope's OWN KSF is authoritative for that user (via
    // `/login/lookup`), but for a test admin whose envelope was
    // freshly minted by the OPAQUE helper the two match, so
    // `/params` is sufficient here.
    let params: OpaqueParamsResp = http
        .get(format!("{base}/api/auth/opaque/params"))
        .send()
        .await
        .map_err(|e| format!("opaque /params: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse /params: {e}"))?;
    if !params.enabled {
        return Err("server reports OPAQUE disabled — did opaque-hurl-helper run first?".into());
    }
    let ksf_params = argon2::Params::new(
        params.ksf.memory_kib,
        params.ksf.iterations,
        params.ksf.parallelism,
        None,
    )
    .map_err(|e| format!("build Argon2 params: {e}"))?;
    let ksf = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        ksf_params,
    );

    let mut rng = OpaqueRng;

    // ── KE1 — public endpoint, no bearer ─────────────────────────────
    let client_login = ClientLogin::<OxiCloudSuite>::start(&mut rng, password.as_bytes())
        .map_err(|e| format!("ClientLogin::start: {e}"))?;
    let ke1_body = json!({
        "userIdentifier": username,
        "startLoginRequest": B64.encode(client_login.message.serialize()),
    });
    let ke1_res = http
        .post(format!("{base}/api/auth/opaque/login/ke1"))
        .json(&ke1_body)
        .send()
        .await
        .map_err(|e| format!("ke1 POST: {e}"))?;
    if !ke1_res.status().is_success() {
        return Err(format!("ke1 returned {}", ke1_res.status()));
    }
    let ke1: OpaqueKe1Resp = ke1_res
        .json()
        .await
        .map_err(|e| format!("parse ke1: {e}"))?;
    let cred_bytes =
        decode_opaque_b64(&ke1.login_response).map_err(|e| format!("decode loginResponse: {e}"))?;
    let cred_response = CredentialResponse::<OxiCloudSuite>::deserialize(&cred_bytes)
        .map_err(|e| format!("deserialize CredentialResponse: {e}"))?;

    // ── KE3 — finish + submit ─────────────────────────────────────────
    let login_finish = client_login
        .state
        .finish(
            password.as_bytes(),
            cred_response,
            ClientLoginFinishParameters::new(None, opaque_ke::Identifiers::default(), Some(&ksf)),
        )
        .map_err(|e| format!("ClientLogin::finish (bad password?): {e}"))?;
    // URL_SAFE_NO_PAD on the wire — matches what the SPA sends and
    // what the server's handler prefers (accepts both, but this is
    // the canonical form).
    let ke3_body = json!({
        "exchangeId": ke1.exchange_id,
        "finishLoginRequest": B64_URL_NO_PAD.encode(login_finish.message.serialize()),
    });
    let ke3_res = http
        .post(format!("{base}/api/auth/opaque/login/ke3"))
        .json(&ke3_body)
        .send()
        .await
        .map_err(|e| format!("ke3 POST: {e}"))?;
    if !ke3_res.status().is_success() {
        return Err(format!("ke3 returned {}", ke3_res.status()));
    }
    let body: serde_json::Value = ke3_res.json().await.map_err(|e| format!("ke3 body: {e}"))?;
    let access = body["access_token"]
        .as_str()
        .ok_or("ke3 response missing access_token")?
        .to_string();
    let refresh = body["refresh_token"]
        .as_str()
        .ok_or("ke3 response missing refresh_token")?
        .to_string();
    Ok((access, refresh))
}

/// Send a GET request to `path` with a DPoP proof, following the
/// nonce-challenge retry loop. Returns the final response + the
/// nonce currently cached (which the caller threads into follow-up
/// requests).
async fn get_with_dpop(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    access_token: &str,
    keys: &KeyBundle,
    cached_nonce: Option<String>,
    overrides: &ProofOverrides<'_>,
) -> Result<(reqwest::Response, Option<String>), String> {
    let url = format!("{base}{path}");
    let proof = build_proof(keys, "GET", &url, cached_nonce.as_deref(), overrides);
    let res = http
        .get(&url)
        .bearer_auth(access_token)
        .header("DPoP", proof)
        .send()
        .await
        .map_err(|e| format!("GET {path}: {e}"))?;
    // Harvest DPoP-Nonce even on failure — the server stamps it
    // regardless so the client can retry with the fresh value.
    let updated_nonce = res
        .headers()
        .get("DPoP-Nonce")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or(cached_nonce);
    // Challenge-retry once — mirrors the SPA fetch interceptor.
    if res.status() == 401
        && res
            .headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("use_dpop_nonce"))
    {
        // Only ONE retry — a second challenge on the retry is a
        // server bug and should surface as-is.
        let proof2 = build_proof(keys, "GET", &url, updated_nonce.as_deref(), overrides);
        let res2 = http
            .get(&url)
            .bearer_auth(access_token)
            .header("DPoP", proof2)
            .send()
            .await
            .map_err(|e| format!("GET {path} (retry): {e}"))?;
        let updated_nonce2 = res2
            .headers()
            .get("DPoP-Nonce")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or(updated_nonce);
        return Ok((res2, updated_nonce2));
    }
    Ok((res, updated_nonce))
}

fn expect_status(scenario: &str, res: &reqwest::Response, want: u16) -> Result<(), String> {
    if res.status().as_u16() == want {
        Ok(())
    } else {
        Err(format!(
            "scenario {scenario}: expected HTTP {want}, got {}",
            res.status()
        ))
    }
}

// Defensive-programming pattern: every scenario refreshes the
// cached nonce so a later scenario picks up any rotation the
// server did in between. Scenarios that DON'T re-consume the
// cached nonce (e.g. #6 which passes a bogus value on purpose)
// look like dead assignments to the linter — silence it here
// rather than sprinkle `let _ = n` calls that obscure intent.
#[allow(unused_assignments)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let base = env_or_fail("DPOP_HELPER_BASE_URL");
    let username = env_or_fail("DPOP_HELPER_USERNAME");
    let password = env_or_fail("DPOP_HELPER_PASSWORD");
    let base = base.trim_end_matches('/');

    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail(format!("build reqwest client: {e}")),
    };

    // ── 1. Log in via OPAQUE (session created unbound — Gate 3
    //       requires the client to send `dpop_jkt` in the login
    //       body; this helper doesn't, so we get the fail-open
    //       unbound path). Bind support gets tested via
    //       `POST /api/auth/dpop/bind` in a later gate.
    //
    //       OPAQUE (not legacy) because `opaque-hurl-helper`
    //       migrates `admin` earlier in `run.sh`, after which
    //       legacy login 403s with Phase-4 refusal — and once
    //       OPAQUE-only mode ships (`docs/plan/opaque-only.md`)
    //       there IS no legacy path anyway.
    let (access, _refresh) = match opaque_login(&http, base, &username, &password).await {
        Ok(t) => t,
        Err(e) => return fail(e),
    };

    let keys = KeyBundle::fresh();
    let jkt = {
        // Compute expected thumbprint for logging — server ignores
        // at this gate but future scenarios will compare.
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            keys.jwk_x_b64, keys.jwk_y_b64
        );
        B64_URL_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
    };
    eprintln!("dpop-hurl-helper: keypair jkt={jkt}");

    let mut nonce: Option<String> = None;

    // ── Scenario 1: happy path — bootstrap (no nonce) → challenge
    //    → retry with nonce → 200. The retry loop is inside
    //    `get_with_dpop`.
    let (res, n) = match get_with_dpop(
        &http,
        base,
        "/api/auth/me",
        &access,
        &keys,
        nonce.clone(),
        &ProofOverrides::default(),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    if let Err(e) = expect_status("happy_path", &res, 200) {
        return fail(e);
    }
    if n.is_none() {
        return fail("happy_path: server did not stamp DPoP-Nonce on response");
    }
    nonce = n;
    eprintln!(
        "dpop-hurl-helper: scenario 1 (happy path) ✓  cached_nonce={:?}",
        nonce
    );

    // ── Scenario 2: wrong htm — sign for POST but send GET → 401
    let (res, n) = match get_with_dpop(
        &http,
        base,
        "/api/auth/me",
        &access,
        &keys,
        nonce.clone(),
        &ProofOverrides {
            override_htm: Some("POST"),
            ..Default::default()
        },
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    nonce = n;
    if let Err(e) = expect_status("wrong_htm", &res, 401) {
        return fail(e);
    }
    eprintln!("dpop-hurl-helper: scenario 2 (wrong htm) ✓");

    // ── Scenario 3: wrong htu — sign for /api/foo but send /api/auth/me → 401
    let (res, n) = match get_with_dpop(
        &http,
        base,
        "/api/auth/me",
        &access,
        &keys,
        nonce.clone(),
        &ProofOverrides {
            override_htu: Some("https://not-this-host.example/api/foo"),
            ..Default::default()
        },
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    nonce = n;
    if let Err(e) = expect_status("wrong_htu", &res, 401) {
        return fail(e);
    }
    eprintln!("dpop-hurl-helper: scenario 3 (wrong htu) ✓");

    // ── Scenario 4: wrong alg (RS256) — 401
    let (res, n) = match get_with_dpop(
        &http,
        base,
        "/api/auth/me",
        &access,
        &keys,
        nonce.clone(),
        &ProofOverrides {
            override_alg: Some("RS256"),
            ..Default::default()
        },
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    nonce = n;
    if let Err(e) = expect_status("wrong_alg", &res, 401) {
        return fail(e);
    }
    eprintln!("dpop-hurl-helper: scenario 4 (wrong alg) ✓");

    // ── Scenario 5: wrong typ — 401
    let (res, n) = match get_with_dpop(
        &http,
        base,
        "/api/auth/me",
        &access,
        &keys,
        nonce.clone(),
        &ProofOverrides {
            override_typ: Some("jwt"),
            ..Default::default()
        },
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    nonce = n;
    if let Err(e) = expect_status("wrong_typ", &res, 401) {
        return fail(e);
    }
    eprintln!("dpop-hurl-helper: scenario 5 (wrong typ) ✓");

    // ── Scenario 6: stale nonce — simulate the client's cached
    //    nonce having expired server-side (server restarted, or
    //    pool TTL elapsed). The server issues a challenge with a
    //    fresh nonce, and `get_with_dpop` MUST retry once using
    //    that fresh value from the response header — not the
    //    stale one from `cached_nonce`.
    //
    //    Pass the bogus value positionally (via `cached_nonce`)
    //    rather than through the `force_nonce` override — the
    //    override would survive the retry and cause a second
    //    challenge, defeating the recovery path we're testing.
    let (res, n) = match get_with_dpop(
        &http,
        base,
        "/api/auth/me",
        &access,
        &keys,
        Some("nonce-that-server-does-not-know".to_string()),
        &ProofOverrides::default(),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    if let Err(e) = expect_status("stale_nonce_challenge_retry", &res, 200) {
        return fail(e);
    }
    nonce = n;
    eprintln!("dpop-hurl-helper: scenario 6 (stale nonce → challenge → retry) ✓");

    // ── Scenario 7: replay — build a proof, send it TWICE with
    //    the same jti; second call must be replay-rejected.
    //    Uses a bespoke send (no retry-loop) so we control both
    //    submissions of the exact-same bytes.
    let fixed_jti = random_jti();
    let url = format!("{base}/api/auth/me");
    let proof_replay = build_proof(
        &keys,
        "GET",
        &url,
        nonce.as_deref(),
        &ProofOverrides {
            fixed_jti: Some(fixed_jti.clone()),
            ..Default::default()
        },
    );
    let res_first = match http
        .get(&url)
        .bearer_auth(&access)
        .header("DPoP", proof_replay.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return fail(format!("replay send 1: {e}")),
    };
    if let Err(e) = expect_status("replay_first_send", &res_first, 200) {
        return fail(e);
    }
    let res_second = match http
        .get(&url)
        .bearer_auth(&access)
        .header("DPoP", proof_replay)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return fail(format!("replay send 2: {e}")),
    };
    if let Err(e) = expect_status("replay_second_send", &res_second, 401) {
        return fail(e);
    }
    // The replay 401 is an invalid_dpop_proof shape (not use_dpop_nonce)
    let www_auth = res_second
        .headers()
        .get("WWW-Authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if www_auth.contains("use_dpop_nonce") {
        return fail(format!(
            "replay: WWW-Authenticate should be invalid_dpop_proof, was: {www_auth}"
        ));
    }
    eprintln!("dpop-hurl-helper: scenario 7 (replay) ✓");

    // ── Scenario 8: malformed JWS — send garbage in the DPoP
    //    header. Server-side verifier rejects at the split step.
    let res = match http
        .get(&url)
        .bearer_auth(&access)
        .header("DPoP", "not-a-real-jws")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return fail(format!("malformed send: {e}")),
    };
    if let Err(e) = expect_status("malformed", &res, 401) {
        return fail(e);
    }
    eprintln!("dpop-hurl-helper: scenario 8 (malformed) ✓");

    // ── Scenario 9: no proof at all on a bound-if-required path.
    //    In opportunistic mode this passes through; in required
    //    mode the session is unbound (`dpop_jkt IS NULL`) so it
    //    STILL passes through. Server-side gate 9 will flip this
    //    once session-context enforcement lands.
    let res_no_proof = match http.get(&url).bearer_auth(&access).send().await {
        Ok(r) => r,
        Err(e) => return fail(format!("no_proof send: {e}")),
    };
    if let Err(e) = expect_status("no_proof_unbound_session", &res_no_proof, 200) {
        return fail(e);
    }
    eprintln!("dpop-hurl-helper: scenario 9 (no proof, unbound session → pass) ✓");

    eprintln!("dpop-hurl-helper: all scenarios passed");
    ExitCode::SUCCESS
}
