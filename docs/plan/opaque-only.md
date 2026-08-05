# Plan — Retire legacy password_hash, move to OPAQUE-only

## Context

Phases 0–6 of the OPAQUE work shipped a full RFC 9807 substrate alongside the
legacy Argon2id password path. The two live in parallel today: every user has
BOTH a `password_hash` column populated (used by `POST /api/auth/login`) and,
for anyone who's logged in at least once since the substrate went live, an
`opaque_envelope` column populated (silent-migrated by Phase 2 on the first
successful legacy login). Phase 4 makes legacy login refuse users whose
`opaque_migrated_at IS NOT NULL` — the visible cutover — but the underlying
Argon2 hash stays on the row for admin-reset compatibility and as a fallback
if OPAQUE ever needs to be disabled.

This plan describes the endgame: **remove `password_hash` from the DB
entirely**, run the deployment purely on OPAQUE envelopes, and redesign the
two flows that today assume the hash exists (`change_password`,
`admin_reset_password`). It's the "no plaintext password ever touches the
server, and no derivable-from-password material lives at rest either" state.
Scoped to internal password users; OIDC and magic-link paths are unaffected.

Design conversation captured 2026-08-05 with Ed. Design is locked; timing
and ordering are the load-bearing decisions here.

## What we gain

1. **Complete removal of the offline-brute-force target.** Today's
   `password_hash` is Argon2id at OWASP interactive params. If a DB dump
   leaks, that column is where an attacker starts guessing. OPAQUE envelopes
   are useless without both the passphrase AND the server's
   `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP` — a two-secret compromise is
   qualitatively harder than a one-secret one.

2. **`has_password` becomes obsolete.** All the DTO/UI gates that today branch
   on "does this user have a legacy password" collapse — the only per-user
   auth-capability signal that matters is "has OPAQUE envelope + which SSO"
   after the wipe.

3. **`admin_reset_password` gets cleaner.** Today it means "admin picks a
   temp password the user must change." Post-wipe there's no field for the
   admin to type into — the redesign centres on a recovery-magic-link, which
   is a nicer UX anyway (user picks their own password without an
   intermediate temp).

## What we lose

1. **Legacy-login fallback for OPAQUE-off deployments.** After the wipe, an
   operator flipping `OXICLOUD_AUTH_OPAQUE_MODE=off` locks EVERY user out
   (`/api/auth/login` verifies against NULL `password_hash` → refuses). The
   wipe is one-way for the deployment: once OPAQUE-only for a while, no
   going back short of admin-resetting every user manually.

2. **`admin_reset_password` today (write new hash, force change).** The
   fastest recovery flow — admin types a temp, hands it to the user, done —
   goes away. Recovery-magic-link is slower (email round-trip, user needs
   inbox access). Fine for the compromise-recovery case; slightly worse for
   the "user forgot password, needs immediate access" case.

3. **`change_password` today (verify current-plaintext via Argon2).** Loses
   its verification anchor. Redesign needed (see below).

## Preconditions before we start the wipe

Every one of these MUST hold. Adding a pre-flight check in
`oxicloud-cli opaque wipe-legacy` (proposed below) that refuses to run
otherwise.

1. **`OXICLOUD_AUTH_OPAQUE_MODE=opaque_only`** on the deployment for at
   least 90 days. Any shorter and we haven't given the fleet a chance to
   fully migrate — users who log in seldom (monthly, quarterly) need long
   enough to hit an OPAQUE login and stamp `opaque_migrated_at`.

2. **Every internal, non-OIDC, non-external user has BOTH `opaque_envelope
   IS NOT NULL` AND `opaque_migrated_at IS NOT NULL`.** Envelope alone is
   insufficient — silent-migrated envelopes exist but haven't proven the
   crypto pipeline works for that user until an actual OPAQUE login has
   completed. The admin badge from Phase 5 surfaces both signals; the
   dashboard should report N users still needing at least one OPAQUE login.

3. **Manual audit of the "envelope but never migrated" cohort.** Any user
   who has an envelope from silent-migration but never did an OPAQUE login
   is at risk. Options:
   - Prompt them to log in (email nudge)
   - Admin-reset them manually (they'll re-migrate on next login)
   - Exclude them from the wipe and manually delete their `password_hash`
     later once they've done at least one OPAQUE login

4. **Recovery-magic-link flow is live and tested.** Without it,
   `admin_reset_password` has no path forward and the operator can't help
   any user who ever locks themselves out. Non-negotiable prerequisite.

5. **Backups strategy documented.** The wipe SQL is one-way. Point-in-time
   restore of a pre-wipe backup would resurrect hashes on selected rows.
   Operator needs to know what the recovery path looks like AND what NOT
   to restore.

## The new endpoints

### `POST /api/auth/opaque/verify-current` — proof-of-current-password

Motivation: `change_password` needs to prove the user knows the current
password before accepting a new one. Today it uses Argon2 verify against
`password_hash`. Post-wipe there's no hash, so the proof has to run through
OPAQUE.

Shape: mirrors login KE1/KE3 exactly, but the KE3 handler does NOT mint a
session. Returns 204 on success, 401 with `error_type: "InvalidCredentials"`
on failure (same wire shape as `/login/ke3` — anti-enum). Requires an
authenticated session on the request (user is already logged in and just
proving they know their own current password).

Wire:

```
POST /api/auth/opaque/verify-current/ke1  { startLoginRequest }
  → { exchangeId, loginResponse }
POST /api/auth/opaque/verify-current/ke3  { exchangeId, finishLoginRequest }
  → 204 No Content   (or 401 InvalidCredentials)
```

The `userIdentifier` is implicit — it's the authenticated caller (from the
JWT / session cookie), not something the client re-declares. That closes off
"prove you know some OTHER user's password."

