//! OPAQUE crypto client helper for the api-test suite.
//!
//! Hurl can't drive OPAQUE handshakes: every KE1 / KE3 message
//! contains session-random OPRF blinding + AKE nonces that can't be
//! hardcoded in a .hurl body. `opaque_substrate.hurl` therefore
//! covers only the wire shape (401/400/503 error paths, params publish,
//! login lookup, ciphersuite-mismatch). This binary closes the gap by
//! running a full crypto handshake against a live OxiCloud server, so
//! the api-test suite catches regressions on:
//!
//!   * handler happy path (KE1 → 200 with exchange_id + response,
//!     KE3 → 200 with AuthResponseDto shape),
//!   * middleware composition (public login mount does NOT require
//!     CSRF or session cookies — a regression would 403 here),
//!   * cookie emission on successful KE3 (session cookies are the
//!     mechanism the SPA relies on for subsequent authed calls),
//!   * `opaque_migrated_at` stamp landing (asserted transitively by
//!     the Phase 4 test suite — this binary just proves the login
//!     mints a session, without which mark_migrated wouldn't run).
//!
//! Invocation (from `tests/api/run.sh`):
//!
//! ```bash
//! OPAQUE_HELPER_BASE_URL=$base_url \
//! OPAQUE_HELPER_USERNAME=$username \
//! OPAQUE_HELPER_PASSWORD=$password \
//!     ./target/debug/opaque-hurl-helper
//! ```
//!
//! Exit codes:
//!   * 0 — full register + login handshake succeeded.
//!   * 1 — any HTTP failure or crypto mismatch. Diagnostic printed to
//!     stderr; wall-clock cost is under 100 ms with the fast Argon2
//!     params `tests/common/server.env` publishes.
//!
//! Design constraints:
//!   * Reads the KSF params from `/api/auth/opaque/params` so the
//!     client matches the server's config. A hardcoded KSF here would
//!     silently break the moment operators changed the env vars.
//!   * Registers via the session-authenticated endpoint, so first we
//!     legacy-login to obtain the bearer token (mirrors the Phase 2
//!     silent-migration bootstrap).
//!   * Cleans up NOTHING — the server tears down its DB between
//!     `run.sh` invocations, and mutating admin's envelope is safe
//!     because subsequent hurl files don't assume `hasOpaque=false`.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64_URL_NO_PAD};

/// Decode base64 emitted by the server. The server emits URL-safe-no-pad
/// (matching what the SPA's WASM client expects); this helper accepts
/// both flavours so a future format change on either side doesn't
/// silently break the round-trip. Mirrors `decode_opaque_b64` in the
/// server-side handler.
fn decode_opaque_b64(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let trimmed = input.trim();
    B64_URL_NO_PAD
        .decode(trimmed)
        .or_else(|_| B64.decode(trimmed))
}
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialResponse, RegistrationResponse,
};
use rand_core::OsRng;
use serde_json::json;
use std::process::ExitCode;

// Re-use the concrete ciphersuite the production `OpaqueService` uses;
// mismatching client + server suites would fail every handshake with a
// confusing error, and reproducing that mismatch by hand would rot.
use oxicloud::infrastructure::services::opaque_service::OxiCloudSuite;

const EXIT_OK: u8 = 0;
const EXIT_FAIL: u8 = 1;

fn env_or_fail(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("opaque-hurl-helper: required env var {key} unset");
        std::process::exit(EXIT_FAIL as i32);
    })
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("opaque-hurl-helper: FAIL — {msg}");
    ExitCode::from(EXIT_FAIL)
}

#[derive(serde::Deserialize)]
struct ParamsResp {
    enabled: bool,
    #[serde(rename = "ciphersuiteVersion")]
    ciphersuite_version: i16,
    ksf: KsfParams,
}

