# Refresh-token grace — a lost response should not look like theft

**Status**: **SHIPPED (2026-11-14) — all gates closed.** This document exists
because a CI flake turned out to be a real property of the refresh path, and
the fix was a security-posture call, not a bug fix.

In one line: **an expired access token now survives a lost refresh response —
for 60 seconds, or until the newly-minted token is actually used, whichever
comes first.**

## Decisions (Ed, 2026-11-14)

| Question | Decision |
|---|---|
| What a graced replay returns | **Re-deliver the child**: mint a fresh access token for the child session and return the child's existing refresh token. Literally re-sending the lost response. No new row per lost response, and a thief who replays receives the same token the real client holds — so the next rotation by either still trips reuse detection. |
| How the window closes | **Successor use + clock cap.** Grace ends when the child is first observed serving a request, or after `OXICLOUD_REFRESH_TOKEN_GRACE_SECS`, whichever comes first. The cap bounds the client that receives a child and never uses it — otherwise a user who walks away leaves a replayable parent for the whole 7-day refresh TTL. |
| Knob and default | `OXICLOUD_REFRESH_TOKEN_GRACE_SECS`, **default 60**; `0` restores today's strict behaviour exactly. On by default because successor-binding keeps the detection, and off by default would ship a known user-facing logout that only operators who read release notes would fix. |
| DPoP proof on `/api/auth/refresh` (Gate 4) | **Same PR.** The graced window is then sender-constrained from the first commit: a stolen cookie replayed inside it also needs the browser's private key. |

### Two things that made this cheaper than the draft assumed

- **The access token already carries the session id.** `claims.sid` is what
  `middleware/auth.rs:413` stamps into `LastSeenTracker` (a DashMap upsert,
  flushed to `last_seen_at` by one batched UNNEST every 30s). "The real client
  actually used the child" is therefore observable with no new claim and no
  per-request write — the draft treated this as the expensive part of option 3.
- **The client can satisfy a DPoP nonce challenge on refresh.** `/api/auth/refresh`
  is in `AUTH_PRIMITIVES`, but that exemption only skips the refresh-and-retry
  dance; nonce attach and nonce retry live in the transport's `withProof`
  wrapper and apply there too (`endpoints/auth.ts::tryRefresh`). So Gate 4
  cannot brick refresh into a logout loop, which was the reason to defer it.

### What the implementation found

**`last_seen_at` was being written by the DATABASE, not the app.** The column
is `NOT NULL DEFAULT NOW()` and both session INSERTs omitted it while binding
`created_at` from `Utc::now()` — two different clocks, with PG's default
evaluated at transaction time and therefore always LATER. So every freshly
inserted session read as `last_seen_at > created_at`, the "has the child ever
served a request?" test answered yes for a child nobody had touched, and the
grace never fired once. `auth_refresh_grace.hurl` caught it on its first run
(step 3, expected 200, got 403).

Fixed at the cause: `create_session` and `rotate_session` now bind
`last_seen_at` from the entity, which sets it from the same `Utc::now()` as
`created_at`. That also makes the `Session` field's documented invariant
("set to `created_at` at construction") true of the row rather than only of
the struct — it was previously written and silently discarded. Same class as
`bug_job_run_timestamps_two_clocks`: never compare a column with a DB default
against one bound from the app clock.

Sessions created before the fix keep a DB-clock stamp and are simply never
graced — conservative, and self-correcting after one rotation.

### Accepted imprecision

`last_seen_at` lands in ≤30s batches, so a child used 10s ago can still read as
unused. In that window the grace behaves like a plain clock grace — which is
the risk the clock cap already accepts, so it is taken rather than plumbing a
port from the application layer down to the in-memory tracker. The check says so
at the callsite.

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

## Recommendation — ACCEPTED, with the clock cap from option 2

**1 + 3 + 4, and 3 takes a bounded window from 2.** Option 1 is a test fix and
did not wait for the decision. Option 3 keeps replay detection intact while
removing the false positives; 2's clock, used as a *cap* rather than as the
whole mechanism, closes the one hole in 3 (a child received and never used); 4
narrows what the remaining window is worth to an attacker. Option 2 alone was
the one to avoid: same false-positive relief, but its window stays open even
while the legitimate client is demonstrably active.

## Gates

All of 1–5 landed in ONE PR (Gate 4 included, per the decision above).

| Gate | Deliverable | Depends on |
|---|---|---|
| 0 | ✅ This document — decisions recorded above | — |
| 1 | ✅ `networkidle` before `clearCookies` in `session-refresh.spec.ts`, plus `"survives a refresh whose response never arrives"` — `route.fetch()` then `route.abort()`, so the server rotates and the page never sees the reply. Wire-level contract in `tests/api/auth_refresh_grace.hurl` | 0 |
| 2 | ✅ `find_family_successor(family_id, after)` — port, domain trait, PG impl, 3 integration tests. **No migration**: rotation is the only INSERT into an existing family, so "the next row" IS the child | 0 |
| 3 | ✅ `try_grace_replay()` ahead of the theft branch, re-delivering the child's token; `audit` line `event = "auth.refresh_graced"`, `reason = "successor_unused"` | 2 |
| 4 | ✅ `require_dpop_on_refresh_layer`, mounted inside the rate limiter. Shares `enforce_proof()` with `require_dpop_layer` rather than copying the required-vs-opportunistic policy; unbound sessions pass through | — |
| 5 | ✅ `OXICLOUD_REFRESH_TOKEN_GRACE_SECS` in `config.rs`, `config_file.rs`'s KNOWN table, `example.env`, `docs/config/env.md` | 3 |

## Still open — deliberately out of this PR

- **Alarm threshold.** Should N graced replays in one family raise an operator
  alert? A legitimate client produces at most one per rotation, so a pattern of
  them is either a broken client or an attacker probing. This PR emits the
  `audit` line that such an alert would key off
  (`event = "auth.refresh_graced"`); wiring it to the job-notification sinks
  (`docs/plan/job-registry.md`) is a follow-up.
- **Two-tab coordination.** A `BroadcastChannel` or `localStorage` lock would
  make tabs share one in-flight refresh and remove that trigger at the source.
  Independent of everything here, and no longer urgent now that the second tab's
  replay is graced instead of fatal — but it would still save a pointless
  round-trip per tab per rotation.
- **`last_seen_at` precision.** If the ≤30s flush window ever proves too coarse
  (an operator wanting a 5s cap, say), the fix is a read-through port to
  `LastSeenTracker`'s DashMap rather than a shorter flush interval — the batch
  exists to keep auth off the write path.
