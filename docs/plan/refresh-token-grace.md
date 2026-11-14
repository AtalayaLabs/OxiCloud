# Refresh-token grace — a lost response should not look like theft

**Status**: **DRAFT (2026-11-14) — Gate 0 open, awaiting a decision.** No code
yet. This document exists because a CI flake turned out to be a real property
of the refresh path, and the fix is a security-posture call, not a bug fix.

**Companion**: `docs/plan/dpop.md` (session binding; Gate 7 there is what makes
option 3 below cheap) and `docs/plan/auth-simplification.md`.

## The observation

`tests/e2e/spa/session-refresh.spec.ts::"reload with only oxicloud_access
removed stays logged in"` fails intermittently on CI with the app-shell button
missing. The Playwright trace says why — the refresh is **refused**, not slow:

```
GET  …/folders/<id>/resources   401        ← fires while the page is still live
POST /api/auth/refresh           -1        ← aborted by the reload, mid-flight
GET  /api/auth/me               401        ← expected: access cookie deleted
POST /api/auth/refresh          403        ← reuse detected, family revoked
GET  /api/auth/me               401
POST /api/auth/refresh          403
GET  /login?source=session_expired
```

The chain:

1. The test deletes `oxicloud_access` while the page is **live**. A background
   request 401s, and `transport.ts` does what it is designed to do: one
   `POST /api/auth/refresh`.
2. `page.reload()` aborts that request mid-flight. Server-side the rotation
   already happened — `rotate_session`
   (`session_pg_repository.rs:621`) revokes the parent and inserts the child in
   one transaction — so the old token is dead and the new cookie is sitting in a
   response the browser never applied.
3. After the reload the client presents the only refresh token it has: the
   revoked parent. `refresh_token()`
   (`auth_application_service.rs:2297`) sees `session.is_revoked()`, reads it as
   theft, calls `revoke_session_family()` and returns
   `ErrorKind::AccessDenied` → **403**.
4. Every later refresh is 403: the family is gone. The SPA gives up and
   hard-navigates to `/login?source=session_expired`.

Random because it needs that 401 to land in the gap between deleting the cookie
and the reload.

### This is not only a test

The ingredient is "a refresh whose RESPONSE never reaches the client", and the
real world produces it:

- **Navigation or tab close mid-refresh** — same abort as the test's reload.
- **Connectivity loss mid-refresh** — the rotation lands, the response does not.
- **Two tabs reloaded together.** `transport.ts` collapses concurrent 401s into
  one in-flight refresh *per document*, and `session.svelte.ts`'s `#inflight`
  guard is per store instance. Tabs share the cookie jar but not the guard, so
  both present the same parent token: one rotates, the other replays, and the
  family dies with both tabs in it.

**Blast radius**: `revoke_session_family` sets `revoked = true` on every
unrevoked row sharing the `family_id`. A family starts at login and is inherited
through rotations, so the radius is that login chain — one browser's cookie jar
and all its tabs. Other devices logged in separately have their own family and
survive.

**Frequency**: access tokens live 3600s and refresh tokens 604800s
(`config.rs:1810`, `:1811`), so an active session rotates hourly, every hour,
for a week. The window recurs.

**What the user sees**: no error and no explanation — a bounce to
`/login?source=session_expired` and a password prompt. The second-order cost is
worse than the logout: it teaches users that "session expired" is noise, when it
is exactly the message that should make them suspicious.

## Why the strict policy is there

Rotation plus replay detection is the OAuth BCP (RFC 9700) answer to a stolen
refresh token. Without it, a leaked token is a silent seven-day foothold. With
it, the thief and the legitimate client eventually present the same token, the
family dies, and the user notices because they were logged out. The current
behaviour is correct by the book.

Its one weakness is epistemic: **the server cannot distinguish "replayed by a
thief" from "my response never arrived"**, and it resolves the ambiguity
toward theft 100% of the time. The question this plan asks is whether a
narrower resolution exists that keeps the detection.

## Options

### 1. Status quo + harden the test only

Make the page quiet before deleting the cookie, so nothing is in flight to
abort:

```ts
await expect(page.getByTestId('appshell-user-menu-btn')).toBeVisible();
await page.waitForLoadState('networkidle');       // ← add
await context.clearCookies({ name: 'oxicloud_access' });
await page.reload();
```