State machine: same `OpaqueLoginExchange` cache we use for login KE1/KE3.
Single-use, 60s TTL, anti-replay via `.take()` on KE3 arrival.

Rate limit: shared with the legacy login limiter, same reasoning as
`/login/lookup` — prevents this from becoming a cheaper offline password
oracle than the login endpoint.

### `POST /api/admin/users/{id}/reset-password-recovery` — replaces admin-picks-temp

Motivation: the current `PUT /api/admin/users/{id}/password` writes a new
Argon2 hash. Post-wipe there's no hash to write. Recovery-magic-link is the
substitute: admin triggers, server emails, user redeems, user picks their
own password.

Shape:

```
POST /api/admin/users/{id}/reset-password-recovery
  (admin bearer / cookie)
  → 204 No Content   (magic-link dispatched via SMTP)
```

Server-side flow:
1. Admin-gate check (existing middleware layer)
2. Look up target user; refuse if OIDC-linked (their IdP owns their credentials)
3. Clear OPAQUE envelope + set `force_password_change_at_next_login = TRUE`
   in one UPDATE (existing `clear_registration` — reuse as-is)
4. Revoke all sessions for the target
5. Mint a magic-link scoped to `resource_kind = 'password_reset'` (new
   resource kind) with a short TTL (say 1 hour); mail it to the target's
   registered email
6. Return 204 to the admin

Client-side (user side):
1. User clicks link in email
2. `GET /magic/v1/{token}` redeems, mints a session tied to
   `resource_kind = 'password_reset'`, marks the session as "elevated for
   password reset only" (session claim, checked by middleware)
3. SPA routes to a dedicated set-password page (NOT the usual profile page —
   the user has no other authenticated capability in this session)
4. User picks a new password → SPA does OPAQUE register (new envelope
   under new passphrase) — the session's password-reset scope allows the
   register endpoints
5. Server clears `force_password_change` on register success
6. SPA revokes the reset-scoped session, prompts a fresh login

Distinct-magic-link-resource-kind matters so a normal login-link token
can't be used to change password without proving current-password (which
it can't, since login-link users just clicked email — no proof-of-current).

### The wipe migration

Delivered as `oxicloud-cli opaque wipe-legacy` — a dedicated subcommand,
NOT a schema migration. Reasons:
- Idempotent (won't re-wipe already-nulled rows)
- Pre-flight refuses when preconditions aren't met (unlike a migration
  which runs unconditionally)
- Operator-driven, not deploy-triggered — a rolling deploy shouldn't
  suddenly wipe hashes because the release contained this feature

Pre-flight checks (all must pass, or the CLI refuses):
1. `OXICLOUD_AUTH_OPAQUE_MODE` is `opaque_only` in the running server's
   config (queried via `/api/auth/opaque/params` or a dedicated
   admin-only endpoint that reports config)
2. Fewer than N% of internal non-OIDC users lack an OPAQUE envelope AND
   `opaque_migrated_at`. N configurable via `--allow-unmigrated-percent`
   (default: 0 — strict; operator can raise if some users deliberately
   never log in and admin has accepted the resulting lockout)
3. Recovery-magic-link is available (SMTP wired + auth methods allow
   magic-link OR OIDC — the wipe requires a working recovery path)

Wipe SQL (inside the CLI, after pre-flight passes):

```sql
UPDATE auth.users
   SET password_hash = NULL
 WHERE oidc_subject IS NULL              -- not OIDC (they don't have a hash anyway)
   AND is_external = FALSE               -- not grant-only recipient
   AND opaque_envelope IS NOT NULL       -- has an envelope to log in with
   AND opaque_migrated_at IS NOT NULL    -- has proven OPAQUE works for them
   AND password_hash IS NOT NULL;        -- still has a hash to wipe (idempotent)
```

Output: `N password_hash columns nulled. M users still have password_hash
because they don't meet the OPAQUE-migrated preconditions — inspect via
`oxicloud-cli opaque wipe-legacy --dry-run` and address separately.`

The `WHERE` clause is intentionally strict: OIDC users, externals, and
under-migrated users are ALL left alone. The strict version is safer than
"wipe everyone" because it can't lock out a user we didn't expect to lock
out.