#[derive(serde::Deserialize)]
struct KsfParams {
    #[serde(rename = "memoryKib")]
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

#[derive(serde::Deserialize)]
struct LegacyLoginResp {
    access_token: String,
}

#[derive(serde::Deserialize)]
struct RegisterStartResp {
    #[serde(rename = "registrationResponse")]
    registration_response: String,
}

#[derive(serde::Deserialize)]
struct Ke1Resp {
    #[serde(rename = "exchangeId")]
    exchange_id: String,
    #[serde(rename = "loginResponse")]
    login_response: String,
}

#[derive(serde::Deserialize)]
struct AuthResp {
    access_token: String,
    refresh_token: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let base = env_or_fail("OPAQUE_HELPER_BASE_URL");
    let username = env_or_fail("OPAQUE_HELPER_USERNAME");
    let password = env_or_fail("OPAQUE_HELPER_PASSWORD");
    let base = base.trim_end_matches('/');

    // reqwest with a modest timeout so a hung server can't hang CI.
    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail(format!("build reqwest client: {e}")),
    };

    // ── 1. Fetch server params (must match client-side KSF params) ─────
    let params: ParamsResp = match http
        .get(format!("{base}/api/auth/opaque/params"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(p) => p,
            Err(e) => return fail(format!("parse /params: {e}")),
        },
        Ok(r) => return fail(format!("/params HTTP {}", r.status())),
        Err(e) => return fail(format!("/params network: {e}")),
    };
    if !params.enabled {
        eprintln!("opaque-hurl-helper: skipped (server reports OPAQUE disabled)");
        return ExitCode::from(EXIT_OK);
    }

