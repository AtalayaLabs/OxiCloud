// Fake OpenID Connect Identity Provider for the OxiCloud OIDC
// integration test (tests/oidc/oidc.hurl).
//
// Wraps panva/node-oidc-provider — a spec-compliant OP — with a
// minimal Node http front-end that auto-resolves every interaction
// (login and consent) for a hard-coded test user. We don't wrap with
// our own Koa instance because oidc-provider ships a bundled Koa that
// the response prototype-checks against; layering another Koa around
// it triggers `vary: res argument is required` on the first request.
//
// What we get from the library that we'd otherwise hand-roll:
//   * Discovery (.well-known/openid-configuration)
//   * JWKS endpoint + RS256-signed JWTs
//   * PKCE S256 verification
//   * Authorization code lifecycle
//   * Refresh token + id_token + access_token shapes
//
// What we get FOR FREE when we later add coverage for:
//   * Back-channel logout — flip features.backchannelLogout.enabled
//   * RP-initiated logout — flip features.rpInitiatedLogout.enabled
//   * Token revocation (RFC 7009) — flip features.revocation.enabled
//   * Token introspection (RFC 7662) — flip features.introspection.enabled
//
// Each future OIDC feature is a config flag in this file rather than
// new Rust protocol code to maintain.

import http from 'node:http';
import { URL } from 'node:url';
import { default as Provider } from 'oidc-provider';
// `jose` ships as a transitive dep of oidc-provider (it's what the
// library uses internally for JWTs). We reuse it to (a) generate the
// signing keypair at boot so oidc-provider signs id_tokens with keys
// we also own, and (b) mint spec-compliant logout_token JWTs in the
// /control/backchannel-logout endpoint below.
import { SignJWT, exportJWK, generateKeyPair } from 'jose';

// ── Configuration knobs ─────────────────────────────────────────────────
const ISSUER = process.env.FAKE_IDP_ISSUER || 'http://localhost:1080';
const PORT = parseInt(process.env.FAKE_IDP_PORT || '1080', 10);
const TEST_USER_SUB = 'oidc-test-user';
const TEST_USER_USERNAME = 'oidc_user';
const TEST_USER_EMAIL = 'oidc@example.com';
// Full claim set pinned to deterministic values so the Hurl test can
// assert that JIT provisioning (auth_application_service.rs:2257)
// stores each one verbatim. Keep the claim names matching the OIDC
// `IdTokenClaims` struct in src/infrastructure/services/oidc_service.rs.
const TEST_USER_NAME = 'OIDC Test User';
const TEST_USER_GIVEN_NAME = 'OIDC';
const TEST_USER_FAMILY_NAME = 'Test';
// `picture` is the OIDC claim; OxiCloud persists it as `User.image`
// (a URL or data URI). We use a stable HTTP URL so a simple equality
// check works in the Hurl assertion.
const TEST_USER_PICTURE = 'https://example.com/oidc-test-user.png';
// Group claim — paired with OXICLOUD_OIDC_ADMIN_GROUPS=admin-users in
// server-with-oidc.env. The JIT path intersects this list against the
// configured admin groups; a non-empty intersection escalates the new
// user's role from `user` to `admin`. This is the typical SSO pattern
// every Authentik/Keycloak/Entra deployment uses to map IdP groups to
// app roles.
const TEST_USER_GROUPS = ['admin-users'];

// OxiCloud's base URL — derived from the callback URI so run.sh only
// has one place (test.env) to change the port. Used by the BCL control
// endpoint to POST logout_tokens back to OxiCloud.
const OXICLOUD_BASE_URL =
  process.env.OXICLOUD_BASE_URL_FOR_BCL || 'http://localhost:8087';
const BCL_KID = 'fake-idp-key-1';
const BCL_EVENT = 'http://schemas.openid.net/event/backchannel-logout';

// ── Runtime-toggleable state for negative tests ────────────────────────
// `email_verified` is normally true; the test flips it to false via
// `POST /control/email-verified/false` to drive OxiCloud's anti-takeover
// rejection branch (auth_application_service.rs: only `email_verified`
// callers reach JIT-provisioning), then flips back. Module-level state
// because oidc-provider doesn't pass test-specific context into the
// claims() callback.
let emailVerifiedState = true;

