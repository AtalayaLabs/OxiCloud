-- Repair credential rows whose scopes were the bare token `all`.
--
-- `20261126000000_credential_scopes_to_areas.sql` translated the three
-- documented legacy values — `webdav`, `caldav`, `carddav` — and appended
-- `sharing:ro`. It was written against the DEFAULT the code shipped, not
-- against the values a real database holds, and a reference instance turned
-- out to carry `scopes = 'all'` on 15 of 18 app passwords. No current code
-- emits it, so it predates the documented default.
--
-- For those rows the translation found nothing to replace but still
-- appended, leaving `all,oxicloud:sharing:ro`. That does not parse: `all`
-- carries no `oxicloud:` prefix and is not one of the three aliases, so
-- `ScopeSet::from_credential` fails CLOSED and the credential reaches
-- NOTHING. A sync client would start answering 403 on every request with no
-- explanation beyond one `authz.scope_unparseable` line.
--
-- `all` meant unrestricted, so it translates to the app-password ceiling —
-- which is what an unrestricted app password can actually do now that the
-- ceiling is enforced. Capability is therefore preserved exactly: the row
-- states explicitly what enforcement would have allowed anyway.
--
-- Separate migration rather than a fix to the original, deliberately: the
-- original's checksum is already recorded wherever it has run, and editing
-- it would stop those instances booting.
--
-- Matched with a word-boundary regex, not `LIKE '%all%'`, so a token that
-- merely contains the letters is untouched. Idempotent: after this runs no
-- row matches, and it does not depend on the original's
-- `NOT LIKE '%oxicloud:%'` guard, which these rows now fail.

UPDATE auth.app_passwords
   SET scopes = 'oxicloud:files:rw,oxicloud:calendar:rw,oxicloud:contacts:rw,oxicloud:sharing:ro'
 WHERE scopes ~ '(^|,)\s*all\s*(,|$)';

UPDATE auth.device_codes
   SET scopes = 'oxicloud:files:rw,oxicloud:calendar:rw,oxicloud:contacts:rw,oxicloud:sharing:ro'
 WHERE scopes ~ '(^|,)\s*all\s*(,|$)';