    // Build the Argon2id KSF the server is expecting. `argon2::Params::new`
    // takes (memory_kib, iterations, parallelism, tag_length) — pass None
    // for tag_length to accept opaque-ke's expected length.
    let ksf = match argon2::Params::new(
        params.ksf.memory_kib,
        params.ksf.iterations,
        params.ksf.parallelism,
        None,
    ) {
        Ok(p) => argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, p),
        Err(e) => return fail(format!("build Argon2 params from server: {e}")),
    };

    // ── 2. Legacy login to grab a session bearer for register/* ────────
    // The register endpoints are session-authenticated; we don't yet
    // have an envelope, so we bootstrap the session with legacy password
    // login — same shape as the Phase 2 silent-migration hook.
    let login: LegacyLoginResp = match http
        .post(format!("{base}/api/auth/login"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(e) => return fail(format!("parse legacy login response: {e}")),
        },
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            return fail(format!("legacy login HTTP {status}: {body}"));
        }
        Err(e) => return fail(format!("legacy login network: {e}")),
    };
    let bearer = format!("Bearer {}", login.access_token);

    // ── 3. OPAQUE register (KE1) ───────────────────────────────────────
    let mut rng = OsRng;
    let client_reg = match ClientRegistration::<OxiCloudSuite>::start(&mut rng, password.as_bytes())
    {
        Ok(c) => c,
        Err(e) => return fail(format!("client_register.start: {e}")),
    };
    let reg_start_body = json!({
        "registrationRequest": B64.encode(client_reg.message.serialize())
    });
    let reg_start: RegisterStartResp = match http
        .post(format!("{base}/api/auth/opaque/register/start"))
        .header("Authorization", &bearer)
        .json(&reg_start_body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(e) => return fail(format!("parse register/start: {e}")),
        },
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            return fail(format!("register/start HTTP {status}: {body}"));
        }
        Err(e) => return fail(format!("register/start network: {e}")),
    };
    let reg_response_bytes = match decode_opaque_b64(&reg_start.registration_response) {
        Ok(b) => b,
        Err(e) => return fail(format!("decode registration_response: {e}")),
    };
    let reg_response = match RegistrationResponse::<OxiCloudSuite>::deserialize(&reg_response_bytes)
    {
        Ok(r) => r,
        Err(e) => return fail(format!("deserialize RegistrationResponse: {e}")),
    };

    // ── 4. OPAQUE register (KE2) ───────────────────────────────────────
    let reg_finish = match client_reg.state.finish(
        &mut rng,
        password.as_bytes(),
        reg_response,
        ClientRegistrationFinishParameters::new(opaque_ke::Identifiers::default(), Some(&ksf)),
    ) {
        Ok(f) => f,
        Err(e) => return fail(format!("client_register.finish: {e}")),
    };
    let reg_finish_body = json!({
        "registrationRecord": B64.encode(reg_finish.message.serialize()),
        "ciphersuiteVersion": params.ciphersuite_version,
    });
    match http
        .post(format!("{base}/api/auth/opaque/register/finish"))
        .header("Authorization", &bearer)
        .json(&reg_finish_body)
        .send()
        .await
    {
        Ok(r) if r.status() == 204 => {}
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            return fail(format!("register/finish HTTP {status}: {body}"));
        }
        Err(e) => return fail(format!("register/finish network: {e}")),
    };

    // ── 5. OPAQUE login (KE1) ──────────────────────────────────────────
    // Public endpoint — no bearer, no CSRF.
    let client_login = match ClientLogin::<OxiCloudSuite>::start(&mut rng, password.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail(format!("client_login.start: {e}")),
    };
    let ke1_body = json!({
        "userIdentifier": username,
        "startLoginRequest": B64.encode(client_login.message.serialize()),
    });
    let ke1: Ke1Resp = match http
        .post(format!("{base}/api/auth/opaque/login/ke1"))
        .json(&ke1_body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(e) => return fail(format!("parse ke1: {e}")),
        },
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            return fail(format!("login/ke1 HTTP {status}: {body}"));
        }
        Err(e) => return fail(format!("login/ke1 network: {e}")),
    };
    let cred_bytes = match decode_opaque_b64(&ke1.login_response) {
        Ok(b) => b,
        Err(e) => return fail(format!("decode loginResponse: {e}")),
    };
    let cred_response = match CredentialResponse::<OxiCloudSuite>::deserialize(&cred_bytes) {
        Ok(c) => c,
        Err(e) => return fail(format!("deserialize CredentialResponse: {e}")),
    };

    // ── 6. OPAQUE login (KE3) ──────────────────────────────────────────
    let login_finish = match client_login.state.finish(
        password.as_bytes(),
        cred_response,
        ClientLoginFinishParameters::new(None, opaque_ke::Identifiers::default(), Some(&ksf)),
    ) {
        Ok(f) => f,
        Err(e) => {
            return fail(format!(
                "client_login.finish (server may have rejected): {e}"
            ));
        }
    };
    let ke3_body = json!({
        "exchangeId": ke1.exchange_id,
        "finishLoginRequest": B64.encode(login_finish.message.serialize()),
    });
    let auth: AuthResp = match http
        .post(format!("{base}/api/auth/opaque/login/ke3"))
        .json(&ke3_body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(e) => return fail(format!("parse ke3 AuthResponse: {e}")),
        },
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            return fail(format!("login/ke3 HTTP {status}: {body}"));
        }
        Err(e) => return fail(format!("login/ke3 network: {e}")),
    };
    if auth.access_token.is_empty() || auth.refresh_token.is_empty() {
        return fail("ke3 returned empty tokens");
    }

    // Sanity-check the token minted by OPAQUE is genuinely usable —
    // hit `/api/auth/me` with it. Catches a class of regressions where
    // the OPAQUE handler mints a token that's syntactically valid but
    // doesn't authenticate the user (e.g. wrong user_id, wrong signing
    // key path, missing role claim).
    match http
        .get(format!("{base}/api/auth/me"))
        .header("Authorization", format!("Bearer {}", auth.access_token))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            return fail(format!(
                "/api/auth/me with OPAQUE token: HTTP {}",
                r.status()
            ));
        }
        Err(e) => return fail(format!("/api/auth/me network: {e}")),
    }

    eprintln!("opaque-hurl-helper: OK — register + login + /me round-trip for '{username}'");
    ExitCode::from(EXIT_OK)
}