// Runtime-swappable email — normally the pinned TEST_USER_EMAIL, but
// the OIDC-account-linking Hurl suite flips it via
// `POST /control/set-email` to test the auto-link + self-service-link
// safety checks: email mismatch refusal, +alias normalization
// equivalence, etc. Reset by `POST /control/reset-email` (or by
// setting to the pinned value explicitly).
let emailOverride = null;

// Runtime-swappable sub (subject / accountId) — normally TEST_USER_SUB,
// overridable via `POST /control/set-sub` to test the OIDC-linking
// scenarios that need a fresh, not-yet-known federated identity:
// self-service link happy path, +alias normalization link, auto-link
// happy path, auto-link refusal (verified=false). The set-sub endpoint
// ALSO clears the OP's session cookies in the response — otherwise
// the next authorize dance would reuse the previously-established
// session bound to the OLD sub and skip the login prompt (where the
// new sub gets bound). Reset by POSTing an empty/null body.
let subOverride = null;

// Pre-generate the signing keypair. oidc-provider v9 accepts private
// JWKs via configuration.jwks and exports the public halves at
// /jwks.json; keeping our own reference to the private key means we
// can also mint valid logout_token JWTs from the /control endpoint,
// so OxiCloud's back-channel-logout validator (which fetches the same
// JWKS) accepts them.
const { publicKey: bclPublicKey, privateKey: bclPrivateKey } =
  await generateKeyPair('RS256', { extractable: true });
const bclPrivateJwk = await exportJWK(bclPrivateKey);
bclPrivateJwk.use = 'sig';
bclPrivateJwk.alg = 'RS256';
bclPrivateJwk.kid = BCL_KID;
// eslint-disable-next-line no-unused-vars
const _bclPublicKeyRef = bclPublicKey; // kept for symmetry / debugging

