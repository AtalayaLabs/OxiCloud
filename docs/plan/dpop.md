# DPoP (Demonstrating Proof-of-Possession) implementation plan

**Status**: draft — not yet started.
**Companion**: `docs/plan/opaque-only.md` (OPAQUE is orthogonal but complementary — OPAQUE authenticates the user, DPoP binds the resulting session to a specific browser).

## Motivation

OxiCloud's current session-binding stack:

1. `HttpOnly` cookies (JS can't read the token)
2. `SameSite=Strict` (cross-site can't send the cookie)
3. `Secure` (HTTPS-only in production)
4. CSRF double-submit (`X-CSRF-Token` header echoing a same-origin cookie)
5. `Session.user_agent` recorded (for the sessions-list UI — **not verified per request**)

This stack defends well against XSS-exfiltration and cross-site forgery. It does **not** defend against **local host compromise** — an info-stealer that reads Chrome's `Cookies` SQLite DB via the OS keyring (DPAPI on Windows, Keychain on macOS) walks away with a working session that replays from anywhere. This is the dominant threat for a self-hosted cloud used from mixed-trust endpoints.

DPoP (RFC 9449) closes this gap by binding the session to a browser-held private key. Every request carries a JWT signed with that key. Stealing the cookie without the private key gets you nothing.

## Design decisions locked upfront

**Scope**: **DPoP-lite** for OxiCloud's own session cookies (OPAQUE, OIDC, magic-link, admin-password). NOT full RFC 9449 resource-server mode or OIDC-bearer binding — those are Phase 2 (deferred, see end).

**Crypto**: **ES256** (ECDSA P-256 + SHA-256). Universal `SubtleCrypto` support, RFC 9449 mandatory-to-support, ~90-byte public keys, ~50µs verify on modern CPUs.

**Anti-replay + clock-independence**: **DPoP-Nonce** (RFC 9449 §8). Server issues an opaque nonce in a response header; client MUST include it as a `nonce` claim in subsequent proofs. Server rotates the nonce every few minutes. Because the nonce is server-generated with a server-known issue time, **the client clock never appears in the trust chain** — no ±30s skew tolerance needed, no dead-mobile-clock failures.

**Storage**:
- **Browser**: single `IndexedDB` entry per origin holding a `CryptoKey` created with `extractable: false`. JS can call `sign()` on the handle but never `exportKey()`. The raw bytes live in the browser's crypto subsystem, at rest encrypted by the browser's per-profile key store.
- **Server**: `auth.sessions` gains a nullable `dpop_jkt VARCHAR(64)` column holding the JWK thumbprint (RFC 7638, base64url-encoded SHA-256 of the canonical JWK).

**Threat targets**:

| Attack | Result under DPoP |
|---|---|
| Info-stealer copies cookies to attacker's machine | ✅ Attacker has cookie but no private key → 401 on first request |
| DB backup leaked / dumped | ✅ Attacker gets thumbprints (public), useless for forgery |
| XSS reads `document.cookie` | Already blocked by `HttpOnly` — DPoP neither helps nor hurts |
| Same-origin XSS calls the fetch interceptor | ⚠️ Attacker's script signs its own requests through the interceptor. Mitigation is CSP hardening, not DPoP |
| Browser process compromised at login time | ❌ Attacker enrolls their own keypair. Only WebAuthn-attested keys close this — out of scope |
| Malicious extension with `webRequest` permission | ❌ Extension can wrap `fetch`. Same as above; browser-trust is a prerequisite |

**Feature flag**: `OXICLOUD_DPOP_MODE ∈ {off, opportunistic, required}`
- `off` (default in dev): middleware pass-through, client sends nothing.
- `opportunistic` (staged rollout): if proof present → verify; if absent → allow. Server logs `dpop.header_missing_but_session_bound` when a bound session skips a proof, so operators can spot broken clients before flipping to `required`.
- `required` (final): sessions with `dpop_jkt IS NOT NULL` MUST present a valid proof. Sessions with `dpop_jkt IS NULL` (app passwords, legacy pre-DPoP sessions) remain exempt.

**Non-goals for Phase 1**:
- Native Nextcloud sync clients (mobile/desktop) — Basic Auth over app passwords, no `SubtleCrypto`. Exempt via `dpop_jkt IS NULL`.
- OIDC bearer tokens minted by an upstream IdP — separate downstream problem, needs IdP cooperation.
- Multi-device attested keys via WebAuthn — major UX shift.

---

## Gate 0 — Design record

- This document, plus a lightweight `docs/adr/dpop-crypto-choice.md` pinning ES256 + DPoP-Nonce + `htu` canonicalisation rules.
- No code.
- **Deliverable**: PR-reviewable design doc.

## Gate 1 — Schema + DTO plumbing

- Migration `<timestamp>_dpop_session_binding.sql`: `ALTER TABLE auth.sessions ADD COLUMN dpop_jkt VARCHAR(64)` (nullable).
- Extend `Session` domain entity: `dpop_jkt: Option<String>`.
- Extend `SessionRepository::create_session` signature + PG implementation (append column to `INSERT`, expose `Option<&str>` in the trait).
- No middleware, no verification, no client changes.
- **Test**: existing session tests unchanged (thumbprint stays `None`); a new test writes a fake thumbprint and reads it back.
- **Rollback**: `ALTER TABLE ... DROP COLUMN dpop_jkt` — nothing depends on it yet.

## Gate 2 — Client keypair lifecycle

- New module `frontend/src/lib/auth/dpop.ts`:
  - `ensureKeypair(): Promise<CryptoKeyPair>` — read from IndexedDB (`db: "oxicloud-dpop"`, store: `"keypair"`), else generate P-256 with `extractable: false`, persist, return.
  - `computeJkt(pubKey: CryptoKey): Promise<string>` — export public key JWK, canonicalise (RFC 7638), SHA-256, base64url — the thumbprint.
  - `clearKeypair(): Promise<void>` — deletes the IndexedDB entry; called from logout.
  - Concurrency: wrap the ensure path in `navigator.locks.request("dpop-keypair", ...)` so two tabs opened simultaneously don't race to generate two keypairs.
- Zero server changes in this gate — keypair exists only in the browser.
- **Test**: Vitest with `fake-indexeddb`, verify that a second `ensureKeypair()` call in the same session returns the SAME `CryptoKey` handle (identity), and that the JWK thumbprint is stable across page reloads.

## Gate 3 — Bind ceremony on login

- Fold the thumbprint into the login request body (single round trip, no separate bind endpoint):
  - OPAQUE `POST /api/auth/opaque/login/ke3`: DTO gains `dpop_jkt: Option<String>`.
  - OIDC callback `GET/POST /api/auth/oidc/callback`: harder — the browser redirects through the IdP, so the thumbprint has to survive the redirect. Two options:
    - (a) Include it in the `state` param (base64url-encoded JSON, signed).
    - (b) Post-callback: server issues a temporary "unbound" session; client immediately calls `POST /api/auth/dpop/bind` with the thumbprint and gets a bound cookie back.
    - **Pick (b)** — simpler, doesn't inflate the `state` param, keeps OIDC parity across IdPs. Adds one round trip to OIDC login only.
  - Magic-link exchange `POST /api/auth/magic-link/redeem`: DTO gains `dpop_jkt: Option<String>`.
  - Legacy password `POST /api/auth/login`: DTO gains `dpop_jkt: Option<String>`.
- SPA changes: each `login*()` helper in `frontend/src/lib/api/endpoints/auth.ts` calls `ensureKeypair()` + `computeJkt()` before dispatch, threads the thumbprint into the body.
- Server: validate JKT is well-formed (43 chars, base64url); persist as `session.dpop_jkt`. Field ABSENCE is not a rejection at this gate — only Gate 5 in `required` mode enforces presence.
- **Test**: Hurl scenario per login path — POST with a fake JKT, then `SELECT dpop_jkt FROM auth.sessions WHERE id = <token>` returns exactly the sent value.
- **Rollback**: server ignores the extra field, client stops sending it. No lingering state.

## Gate 4 — Fetch interceptor emits DPoP header (nonce-aware)

- Extend `frontend/src/lib/api/client.ts::apiFetch`:
  - Before each request, load the keypair via `ensureKeypair()`.
  - Build the DPoP JWT:
    - **Header**: `{typ: "dpop+jwt", alg: "ES256", jwk: <public-key-JWK>}`
    - **Claims**: `{htm: <method>, htu: <canonical URL, no query>, iat: <now-seconds>, jti: <crypto.randomUUID()>, nonce: <current-nonce-or-omitted>}`
  - Sign with `crypto.subtle.sign({name: "ECDSA", hash: "SHA-256"}, privateKey, payload)`.
  - Set `DPoP: <compact-JWT>` header.
- **Nonce handling**:
  - Maintain a per-origin `currentNonce: string | null` in a module-level state (mirror in `sessionStorage` so cross-tab reads work).
  - Every response with a `DPoP-Nonce` header updates `currentNonce`.
  - On any 401 with `WWW-Authenticate: DPoP error="use_dpop_nonce"`, extract the fresh nonce from `DPoP-Nonce`, update `currentNonce`, retry the ORIGINAL request ONCE with the new nonce. Reject the response if the retry also fails.
  - First request in a fresh browser has `currentNonce = null` → server issues a challenge → client retries with nonce → done. One extra round trip per session bootstrap.
- **No server-side verification yet** in this gate — server logs when it sees the header for observability. Nonce issuance lives in Gate 5.
- **Test**: Vitest with a real ES256 keypair — verify the emitted JWT parses, signature validates, `htu`/`htm` match, `jti` is unique per request, `nonce` is included when set.

## Gate 5 — Server-side verifier + middleware (opportunistic mode)

- New `src/infrastructure/services/dpop_verifier.rs`:
  - Parse the `DPoP` header as JWS compact serialization (3 base64url segments).
  - Extract the JWK from the JWS header. Reject if `alg != "ES256"`, `typ != "dpop+jwt"`, `jwk.kty != "EC"`, `jwk.crv != "P-256"`.
  - Verify the JWS signature using the embedded public key (`p256::ecdsa::Signature::from_der(...).verify(...)` — see Gate 5 dependency note).
  - Validate claims:
    - `htm` == request method
    - `htu` == canonical URL (`scheme://authority/path`, no query, external scheme/host from `X-Forwarded-*` if behind a proxy — mirror the same helper the request-span uses for `client_ip`)
    - `iat` — informational only when nonce present; when nonce absent (very first request), fall back to ±30s tolerance
    - `nonce` — validated by the nonce service (Gate 5b)
    - `jti` — replay-cache lookup (Gate 6), scoped to nonce
  - Compute JWK thumbprint (RFC 7638), compare to `session.dpop_jkt`. Mismatch → 401 with `reason = "jkt_mismatch"`.
- New middleware `require_dpop_layer` in `src/interfaces/middleware/dpop.rs`:
  - Reads `Arc<AppState>` for the verifier + nonce service + mode flag.
  - Behaviour by mode:
    - `off` → pass through.
    - `opportunistic` → if header present, verify (401 on failure); if absent, pass. If `session.dpop_jkt IS NOT NULL` but header absent → log `dpop.header_missing_but_session_bound`.
    - `required` → header MUST be present and verify, OR `session.dpop_jkt IS NULL` (exempt).
  - Response shape on failure: 401 with `WWW-Authenticate: DPoP error="<invalid_dpop_proof|use_dpop_nonce>"` and `{error_type: "DpopVerificationFailed"}`.
- Mount on the same `/api/*` subtrees as `require_no_password_change_pending_layer`.
- Exempt paths (allowlist, not on wildcard): `/api/auth/login/*`, `/api/auth/opaque/register/*`, `/api/auth/oidc/callback`, `/api/auth/dpop/bind` (Gate 3 endpoint), and public discovery endpoints.
- **Test**: Rust unit tests for the verifier (happy path, wrong-alg, wrong-htm, wrong-htu, expired-iat when nonce absent, mismatched-jkt, malformed-JWS). Hurl scenarios for each mode.
- **Dependency check**: confirm the `jsonwebtoken` crate supports ES256 with an embedded JWK in the header. If not, use `p256` + `base64` + `serde_json` and hand-parse the compact serialization (~50 LoC, more control, no dep surprise).

## Gate 5b — DPoP-Nonce service

- New `src/infrastructure/services/dpop_nonce_service.rs`:
  - **Nonce format**: 32 random bytes, base64url-encoded (~43 chars).
  - **Store**: moka `Cache<String, NonceMeta>` (in-memory, per-instance). No PG persistence — nonces are ephemeral by design; on server restart clients fetch a new one via the challenge flow.
  - **Lifetime**: 5 minutes rolling window (`max_time_to_live = 5min`). Store `issued_at`.
  - **Reuse policy**: nonces are REUSABLE within their lifetime — one round trip per session bootstrap, not one per request. Replay protection is per-`jti` within a nonce (Gate 6).
  - **Rotation**: on any response, if the current session's nonce is older than 2 minutes, issue a fresh nonce via `DPoP-Nonce` response header. Client's fetch interceptor picks it up automatically. This gives an overlap window (client's cached nonce is still valid for 3 more minutes while it starts using the fresh one) so requests in flight during rotation don't fail.
  - **Challenge on missing/stale**: if `verify()` finds `nonce` claim absent OR not in the store OR expired → return 401 + `WWW-Authenticate: DPoP error="use_dpop_nonce"` + `DPoP-Nonce: <fresh-nonce>` response header. Client's Gate 4 retry logic handles the round trip.
- Cap the cache size (moka LRU max 100k entries by default) to bound memory under attack.
- **Test**: Rust unit test — issue nonce, verify accepts within window, rejects after expiry; issuing a fresh nonce doesn't invalidate the previous one until its own TTL.
- **Why nonce eliminates client-clock dependence**: the nonce is generated at a server-known moment and expires by server clock. A proof carrying that nonce is provably "recent" from the server's own perspective, regardless of what the client's clock says. `iat` becomes advisory (useful for logs, ignored for freshness) unless the client hasn't yet received a nonce (the very first request).

## Gate 6 — Replay cache (nonce-scoped)

- Moka `Cache<(String /* nonce */, String /* jti */), ()>` with TTL = 5 minutes (matches max nonce lifetime).
- Every verified proof inserts `(nonce, jti)`. If the same key is inserted again → 401 with `reason = "replay_detected"` and audit line `event = "dpop.replay_detected"`.
- **Test**: send the same proof twice → second call fails; send two proofs with same `jti` but different nonces → both accepted (they belong to different scopes).

## Gate 6b — HTTP-level DPoP crypto-handshake test binary

**Why not Hurl.** Hurl scripts are declarative — request template plus expected response. Every DPoP proof is unique per request (fresh `jti`, current `iat`, `htm`/`htu` matching the actual method+URL, ES256 signature from a persistent browser-held keypair, threaded nonce). Hurl has no scripting hook to compute a signed JWT per request. Same fundamental limitation that made us build the OPAQUE crypto-handshake test binary (task #19); same solution.

**Approach**. A new `src/bin/dpop-hurl-helper.rs` following the same pattern as `src/bin/opaque-hurl-helper.rs` (task #19). Same invocation shape — env-var driven from `tests/api/run.sh`, exit-code contract, no cleanup (server tears down DB between `run.sh` invocations), full crypto against a live server. The `-hurl-helper` naming is deliberate: this binary IS the DPoP counterpart of what Hurl covers for other protocols.

**Structure**:
- Generate a P-256 keypair once at binary start (persistent across the run — simulates one browser session).
- Compute JWK thumbprint (RFC 7638) for the public key.
- Reqwest-based HTTP client wrapped in a small helper that, for every request:
  - Builds a fresh DPoP proof with the correct `htm` (request method), `htu` (canonical URL), `iat` (unix seconds now), `jti` (random UUID), and current `nonce` if one is cached.
  - Signs with the persistent P-256 key.
  - Attaches `DPoP: <compact-JWT>` header.
- Auto-handle the `use_dpop_nonce` challenge: on 401 + `WWW-Authenticate: DPoP error="use_dpop_nonce"`, extract fresh nonce from the `DPoP-Nonce` response header, retry the ORIGINAL request once with the new nonce. Mirrors the SPA fetch interceptor from Gate 4.
- Reuse the OPAQUE handshake helper for login, so this binary exercises the OPAQUE+DPoP composed path end-to-end.

**Scenarios**:
1. **Happy path** — OPAQUE login includes `dpop_jkt`, first API request lacks nonce → challenge → retry with nonce → 200. Follow-up requests reuse the nonce until rotation.
2. **Fail-open** — login WITHOUT `dpop_jkt`, subsequent requests succeed with no `DPoP` header even in `required` mode (session exempt via `dpop_jkt IS NULL`).
3. **Bound session missing proof** — session created with `dpop_jkt`, request sent WITHOUT `DPoP` header. Expect 401 with `error_type: "DpopVerificationFailed"` in `required` mode; 200 in `opportunistic` mode with a `dpop.header_missing_but_session_bound` audit line.
4. **Wrong `htm`** — sign proof declaring `htm: "POST"` but send GET → 401 `reason = "wrong_htm"`.
5. **Wrong `htu`** — sign for `/api/files/list` but send to `/api/auth/me` → 401 `reason = "wrong_htu"`.
6. **`htu` canonicalisation behind proxy** — send `X-Forwarded-Proto`/`X-Forwarded-Host` headers matching the client-side `htu`; verifier must canonicalise identically. (Guards against Risk #1.)
7. **Stale `iat` on first request** (no-nonce path) — sign with `iat` far in the past → 401 in the no-nonce branch. Establishes the bootstrap-only clock check works.
8. **Nonce rotation** — issue a proof with nonce A, wait past the rotation window so server issues nonce B, present a fresh proof still bearing nonce A but within A's overlap window → still 200. Then wait past A's expiry → 401 challenge for B.
9. **Replay detection** — send the same signed proof twice → second call 401 `reason = "replay_detected"`. Confirms Gate 6.
10. **Thumbprint mismatch** — generate a SECOND keypair mid-run, sign with it → 401 `reason = "jkt_mismatch"`. Session's bound JKT is immutable.
11. **Refresh continuity** — DPoP-signed `POST /api/auth/refresh` succeeds, new session inherits the same `dpop_jkt`, subsequent requests continue to verify with the same keypair. Confirms Gate 7.
12. **Logout wipe** — `POST /api/auth/logout` succeeds; a fresh login on the same reqwest client (new keypair generated by the helper) gets a DIFFERENT `dpop_jkt` — no correlation across the logout boundary.
13. **Malformed proof** — send an unsigned JWT, wrong-alg (RS256), wrong-typ (`jwt` instead of `dpop+jwt`), missing `jwk` in header → 401 with the expected `reason` value in each case.
14. **Bind-time downgrade attempt** — attempt to POST login twice, once with `dpop_jkt` and once without, and confirm the resulting sessions honour their per-session bind status independently.

**Wiring**:
- Invocation mirrors `opaque-hurl-helper`: `tests/api/run.sh` sets `DPOP_HELPER_BASE_URL` / `DPOP_HELPER_USERNAME` / `DPOP_HELPER_PASSWORD` env vars, then runs `./target/debug/dpop-hurl-helper`. Exit 0 = all scenarios passed; exit 1 = diagnostic to stderr.
- Reuses the same running-server test target already spun up for the OPAQUE helper — no extra server process. Test run sets `OXICLOUD_DPOP_MODE=required` so scenarios exercise the strict path; the fail-open scenario logs in without the JKT to confirm the exemption still works.
- No `Cargo.toml` juggling — the binary is a `[[bin]]` entry alongside the other helpers; the workspace already builds all bins in `cargo build`.
- CI's `just api-test` continues to run everything (Hurl scenarios + `opaque-hurl-helper` + `dpop-hurl-helper`).

**Test-only deps**: reuse `p256` + `base64` + `serde_json` + `reqwest` already introduced in Gate 5. No extra crates just for tests.

**What this doesn't cover** — SPA-side journeys (fetch interceptor, IndexedDB persistence, multi-tab, cross-tab logout via `BroadcastChannel`). Those get **Playwright** coverage under Gate 8. The Rust binary owns the wire-protocol contract; Playwright owns the browser-side integration.

## Gate 7 — Refresh + logout flows

- **`POST /api/auth/refresh`**: currently unauthenticated (rate-limited public path minting new access tokens from a refresh token cookie). Two changes:
  - Client sends DPoP header on the refresh call. Verifier looks up the CURRENT session (pre-refresh) to fetch the `dpop_jkt` for comparison.
  - After minting the new session, copy `dpop_jkt` over. The same browser continues to sign with the same key.
- **`POST /api/auth/logout`**: server clears the session row (already does); client calls `clearKeypair()` to also wipe IndexedDB. Next login generates a fresh keypair (which is desirable — post-logout state is fully clean, no correlation between pre- and post-logout activity).
- **Session revocation from admin panel**: no client-side coordination possible. Server clears the row, next client request 401s at the auth layer (session gone), client redirects to login, generates fresh keypair. Same effect.
- **Test**: full login → several DPoP-signed requests → refresh (with DPoP) → several more requests → logout → new login uses different `dpop_jkt`.

## Gate 8 — Multi-tab handoff

- IndexedDB is per-origin, shared across tabs of the same profile → concurrent READ is fine.
- Concurrent WRITE (both tabs racing to generate the initial keypair): handled at Gate 2 via `navigator.locks.request("dpop-keypair", ...)`.
- Cross-tab logout: `BroadcastChannel("dpop-cleared").postMessage()` on logout so other open tabs invalidate their in-memory keypair reference and re-`ensureKeypair()` on next request.
- Nonce cache: `sessionStorage` per-tab is fine — each tab does its own initial challenge on cold start. No coordination needed.
- **Test**: Playwright scenario — open two tabs, log in on one, both make requests, both succeed with the same `dpop_jkt` on the server side.

## Gate 9 — Enforcement rollout

- Ship `OXICLOUD_DPOP_MODE=opportunistic` as default in the release that lands Gates 1-8.
- Operators monitor:
  - `dpop.header_missing_but_session_bound` count — should trend to zero as clients update.
  - `dpop.verify_failed{reason}` breakdown — spikes indicate client bugs, not attacks (attacks would be at trickle rate).
- After 2-4 weeks of clean opportunistic-mode telemetry, flip default to `required` in a later release. Pre-existing app-password / legacy sessions with `dpop_jkt IS NULL` still work; they're exempt at the middleware.
- **Documentation deliverable**: operator guide entry explaining the flag, the modes, the observability signals, the upgrade path.

## Gate C — Content-serve + streaming allowlist (browser-direct GETs)

Discovered during the `required`-mode rollout: some SPA endpoints are fetched by the **browser itself**, not by JS via `fetch()`. These paths have no JS in the loop to sign a DPoP proof:

- `<img src>` — thumbnails, photo previews.
- `<a href download>` — file downloads, folder ZIP downloads.
- `<a href>` — file inline previews.
- `EventSource` — streaming endpoints (RFC 9449 known gap: `EventSource` cannot set custom request headers, only cookies).

Without a carve-out, flipping to `DPOP=required` breaks all of these on bound sessions. Ships a middleware allowlist keyed on `(method, path)` that exempts these specific shapes from the missing-proof reject:

- `GET /api/files/{uuid}` — download / inline
- `GET /api/files/{uuid}/thumbnail/{size}` — thumbnails
- `GET /api/folders/{uuid}/download` — zip
- `GET /api/photos/{uuid}/preview` — preview (best-effort; adds no risk if the endpoint doesn't exist)
- `GET /api/admin/plugins/{id}/logs/stream` — plugin log tail SSE

The allowlist exempts ONLY the missing-proof reject. Proofs that ARE sent on these paths (e.g. an SPA image preloader that went through `apiFetch` and blob'd) still get fully verified.

**Security posture (accepted trade-off).** An attacker with a stolen cookie can GET one of these URLs *if and only if* they already know a specific 128-bit UUID. Every listing / discovery endpoint (`/api/folders/{id}/children`, `/api/photos`, `/api/files/by-hash`, `/search`, …) still requires a DPoP proof — those go through `apiFetch`. So a bare stolen cookie gives the attacker "download the exact IDs you already know" — effectively nothing without prior knowledge. Plugin log SSE additionally has admin-only AuthZ at the handler.

**Refactor pending (near-term).** The allowlist currently lives in the DPoP middleware as a slice-pattern matcher over path segments (`matches_content_path` in `src/interfaces/middleware/dpop.rs`). Cleaner: split the router — exempt routes on one sub-router without the `require_dpop_layer`, protected routes with it, merge. Declares exemption next to the route registration rather than centrally in the matcher; deletes the matcher and its 9 tests. Effort: 0.5 day. Behaviour-preserving.

**Long-term evolution (Option B).** See Deferred to Phase 2 — signed short-lived URL tokens replace the allowlist entirely.

## Gate 10 — Observability + admin UX

- **Audit events** (all with `target: "audit"`):
  - `dpop.bound_at_login` — info, records `session_id`, `dpop_jkt` prefix, auth method.
  - `dpop.header_missing_but_session_bound` — info, opportunistic-mode warning.
  - `dpop.verify_failed` with `reason ∈ {invalid_sig, wrong_htm, wrong_htu, expired_iat, jkt_mismatch, replay_detected, nonce_missing, nonce_stale, nonce_unknown, malformed_jws}`.
  - `dpop.nonce_challenge_issued` — debug-level, high volume; behind `target: "oxicloud::dpop"` not `audit`.
- **Admin session-list UI**: show a lock icon on sessions where `dpop_jkt IS NOT NULL`. Complements the auth-badges work (task #26).
- **Metrics**: Prometheus counters for `dpop_verify_failed_total{reason}`, `dpop_nonce_challenges_total`. Alerts on `verify_failed` spikes.

---

## Deferred to Phase 2

- **RFC 9449 full compliance** — resource-server mode, `ath` claim binding for OIDC access tokens.
- **OIDC-bearer DPoP** — configure the upstream IdP to mint DPoP-bound tokens; resource-side validation. Depends on IdP support (Keycloak ≥ 20, Auth0, Okta, Zitadel all have it).
- **Native Nextcloud client support** — no `SubtleCrypto`; would need embedded ECDSA + secure keystore (Android Keystore / iOS Keychain / OS keyring). Substantially larger project; the current Basic-Auth-over-app-password path stays unchanged.
- **Attested keys via WebAuthn** — bind to TPM / Secure Enclave. Blocks the login-time-compromised-browser attack. Major UX shift (per-request user gesture unless resident-key + silent-assertion flows mature).
- **Detect DPoP capability on upstream IdP** — parse `.well-known/openid-configuration`, warn at boot when `dpop_signing_alg_values_supported` is absent. Half-day of work, orthogonal to this plan, worth its own tiny PR.
- **Signed short-lived URL tokens for content-serve paths (Option B, replaces Gate C)**. The SPA (through `apiFetch`, so DPoP-verified) mints a per-user, per-URL token like `?dl_token=<sig>` for each browser-direct URL; the server accepts EITHER a valid DPoP proof OR a valid short-lived token (~5 min TTL, HMAC over `(user_id, path, expiry)`). Attacker with a stolen cookie loses the ability to GET any content path because tokens are user-scoped and expire fast — removes the "known UUID = downloadable" trade-off Gate C accepts today. Retires the Gate C allowlist entirely (plus its router-split follow-up). Effort: ~2-3 person-days (token mint endpoint + verifier middleware + SPA URL rewriter for `<img src>`/`<a href>`/EventSource URLs).

---

## Effort estimate

| Gate | Rough effort |
|---|---|
| 0 — Design record | 0.5 day |
| 1 — Schema | 0.5 day |
| 2 — Client keypair | 1 day |
| 3 — Bind on login | 1.5 days (touches 4 login paths, OIDC needs the bind endpoint) |
| 4 — Fetch interceptor (nonce-aware) | 1 day |
| 5 — Server verifier + middleware | 2 days |
| 5b — DPoP-Nonce service | 1 day |
| 6 — Replay cache | 0.5 day |
| 6b — DPoP hurl-helper binary | 1.5 days |
| 7 — Refresh + logout | 1 day |
| 8 — Multi-tab | 0.5 day |
| 9 — Rollout | 0 (calendar time, no engineering work) |
| C — Content-serve + streaming allowlist | 0.5 day (shipped alongside Gate 9) |
| 10 — Observability + admin UX | 1 day |
| **Total (Phase 1)** | **~12 person-days** |

## Risks worth naming

1. **`htu` claim + reverse proxies**. Client sees `https://oxicloud.example`; server behind nginx / Cloudflare sees `http://internal:8086`. Verifier MUST canonicalise both sides identically — `scheme://authority/path` with scheme and authority pulled from `X-Forwarded-Proto` / `X-Forwarded-Host` / `Forwarded`. Reuse the exact same helper the audit-log request span uses for `client_ip`; if it doesn't exist yet, extract one first. Getting this wrong = every request fails with `wrong_htu` in production but passes in dev.

2. **JWK thumbprint canonicalisation subtlety**. RFC 7638 requires a specific JSON member order (`{"crv":..., "kty":..., "x":..., "y":...}` alphabetical) and NO whitespace. Small deviations from library defaults produce different SHA-256s and thus mismatched thumbprints. Write a unit test with the RFC 7638 §3.1 example vector to lock the canonicalisation on both client and server before shipping.

3. **JWK header inflation on every request**. Each proof carries a ~300-byte JWT (mostly the JWK header). At high RPS this is measurable network overhead. Not a blocker at OxiCloud's expected load; note it for the perf budget review.

4. **Nonce cache DoS**. An attacker generating requests without valid sessions can force the server to issue nonces indefinitely, growing the moka cache. Mitigations: (a) cache is size-capped (LRU eviction), (b) rate-limit `/api/auth/dpop/bind` and other unauthenticated paths that could trigger issuance. Neither is DPoP-specific — existing rate limits apply.

5. **Chromium bug landscape for non-extractable IndexedDB CryptoKeys**. There have been historical issues where a browser update invalidates the stored CryptoKey structure (schema migration in the crypto subsystem). Mitigation: on `sign()` failure, drop the stored keypair, generate fresh, force re-bind on next login. This is a one-time inconvenience per browser upgrade, not a security issue.

6. **`jsonwebtoken` crate ES256 + embedded JWK support**. Confirm before Gate 5 that the crate handles JWS with a JWK in the header (not a `kid` reference). If not, hand-parse with `p256` + `base64` + `serde_json` — ~50 LoC, no dep surprise.

7. **Plan assumes cookies stay the primary session carrier**. If a future refactor moves to `Authorization: Bearer <token>` headers, the DPoP shape shifts slightly (the `ath` claim becomes relevant to bind the DPoP proof to the specific access token). Not a blocker for Phase 1 — cookies-only.

8. **Interaction with `force_password_change_at_next_login` gate**. Order of middleware layers matters: auth → DPoP verify → password-change gate. A user in reset-pending state must still pass DPoP verification (their session is bound); the reset-flow allowlist endpoints must also be DPoP-verified. Add explicit tests for this interaction.

---

## Success criteria (Phase 1 complete)

- All four login paths bind a keypair thumbprint to the new session.
- Every `/api/*` request from an SPA session carries a valid DPoP proof (verified in `required` mode).
- App-password sessions (`dpop_jkt IS NULL`) continue to work — no regression for Nextcloud sync clients.
- Copying the session cookie to `curl` on another machine reproducibly fails with 401.
- Audit stream contains actionable telemetry for verify failures, replay attempts, nonce challenges.
- Documentation in `docs/config/authentication.md` explains the modes, the flag, and the rollout guidance for operators.
