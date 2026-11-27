-- Credential scopes move from the PROTOCOL vocabulary to the AREA one.
--
-- PR 2 step 5 of `docs/plan/scope-gates.md`. Both columns already exist —
-- `auth.app_passwords.scopes` and `auth.device_codes.scopes`, both
-- `VARCHAR(512) NOT NULL DEFAULT 'webdav,caldav,carddav'` since the
-- initial schema. The plan said app passwords had no such column; they do.
-- So nothing is ADDED here. What changes is the vocabulary stored in it.
--
-- `webdav,caldav,carddav` names PROTOCOLS. § Areas, not protocols rejects
-- that: every door maps to exactly one area, so a protocol vocabulary
-- names the same permission twice — and once `POST /api/calendars` exists,
-- a `caldav` scope guards one of two doors to the same data while
-- `calendar` guards both.
--
-- `oxicloud:` prefixed, because `POST /api/auth/device/authorize`
-- implements RFC 8628 and inherits RFC 6749's `scope` parameter: these
-- values live in the OAuth scope namespace, where `openid`, `profile`,
-- `email` and friends are reserved. An unprefixed `profile` would have
-- collided head-on with OIDC's, same word and opposite meaning.
--
-- A PURE TRANSLATION: no capability is added or removed. The first draft
-- appended `account:ro`, on the belief that avatars needed it — Ed asked
-- why, and the answer was that they do not. `/remote.php/dav/avatars/…`
-- is keyed by `{user}`, so it is not the caller`s own profile, and
-- `avatar_handler` falls through to SVG initials for ANY username, so it
-- enumerates nothing and confirms nothing. It is scopeless, declared in
-- `policy_declarations`, and no sync client needs an `account` grant.
--
-- `sharing` is deliberately absent too. It governs `/ocs/…/sharees`, the
-- share-recipient picker, where enumeration IS the risk: an
-- app-password client loses recipient autocomplete in the NC share
-- dialog and keeps the web UI, where sharing is done. The asymmetry
-- decides it — widening a ceiling later is safe, narrowing one breaks
-- clients already in the field.
--
-- Token-shaped translation rather than an equality match on the default:
-- the device flow accepts an arbitrary `scope` string from the client, so
-- a row may hold any subset. Each protocol token is replaced wherever it
-- appears, and rows already namespaced are skipped.
--
-- `sharing:ro` is APPENDED to every translated row, and that is a
-- RESTORATION, not a widening. Before this migration there was no scope
-- axis on the NextCloud surfaces at all, so an existing app password
-- could already read the roster — the OCS profile lookup and the share
-- dialog's recipient picker both worked. Translating to the three sync
-- areas alone would have TAKEN that away, which is how
-- `nc_admin_views_other_user.hurl` started failing with a 403.
--
-- So the rule the migration keeps is "no capability change relative to
-- what the credential could actually do", which is the promise that
-- matters to a user with a configured client. Read-only because every
-- OCS route that declares `sharing` is a GET.

-- ── app passwords ────────────────────────────────────────────────────────
UPDATE auth.app_passwords
   SET scopes =
       replace(
         replace(
           replace(scopes, 'webdav', 'oxicloud:files:rw'),
           'caldav', 'oxicloud:calendar:rw'),
         'carddav', 'oxicloud:contacts:rw')
       || ',oxicloud:sharing:ro'
 WHERE scopes NOT LIKE '%oxicloud:%';

ALTER TABLE auth.app_passwords
  ALTER COLUMN scopes
  SET DEFAULT 'oxicloud:files:rw,oxicloud:calendar:rw,oxicloud:contacts:rw,oxicloud:sharing:ro';

-- ── device codes ─────────────────────────────────────────────────────────
-- Same translation. The plan's § Defaults says the device-flow default
-- omits `account`; that is true of what a CLIENT may request by default,
-- but an existing row must not lose a capability it has today, so these
-- are widened exactly as the app passwords are.
UPDATE auth.device_codes
   SET scopes =
       replace(
         replace(
           replace(scopes, 'webdav', 'oxicloud:files:rw'),
           'caldav', 'oxicloud:calendar:rw'),
         'carddav', 'oxicloud:contacts:rw')
       || ',oxicloud:sharing:ro'
 WHERE scopes NOT LIKE '%oxicloud:%';

ALTER TABLE auth.device_codes
  ALTER COLUMN scopes
  SET DEFAULT 'oxicloud:files:rw,oxicloud:calendar:rw,oxicloud:contacts:rw,oxicloud:sharing:ro';

-- VARCHAR(512) still fits: the longest value this migration can produce is
-- the four-area default at 88 characters, and the widest legal set — every
-- area at `:rw` — is under 200.