const configuration = {
  clients: [
    {
      client_id: 'oxicloud-test',
      client_secret: 'test-client-secret-not-used-in-prod',
      // 8087: automated tests/oidc/oidc.hurl suite. 8090: human-run
      // tests/oidc/run-manual-sso-only.sh (SSO-only auto-redirect check).
      redirect_uris: [
        'http://localhost:8087/api/auth/oidc/callback',
        'http://localhost:8090/api/auth/oidc/callback',
      ],
      grant_types: ['authorization_code'],
      response_types: ['code'],
      token_endpoint_auth_method: 'client_secret_post',
      // Back-Channel Logout 1.0 wire-up. The URI is where OxiCloud's
      // handler lives (POST /api/auth/oidc/backchannel-logout). With
      // session_required = true, the OP MUST include `sid` in both the
      // id_token AND the logout_token — mirrors Keycloak's "Backchannel
      // Logout Session Required" client toggle. OxiCloud persists the
      // id_token sid on auth.sessions.oidc_sid so per-device revocation
      // works; without session_required we'd fall back to sub-based
      // (all-device) revocation.
      backchannel_logout_uri: `${OXICLOUD_BASE_URL}/api/auth/oidc/backchannel-logout`,
      backchannel_logout_session_required: true,
      // RP-initiated logout — required for tests/oidc/sso-only.hurl to
      // exercise the `post_logout_url` shape returned by OxiCloud's
      // /api/auth/logout when the session is OIDC-backed. The `/login`
      // URLs on both automated (8087) and manual (8090) ports are
      // registered so both runners can drive the flow.
      post_logout_redirect_uris: [
        'http://localhost:8087/login',
        'http://localhost:8090/login',
      ],
    },
  ],

  // Register the private JWK we generated above. The library uses it
  // to sign id_tokens; the public half is served at /jwks.json and is
  // what OxiCloud's OidcService caches for id_token AND logout_token
  // signature verification (they share the same JWKS per BCL 1.0).
  jwks: { keys: [bclPrivateJwk] },

  pkce: { required: () => true, methods: ['S256'] },

  claims: {
    openid: ['sub'],
    email: ['email', 'email_verified'],
    // `profile` is the standard scope OxiCloud requests
    // (OXICLOUD_OIDC_SCOPES in server-with-oidc.env). It covers every
    // claim the JIT-provisioning code in auth_application_service.rs
    // reads except email — name + given/family + picture +
    // preferred_username + groups all ride here.
    profile: [
      'name',
      'given_name',
      'family_name',
      'preferred_username',
      'picture',
      'groups',
    ],
  },

  async findAccount(_ctx, sub) {
    const currentSub = subOverride ?? TEST_USER_SUB;
    if (sub !== currentSub) return undefined;
    return {
      accountId: sub,
      // Return EVERY claim the OIDC client could ask for. The provider
      // filters by the consented scope before issuing — values not in
      // a granted scope are dropped from the ID token / userinfo.
      async claims() {
        return {
          sub: currentSub,
          email: emailOverride ?? TEST_USER_EMAIL,
          email_verified: emailVerifiedState,
          name: TEST_USER_NAME,
          given_name: TEST_USER_GIVEN_NAME,
          family_name: TEST_USER_FAMILY_NAME,
          preferred_username: TEST_USER_USERNAME,
          picture: TEST_USER_PICTURE,
          groups: TEST_USER_GROUPS,
        };
      },
    };
  },

  features: {
    // Turn off the dev login/consent UI; we own the interaction route.
    devInteractions: { enabled: false },
    // OIDC Back-Channel Logout 1.0. Turning it on makes the OP
    // advertise `backchannel_logout_supported` in discovery and
    // emit `sid` in id_tokens when the client has
    // `backchannel_logout_session_required: true`. We do NOT rely on
    // oidc-provider to send BCL notifications from its internal
    // session-destroy path (which would require driving OP session
    // lifecycle from the test); the /control/backchannel-logout
    // endpoint below mints a spec-compliant logout_token directly
    // and POSTs it to OxiCloud. That's the same wire shape a real
    // IdP produces, so OxiCloud's validator is exercised end-to-end.
    backchannelLogout: { enabled: true },
    // RP-Initiated Logout 1.0. Turning it on advertises
    // `end_session_endpoint` in discovery so OxiCloud's
    // `build_end_session_url` (invoked from POST /api/auth/logout)
    // returns a real URL instead of None. Without this the SSO-only
    // Hurl assertion `post_logout_url is present` fails silently.
    rpInitiatedLogout: { enabled: true },
  },

  // Put scope-implied claims (name, given_name, family_name,
  // preferred_username, picture, email, …) directly into the ID token
  // instead of keeping them at /userinfo only.
  //
  // OxiCloud's OIDC client (auth_application_service.rs:2085) only
  // calls /userinfo when the ID token lacks `email` — with the email
  // scope granted the ID token DOES carry email, so userinfo never
  // runs, and the default (conformIdTokenClaims: true) means `picture`
  // would silently vanish during JIT provisioning. Setting this to
  // `false` mirrors what most real-world IdPs (Authentik, Keycloak's
  // default profile) do for browser SSO clients.
  conformIdTokenClaims: false,

  // Point every interaction at our auto-resolver below.
  interactions: {
    url(_ctx, interaction) {
      return `/auto/${interaction.uid}`;
    },
  },

  cookies: {
    keys: ['fake-idp-cookie-key-not-a-real-secret'],
  },
};

const provider = new Provider(ISSUER, configuration);
provider.proxy = false;

// `provider.callback()` is an http-compatible request handler.
// We intercept /auto/<uid> ourselves and forward everything else.
const oidcHandler = provider.callback();

