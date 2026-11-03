# Normalize API error responses

Branch (current pass): `chore/wire-apierrorbody-responses` on top of
`feat/registration`.

## Goal

Advertise and enforce a uniform error-body shape across every API
surface:

```json
{
  "status":     "<numeric http status as string>",
  "error":      "<human message, duplicated into message>",
  "message":    "<human message>",
  "error_type": "<PascalCase machine discriminator>"
}
```

- `error` + `message` carry the SAME string. Legacy clients that keyed
  on `.error` keep working; FE / SDK clients that key on `.message`
  work too. Server-side the `AppError::IntoResponse` serializer
  already duplicates (`interfaces/errors.rs:239`).
- `error_type` is the SPA's switch key (PascalCase since
  `feat/registration`). FE never switches on `.message`.
- `status` is the numeric HTTP status rendered as a string, so a
  non-SPA client that only reads the body (no headers) can still
  distinguish.

## Status

### Done (this pass)

- `components.schemas.ApiErrorBody` published on `feat/registration`
  (`db8fa792`). Enabling step — purely additive, no path references.
- `body = crate::interfaces::errors::ApiErrorBody` added to 517
  single-line `(status = 4XX|5XX, description = …)` entries across
  28 handler files (this branch, uncommitted). Fully-qualified path
  so no module needs a new `use`.
- OpenAPI regenerated — `resources/gen/openapi.json` now carries the
  schema reference on every 4xx/5xx response.
- `cargo check --lib` green on the sweep.

### Open (this pass) — drift inventory

71 error-response sites across 15 handlers return ad-hoc JSON tuples
instead of going through `AppError`:

- **70 sites are losslessly normalizable** — shapes are either
  `{"error": "<msg>"}` or `{"message": "<msg>"}` or
  `{"error_type": "<snake>", "message": "<msg>"}`. All three can
  become `AppError::<kind>(message)` without dropping fields; the
  `AppError::IntoResponse` serializer duplicates `message` into
  `.error` for backward compat.

- **1 site has real data loss**: `share_handler.rs:307` returns
  `{"error": "Password required", "requiresPassword": true}` on
  401. The SPA share-landing page reads `.requiresPassword` to
  decide whether to prompt. Needs migration to
  `error_type: "SharePasswordRequired"` + SPA switch.

## Plan (this pass)

### Step 1 — Normalize the 70 lossless sites

Rewrite every drift site to use `AppError::<kind>(…)` or (for status
codes without a built-in constructor like 501 Not Implemented) an
inline `AppError::new(status, message, error_type)`.

Scope: 15 handler files, 70 sites total.

Risk: FE breakage from reading `.error` or `.message`. **Mitigated**
by the `AppError::IntoResponse` serializer which already duplicates
`message` into `.error`. FE code reading either field keeps working;
only `requiresPassword` is a real break (handled in Step 2).

### Step 2 — `SharePasswordRequired` migration

- Backend: `share_handler.rs` returns
  `AppError::new(StatusCode::UNAUTHORIZED, "Password required",
  "SharePasswordRequired")`.
- SPA: share-landing page branches on
  `err.errorType === 'SharePasswordRequired'` instead of
  `.requiresPassword`.
- Vitest pin: a regression test that asserts the SPA shows the
  password prompt when it sees that `error_type`.

### Step 3 — Regen + gates

- `cargo run --features dev_tools --bin generate-openapi`.
- `cargo test --lib`, `cargo clippy --all-features --all-targets -D
  warnings`, `cargo fmt --all --check`.
- `cd frontend && npm run check && npm run test:unit`.
- `just test-api` — Hurl pins may need updating if any asserted on
  the old `.error`-only shape; the migration is additive so pins
  that assert `.error == "msg"` keep working.

### Step 4 — Explicit FE audit

Grep the SPA for `.error`, `.message`, `.error_type`, and any other
field name under `response.data` from error branches. Confirm every
read is covered by the normalized shape. Known-good reads: all three
listed fields are preserved by `ApiErrorBody`. Known-bad reads: only
`.requiresPassword` (handled in Step 2).

### Step 5 — Playwright pass

Run the three UX surfaces most affected by the drift sites:
- Trash (trash_handler.rs — 1 site).
- Share password flow (share_handler.rs — the Step 2 migration).
- Photos (photos_handler.rs — 6 sites, previously had bespoke
  `error_type: "bad_request"` lowercase_snake that now harmonizes).

Expect green; a regression here is a design miss in Step 1/2.

## PR shape

One commit per logical unit:
1. `refactor: normalize handler error responses to AppError`
   (the 70 lossless sites)
2. `feat(share): migrate requiresPassword to error_type SharePasswordRequired`
   (Step 2 + SPA + Vitest pin)
3. `chore: regen openapi.json after error-shape normalization`

Alternatively, (1) and (3) collapse into one if the regen is tiny; (2)
stays separate because it carries SPA code.

---

## Deferred follow-ups (future passes, NOT this PR)

These are intentionally out of scope for the normalize-shapes pass.
They layer additive capabilities on top of the uniform
`ApiErrorBody`; starting them before the shape is uniform would mean
designing against a moving contract.

### Phase 2 — Translatable error messages with params