## Code cleanups after the wipe

Once every deployment has wiped and enough time has passed (say a year), we
can drop the legacy password code:

1. `password_hash` column: `ALTER TABLE auth.users DROP COLUMN password_hash;`
   (new migration)
2. `Argon2PasswordHasher` service: delete
3. `POST /api/auth/login` handler: delete (or reduce to a hardcoded 410 Gone
   with `error_type: "LegacyLoginRemoved"`)
4. `AuthApplicationService::login()`: delete
5. `is_oidc_user()` gate in `change_password` (blocked by task #31 fix
   discussion): now defended by "no password_hash to verify, must go through
   OPAQUE-verify-current" — becomes structurally impossible for OIDC-only
   users to hit change_password
6. `has_password` field on `UserDto` / `AdminUserSummaryDto`: delete (always
   false, meaningless signal)
7. `admin`-badge `password` chip: delete (same reason)
8. `oxicloud-cli opaque reset --user X` for legacy-recovery: still useful
   as an emergency lever (envelope somehow corrupted, need to force
   re-registration via recovery-magic-link), but its "silent-migration
   handles the recovery" semantics become "recovery-magic-link handles the
   recovery"
9. Silent-migration hook (Phase 2): delete — nothing to migrate FROM

Deprecation window before each of these: at least one major version. Users
who somehow still have `password_hash` set after the wipe need one more
opportunity to log in and complete migration before the column disappears.

## OIDC users through the transition

OIDC-linked users never had `password_hash` populated in the first place
(the OIDC-JIT path doesn't write one). They're unaffected by the wipe.
`change_password` refuses them today ("Password changes are not available
for SSO/OIDC accounts") — that check STAYS, because their credentials are
still IdP-managed and the OPAQUE verify-current handshake would fail
anyway (no envelope).

Hybrid users (SSO-linked + local password + OPAQUE envelope) — a real case
today, e.g. Ed's own admin account — flow through the wipe like any other
internal user: their `password_hash` gets nulled once they're OPAQUE-
migrated. They continue to have SSO available as an alternative login
path (independent of the wipe).

## Timeline / decision gates

Rough sequencing; each gate is "green when the previous one has been
running smoothly for the indicated period."

| Gate | Condition | Est. duration |
|---|---|---|
| G0 | Land per-envelope KSF (A+B+C) | ✅ shipped |
| G1 | Land task #31: change_password OPAQUE-lockout fix + hybrid-user password gate | Days |
| G2 | Land recovery-magic-link admin reset flow | Weeks |
| G3 | Land OPAQUE-verify-current + change_password redesign that COMPOSES the two (Argon2-verify AND OPAQUE-verify both work; use whichever the user has) | Weeks |
| G4 | Ship `oxicloud-cli opaque wipe-legacy` (dry-run only initially, no destructive flag) | Days |
| G5 | Add admin-dashboard metric: "N users still on legacy (`password_hash IS NOT NULL AND !opaque_migrated`)" | Days |
| G6 | Operator switches deployment to `opaque_only` mode | ✅ already possible |
| G7 | Wait 90+ days at `opaque_only`, watch the metric drop to 0 | Months |
| G8 | Enable destructive flag on wipe-legacy CLI; operator runs it | Minutes |
| G9 | One major version passes without regression | ~6 months |
| G10 | Delete legacy code paths (per checklist above) | Days |

G3 is the load-bearing one: it's the "change_password works for BOTH
legacy and OPAQUE-verified users simultaneously" bridge that lets the
fleet migrate at its own pace without a big-bang cutover. Without it,
we'd need to atomically switch every user's change-password flow at
once, which is hostile to gradual rollouts.

## Rollback plan

If we hit trouble AFTER the wipe:
1. Point-in-time DB restore to just before the wipe — resurrects
   `password_hash` for the affected rows. Cost: any user changes
   between the wipe and the restore are lost.
2. If restore is off the table: every affected user needs
   `admin_reset_password` (recovery-magic-link flow). Feasible for
   dozens; painful for thousands.
3. Preventive: keep the wipe's SQL output as an audit log — the CLI
   prints the count of affected rows; for a large deployment, capture
   the actual user_ids in a `wipe-report.jsonl` for the recovery path.

If we hit trouble BEFORE the wipe:
- Just don't run the CLI. `opaque_only` mode is reversible (flip back to
  `migrate`). Users can log in either way as long as `password_hash` is
  present.

The wipe itself is the one-way door; everything before it is reversible.

## Related work / dependencies

- `docs/plan/opaque.md` — the original multi-phase OPAQUE plan (Phases 0–6
  now shipped as of 2026-08-05)
- Task #31 — change_password OPAQUE-lockout fix (blocker for G1)
- Feedback memory `feedback_config_file_overrides_shell` — recovery-magic-link
  flow needs SMTP config; document the operator setup
- Existing `magic_link_repo` — recovery-magic-link reuses this infrastructure
  with a new `resource_kind` variant