// `/control/*` paths are test-only hooks the Hurl suite uses to
// flip IdP-side state between flows (e.g. force email_verified=false
// to exercise OxiCloud's anti-takeover rejection branch). Kept on the
// SAME port as the OIDC endpoints so we don't have to thread two ports
// through every test config. Never used in production-shaped flows.
async function handleControl(req, res) {
  const url = new URL(req.url, ISSUER);
  if (req.method === 'POST' && url.pathname === '/control/email-verified/true') {
    emailVerifiedState = true;
    res.statusCode = 200;
    res.setHeader('content-type', 'application/json');
    return res.end(JSON.stringify({ email_verified: true }));
  }
  if (req.method === 'POST' && url.pathname === '/control/email-verified/false') {
    emailVerifiedState = false;
    res.statusCode = 200;
    res.setHeader('content-type', 'application/json');
    return res.end(JSON.stringify({ email_verified: false }));
  }
  // Swap the IdP-returned sub (accountId) to test the OIDC-linking
  // scenarios that need a fresh, not-yet-known identity: self-service
  // link happy path, +alias normalization link, auto-link happy path,
  // auto-link refusal (email_verified=false).
  //
  // Body: `{ sub: "sub-link-happy" }` — or `null`/`""` to reset to
  // the pinned TEST_USER_SUB.
  //
  // ALSO clears the OP's session cookies via Set-Cookie in the
  // response. Without this, the next authorize dance from the same
  // Hurl file (same cookie jar) would reuse the previously-established
  // session — bound to the OLD sub — and skip the login prompt where
  // the new sub gets bound. Panva/node-oidc-provider defaults for the
  // session/grant/interaction cookies are documented in
  // https://github.com/panva/node-oidc-provider/blob/main/docs/README.md#cookies
  // — we clear the ones the client-side cookie jar can hold.
  if (req.method === 'POST' && url.pathname === '/control/set-sub') {
    let body = '';
    for await (const chunk of req) body += chunk;
    let parsed = {};
    try {
      parsed = body ? JSON.parse(body) : {};
    } catch {
      res.statusCode = 400;
      res.setHeader('content-type', 'application/json');
      return res.end(JSON.stringify({ error: 'invalid_json' }));
    }
    subOverride =
      parsed.sub && typeof parsed.sub === 'string' && parsed.sub.length > 0
        ? parsed.sub
        : null;
    res.statusCode = 200;
    res.setHeader('content-type', 'application/json');
    res.setHeader('Set-Cookie', [
      '_session=; Path=/; Max-Age=0; HttpOnly',
      '_session.legacy=; Path=/; Max-Age=0; HttpOnly',
      '_grant=; Path=/; Max-Age=0; HttpOnly',
      '_interaction=; Path=/; Max-Age=0; HttpOnly',
    ]);
    return res.end(
      JSON.stringify({
        sub: subOverride ?? TEST_USER_SUB,
        overridden: subOverride !== null,
      }),
    );
  }
  // Swap the IdP-returned email to test the OIDC-account-linking
  // safety checks (email match, +alias normalization, mismatch refusal).
  // Body: `{ email: "alice@example.com" }` — or `null`/`""` to reset
  // to the pinned TEST_USER_EMAIL.
  if (req.method === 'POST' && url.pathname === '/control/set-email') {
    let body = '';
    for await (const chunk of req) body += chunk;
    let parsed = {};
    try {
      parsed = body ? JSON.parse(body) : {};
    } catch {
      res.statusCode = 400;
      res.setHeader('content-type', 'application/json');
      return res.end(JSON.stringify({ error: 'invalid_json' }));
    }
    emailOverride =
      parsed.email && typeof parsed.email === 'string' && parsed.email.length > 0
        ? parsed.email
        : null;
    res.statusCode = 200;
    res.setHeader('content-type', 'application/json');
    return res.end(
      JSON.stringify({
        email: emailOverride ?? TEST_USER_EMAIL,
        overridden: emailOverride !== null,
      }),
    );
  }
  if (req.method === 'POST' && url.pathname === '/control/backchannel-logout') {
    // Body shape: `{ sub?: string, sid?: string }`. Optional so the test
    // can exercise both revocation modes:
    //   * sub only → OxiCloud falls back to revoke-by-subject (kills all
    //     the user's sessions).
    //   * sid present → OxiCloud revokes just the session bound to that
    //     sid (per-device path — the "typical" mode when
    //     backchannel_logout_session_required is on).
    // Default to sub-only against the built-in test user when neither is
    // supplied — that keeps the simplest scenario a one-liner in Hurl.
    let body = '';
    for await (const chunk of req) body += chunk;
    let parsed = {};
    try {
      parsed = body ? JSON.parse(body) : {};
    } catch {
      res.statusCode = 400;
      res.setHeader('content-type', 'application/json');
      return res.end(JSON.stringify({ error: 'invalid_json' }));
    }
    const sub = parsed.sub ?? TEST_USER_SUB;
    const sid = parsed.sid; // may be undefined
    const now = Math.floor(Date.now() / 1000);

    // Mint the logout_token per BCL 1.0 §2.4:
    //   * `events` MUST contain the backchannel-logout URI as a key.
    //   * `sub` and/or `sid` MUST be present (we always include sub;
    //     sid conditional).
    //   * `nonce` MUST NOT be present (SignJWT does not add one by default).
    //   * `iat` present, `jti` present for replay-guard testing.
    const payload = { events: { [BCL_EVENT]: {} } };
    if (sub) payload.sub = sub;
    if (sid) payload.sid = sid;

    const jwt = await new SignJWT(payload)
      .setProtectedHeader({ alg: 'RS256', kid: BCL_KID, typ: 'JWT' })
      .setIssuer(ISSUER)
      .setAudience('oxicloud-test')
      .setIssuedAt(now)
      .setJti(`bcl-${now}-${Math.random().toString(36).slice(2, 10)}`)
      .sign(bclPrivateKey);

    // POST as application/x-www-form-urlencoded per BCL §2.5.
    const target = `${OXICLOUD_BASE_URL}/api/auth/oidc/backchannel-logout`;
    try {
      const resp = await fetch(target, {
        method: 'POST',
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        body: new URLSearchParams({ logout_token: jwt }).toString(),
      });
      const respBody = await resp.text();
      res.statusCode = 200;
      res.setHeader('content-type', 'application/json');
      return res.end(
        JSON.stringify({
          forwarded_to: target,
          oxicloud_status: resp.status,
          oxicloud_body: respBody,
        }),
      );
    } catch (e) {
      res.statusCode = 502;
      res.setHeader('content-type', 'application/json');
      return res.end(
        JSON.stringify({ error: 'forward_failed', detail: String(e) }),
      );
    }
  }
  res.statusCode = 404;
  res.setHeader('content-type', 'application/json');
  return res.end(JSON.stringify({ error: 'no such control endpoint' }));
}