Current state after this pass: `message` is a server-rendered English
string. Users of parameterized errors see English only. Examples:
- "Character '/' is forbidden" — would benefit from `{char}`
  interpolation.
- "Quota: 95 GB / 100 GB" — would benefit from `{used}` / `{limit}`.
- "File name too long (max 255)" — would benefit from `{max}`.

**Design (Phase 2):**

1. Add `params: HashMap<String, String>` (optional, defaults to empty)
   to `ApiErrorBody`. Non-parameterized errors leave it empty.
2. Server emits:
   ```json
   {
     "status": "400",
     "error": "Character '/' is forbidden",
     "message": "Character '/' is forbidden",
     "error_type": "FilenameContainsForbiddenChar",
     "params": {"char": "/"}
   }
   ```
3. FE i18n catalog gets keys like
   `error.filename_contains_forbidden_char` with `{char}` placeholders.
   FE error-displaying components switch on `error_type` → look up the
   localized template → interpolate with `params`.
4. Fallback: if `error_type` is not in the FE catalog, show `message`
   verbatim (the server's English rendering) — same posture as today.

**Which errors deserve the param-i18n treatment** (initial list — expand
as sites surface):

- `InvalidInput` sub-types that currently return bespoke copy:
  forbidden-char, name-too-long, missing-field, bad-cursor.
- `QuotaExceeded` with `{used, limit}`.
- `AccountLocked` with `{retry_after_minutes}`.
- `AlreadyExists` with `{name}`.
- `SharePasswordRequired` (new in Step 2) — no params, just a key.

**Non-goals (Phase 2):**

- Not server-side i18n (would require rendering per request's
  `Accept-Language`; much bigger scope — see Phase 3 note below).
- Not adding i18n to every error. Phase 2 is about adding the carrier
  (`params`) + wiring the FE for the ~10 error_types that benefit
  most. Long-tail errors stay on the server's English `message`.

### Phase 2b — Batch per-item errors

Batch endpoints (`/api/batch/files/get`, `/api/batch/files/trash`, …)
return 200 OK with a `failed[]` array of per-item errors. Each entry
currently carries `{item_id, error_type, message}` — the same shape as
a flattened `ApiErrorBody` minus the HTTP envelope. The i18n treatment
is identical: add `params` to the per-item entry too so the SPA can
show "File 'photos.zip' — quota exceeded (95 GB / 100 GB)" instead of a
bare English string per failure.

Suggested schema for the batch case:

```json
{
  "succeeded": [...],
  "failed": [
    {
      "item_id": "abc-123",
      "error_type": "QuotaExceeded",
      "message": "Quota exceeded",
      "params": {"used": "95GB", "limit": "100GB"}
    }
  ]
}
```

SPA rendering can group related failures ("3 files failed:
QuotaExceeded on all") or display them per-item with localized copy.

### Phase 2c — Drop the `.error` / `.message` duplication

Today `ApiErrorBody` carries BOTH `.error` and `.message` with the SAME
string. Pure backward compat — pre-`AppError` handlers returned
`{"error": "..."}` and the modern `AppError::IntoResponse` canonical
shape is `{"message": "..."}`. The duplication was added so neither
kind of client (legacy readers of `.error`, modern readers of
`.message`) breaks when the two converged. See `interfaces/errors.rs:29-34`
for the historical note.

Kept as-is in the current pass — this PR normalizes drift to the
canonical duplicated shape rather than introducing a wire-format
break on top of a shape sweep. Dropping one side is a separate,
scoped change:

**Future pass:**

1. **Grep inventory.** Scan the SPA for every read of
   `error.error` / `.error` on an error payload and every read of
   `.message`. Hurl pins too (`tests/api/*.hurl` has
   `jsonpath "$.error"` + `jsonpath "$.message"` assertions). Catalogue
   which field each consumer uses.
2. **Pick a canonical** — recommend `.message` (matches HTTP problem+
   JSON convention, matches what every `AppError::<kind>()`
   constructor was written for, matches the SPA's dominant pattern).
3. **Deprecation window.** Keep both for one release with a boot
   warning / release-note line that `.error` is going away. Migrate
   every internal consumer (SPA + Hurl pins + any internal tooling)
   to read `.message` during that window.
4. **Removal.** In the subsequent release, drop `.error` from
   `ApiErrorBody` + the `ErrorResponse` struct in
   `interfaces/errors.rs`. OpenAPI diff flags it as a breaking schema
   change; `oasdiff` picks it up without manual intervention.

**Why defer:** zero functional benefit until the deprecation window
runs; dropping it in the same PR as the shape normalization doubles
the scope and the risk surface (a bug in the drop couples to a bug in
the normalize). Decoupling keeps each PR small enough to review
properly.

### Phase 3 — Server-side i18n (orthogonal track)

Server renders `message` per the caller's `Accept-Language` so that
non-SPA clients (DAV, CalDAV, mobile, curl) also see localized
errors. Requires a message-catalog loader on the server (partially
shipped — see memory `project_server_i18n` for the Locale newtype +
preferred_locale infrastructure). Compatible with Phase 2: a server
with Accept-Language rendering can still populate `params` for FE
callers who prefer client-side templating.

Not the next step. Phase 2 is sufficient for the common case (SPA
callers see localized errors; non-SPA callers see English). Phase 3
only matters when non-SPA callers become a UX priority.