Two lines, zero security change, CI goes green. It also leaves every real user
one dropped response away from a logout, and leaves the two-tab case live.

**This is the floor, not an option against the others — it should land either
way.**

### 2. Clock grace

Accept the parent token for N seconds after rotation. Simple, one config knob,
no new state beyond a timestamp.

Cost: a fixed window in which a stolen token replays **undetected**. The window
is the same for the attacker and for the lost response, and it has to be long
enough to cover a slow client (seconds), which is also long enough for an
attacker who is already watching.

### 3. Successor-bound grace — recommended

Accept the parent **until its successor is first used**, then strict. In
practice the window closes within milliseconds of the real client's next
request, and it closes *because* the real client made progress.

- Lost response: the client never used the child (it never got it), so the
  parent is still accepted → one more rotation → the user stays logged in.
- Theft: the real client's first request after rotation closes the window, and
  the thief's replay trips the existing family revoke. A thief who arrives
  *before* the real client gets a token — exactly as in option 2 — but only once
  per rotation, and the real client's next request then reveals the theft
  (two live children in one family).

Needs: a per-session marker for "successor has been used" and the branch in
`refresh_token()` ahead of the `is_revoked()` check. A graced replay should
rotate again normally — tokens are stored as plaintext in
`auth.sessions.refresh_token` (`20260307000000_initial_schema.sql:57`) but
re-issuing the same child would mean a second cookie-set for the same row, which
is worse than minting a fresh one.

### 4. Make the grace sender-constrained (stacks on 2 or 3)

`POST /api/auth/refresh` does **not** verify a DPoP proof today: the handler
(`auth_handler.rs:605`) takes no proof, the route is an auth primitive outside
`require_dpop_layer`, and the rotated session merely *inherits* `dpop_jkt`. The
client does send a proof (the trace shows `DPoP` on the request) — nothing reads
it.

Verifying it on refresh would mean a graced replay also has to come from the
same browser key, which removes most of the value of a stolen cookie in the
grace window. It is worth doing on its own merits regardless of which option
wins, and it may be what `docs/plan/dpop.md` Gate 7 intended.

## Recommendation

**Option 1 now, then 3 + 4.** 1 is a test fix and should not wait for the
decision. 3 is the smallest change that keeps replay detection intact while
removing the false positives; 4 narrows what the remaining window is worth.
Option 2 is the one I would not ship alone: it buys the same false-positive
relief as 3 while leaving a window that exists even when the legitimate client
is active.

## Gates

| Gate | Deliverable | Depends on |
|---|---|---|
| 0 | This document — decision on option 2 / 3 / 4 | — |
| 1 | Test hardening (option 1) **plus** a deterministic pin for the lost response: `page.route('**/api/auth/refresh', r => r.fetch().then(() => r.abort()))`, which rotates server-side and drops the response. Asserts the CHOSEN behaviour, so it is red until the gate that implements it | 0 |
| 2 | Successor-used state on `auth.sessions` + repository read/write | 0 |
| 3 | `refresh_token()` grace branch, with an `audit` line for the graced path (`event = "auth.refresh_graced"`, `reason = "successor_unused"`) — a graced replay stays visible to operators even though it is benign | 2 |
| 4 | DPoP proof verification on `/api/auth/refresh`, including the app-password / legacy `dpop_jkt IS NULL` exemption | — |
| 5 | Config knob + `docs/config/env.md`, and a note in `docs/plan/dpop.md` if Gate 4 lands | 3 |

## Open questions

- **Knob and default.** `OXICLOUD_REFRESH_GRACE` as a boolean, or a seconds
  value that also covers option 2? Default on or off? Per the fail-loud
  convention the safer default is the stricter one, which argues for off-by-
  default — at the cost of leaving the flake in place for anyone who does not
  read release notes.
- **Alarm threshold.** Should N graced replays in one family raise an operator
  alert? A legitimate client produces at most one per rotation; a pattern of
  them is either a broken client or an attacker probing.
- **Two-tab coordination.** A `BroadcastChannel` or `localStorage` lock would
  make tabs share one in-flight refresh, which removes that trigger entirely
  and is a frontend change independent of all of the above. Worth its own
  gate if option 3 is rejected.