// One-line per-request log — useful when a future test fails
// mysteriously ("did OxiCloud actually call /me?" / "is the
// /authorize redirect hitting the right URL?"). Kept because it's
// low-noise and makes the next debugging session 10x easier; the
// payload-dumping diagnostics that helped land the
// `image`-missing-from-INSERT fix (UserPgRepository::create_user)
// have been stripped.
const server = http.createServer(async (req, res) => {
  // eslint-disable-next-line no-console
  console.log(`[fake-idp] ${req.method} ${req.url}`);
  if (req.url.startsWith('/control/')) return await handleControl(req, res);

  try {
    const url = new URL(req.url, ISSUER);
    const autoMatch = url.pathname.match(/^\/auto\/[^/]+\/?$/);

    if (autoMatch) {
      return await handleAuto(req, res);
    }

    return oidcHandler(req, res);
  } catch (e) {
    // eslint-disable-next-line no-console
    console.error('[fake-idp] unhandled error:', e);
    if (!res.headersSent) {
      res.statusCode = 500;
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ error: 'internal', detail: String(e) }));
    }
  }
});

// ── Auto-approve handler ───────────────────────────────────────────────
// The library redirects /authorize to /auto/<uid>. We pull the
// interaction state, sign the test user in (prompt=login), then grant
// every requested claim+scope (prompt=consent). The provider issues
// the authorization code and 302s back to OxiCloud's callback.
async function handleAuto(req, res) {
  const details = await provider.interactionDetails(req, res);
  const {
    prompt: { name },
    params,
  } = details;

  if (name === 'login') {
    return provider.interactionFinished(
      req,
      res,
      { login: { accountId: subOverride ?? TEST_USER_SUB } },
      { mergeWithLastSubmission: false },
    );
  }

  if (name === 'consent') {
    const grant = new provider.Grant({
      accountId: subOverride ?? TEST_USER_SUB,
      clientId: params.client_id,
    });
    if (params.scope) grant.addOIDCScope(params.scope);
    // Explicitly grant every profile claim OxiCloud reads at JIT
    // provisioning (see src/application/services/auth_application_service.rs
    // around line 2257). `addOIDCClaims` is additive to whatever the
    // scope already implies, so listing them here is belt-and-braces
    // for keeping the claim set complete.
    grant.addOIDCClaims([
      'email',
      'email_verified',
      'name',
      'given_name',
      'family_name',
      'preferred_username',
      'picture',
    ]);
    const grantId = await grant.save();
    return provider.interactionFinished(
      req,
      res,
      { consent: { grantId } },
      { mergeWithLastSubmission: true },
    );
  }

  res.statusCode = 400;
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify({ error: 'unsupported_prompt', prompt: name }));
}

server.listen(PORT, () => {
  // eslint-disable-next-line no-console
  console.log(`[fake-idp] listening on ${ISSUER} (test user sub=${TEST_USER_SUB})`);
});
