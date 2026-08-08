-- Bind a session cookie to a browser-held ECDSA keypair (DPoP, RFC 9449).
--
-- Each browser session that supports Web Crypto generates a P-256 keypair
-- with `extractable: false` and stores it in IndexedDB. The public-key JWK
-- thumbprint (RFC 7638, base64url-encoded SHA-256) is sent with the login
-- request and stored here. Middleware then requires every subsequent
-- request on the session to carry a valid DPoP proof signed by the paired
-- private key. Stealing the cookie alone gets an attacker nothing — the
-- private key never leaves the browser's crypto subsystem.
--
-- Nullable because:
--   * pre-DPoP sessions created before this feature landed;
--   * app-password / Nextcloud-client sessions (Basic Auth, no browser,
--     no Web Crypto) will always have NULL here and are exempted at the
--     middleware;
--   * browsers without SubtleCrypto (very rare in 2026) fail the client-
--     side keypair generation and log in unbound (fail-open per the
--     `docs/plan/dpop.md` threat model).
--
-- Immutable per-session: set at INSERT time, never updated. That's the
-- point — otherwise an attacker could downgrade a bound session by
-- clearing the column.
--
-- Length is 43 characters for a base64url-encoded SHA-256 (32 bytes ×
-- 4/3 = 43 chars, no padding). Cap at 64 to leave a little slack in
-- case we later support larger thumbprints (e.g. SHA-384 for P-384).
--
-- No index needed — the column is read alongside the session row by
-- primary key in the auth middleware, never queried in isolation.
ALTER TABLE auth.sessions
    ADD COLUMN IF NOT EXISTS dpop_jkt VARCHAR(64);

COMMENT ON COLUMN auth.sessions.dpop_jkt IS
    'DPoP JWK thumbprint (RFC 7638) binding this session to a browser-held keypair. NULL for app-password / legacy / unbound sessions.';
