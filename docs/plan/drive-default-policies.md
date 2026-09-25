# Admin › Drive Policies — defaults, inheritance, and compliance reporting

Status: **approved, implementation in progress** on `feat/policies`.

## Context

Drive policies exist and are enforced (8 boolean knobs in a JSONB bag on
`storage.drives`, 14 enforcement sites, an admin-only PATCH, a shared
`PolicyList` renderer). **What does not exist is any notion of a default.**
The only thing resembling one is a hardcoded literal in two `INSERT`
statements — personal drives get
`{"include_in_photo_index": true, "include_in_music_index": true}`,
shared drives get `{}` (`drive_pg_repository.rs:300-307` and `:425-429`).

Three consequences the admin cannot currently address:

1. There is no way to say "every new shared drive should forbid public links".
2. Policy enforcement is **creation-time only**. Turning on
   `forbid_public_links` does nothing to links already minted — nothing
   retroactive exists anywhere (`docs/plan/drive.md:749-757`).
3. There is no way to see which drives are laxer than intended.

Goal: an admin section that owns per-kind defaults, makes existing drives
follow them, and reports the two gaps above without silently changing
anything.

## Settled decisions

- **Live inheritance.** A drive's bag stores only knobs an admin explicitly
  set; unset knobs resolve to the current default for the drive's kind.
  Tightening a default applies everywhere except where overridden.
- **Defaults live at `GET/PUT /api/admin/drive-policies/defaults/{kind}`**,
  inside the `/api/admin` nest — which already carries the `require_admin`
  router layer (`routes.rs:687-692`) and derives the `role:admin` OpenAPI
  scope from the path prefix (`api/mod.rs:614-645`).
- **`PATCH /api/drives/{id}/policies` stays exactly where it is**, with its
  hand-rolled handler-layer admin check — the documented deviation
  (`drive_management_service.rs:484-487`, memory
  `feedback_drive_policies_admin_at_handler`). `tests/api/drive_policies.hurl`
  keeps passing untouched.

## Design

### 1. Defaults table

New migration, one row per drive kind, seeded with **today's literals** so
upgrading changes no behaviour:

```sql
CREATE TABLE storage.drive_policy_defaults (
    kind        TEXT PRIMARY KEY REFERENCES ... CHECK (kind IN ('personal','shared')),
    policies    JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  UUID
);
```

**The two kinds get genuinely different seeds**, matching today's literals
exactly — this is not one default with a per-kind exception:

```sql
INSERT INTO storage.drive_policy_defaults (kind, policies) VALUES
  ('personal', '{"include_in_photo_index": true, "include_in_music_index": true}'),
  ('shared',   '{}');
```

A table rather than an `auth.admin_settings` KV row: it is per-kind, typed,
and extends to the reserved `Vault` kind (`AGENTS.md:67`) by inserting a row
rather than reshaping a JSON blob. The two kinds are edited independently and
never share a value — a personal default and a shared default for the same
knob are unrelated settings.

### 2. Overrides vs values — the one real semantic change

Today `DrivePolicies` (`src/domain/entities/drive.rs:157-224`) is 8 `bool`
with `#[serde(default)]`, so **absent means false**. Under inheritance
absent must mean *inherit*. So:

- Stored per-drive shape becomes `DrivePolicyOverrides` — 8 `Option<bool>`.
  This is the shape `UpdateDrivePoliciesDto`
  (`drive_handler.rs:392-410`) already has; promote it to the domain.
- `DrivePolicies::resolve(default, overrides) -> DrivePolicies` produces the
  effective value every enforcement site consumes. **Enforcement sites keep
  taking `DrivePolicies` and do not change.**

**Migration must prune, not just copy.** Existing bags were written by
partial-merge PATCHes, so most carry keys equal to the default. Left alone
they would all read as deliberate overrides and the first drift report would
be enormous. The migration deletes keys whose value equals the seeded default
for that kind — behaviour-identical (resolution yields the same answer), and
the initial drift report is empty, as it should be.

### 3. Resolution in SQL

Four enforcement sites read the bag in raw SQL and must see *effective*
values, not the override bag:

- `file_blob_read_repository.rs:571` and `:699` — `include_in_photo_index`
- `trash_db_repository.rs:282` and `:304` — `read_only`

Add a view and point those four at it:

```sql
CREATE VIEW storage.drives_effective AS
SELECT d.*, COALESCE(p.policies,'{}'::jsonb) || d.policies AS effective_policies
  FROM storage.drives d
  LEFT JOIN storage.drive_policy_defaults p ON p.kind = d.kind;
```

The join is dozens of rows against two — negligible even on the photos
timeline, which is the hottest of the four.

Rust-side readers (`get_policies_for_file`, `get_policies_for_folder`,
`get_drive_id_and_policies_for_*` in `drive_pg_repository.rs:729-800`, and
`drive_policies_cached` in `pg_acl_engine.rs:1329-1357`) select
`effective_policies` from the view instead of `policies`. **The 30 s policy
cache must also be invalidated when a default changes**, not only on per-drive
PATCH (`drive_management_service.rs:514-516`).

### 4. Personal drives carry invariants the policy bag does not express

Some personal-drive behaviour is hardcoded and fires **before** any policy is
consulted:

- `refuse_if_personal` (`drive_management_service.rs:694`, called from
  `set_member_role:211` and `remove_member:316`) — membership is immutable,
  single-user single-owner (memory `project_drive_personal_invariant`).
- Quota edits are refused (`:601`) — personal drives use the user envelope.
- `POST /api/drives` with `kind:"personal"` returns 501
  (`drive_handler.rs:174-184`); they are minted only by
  `PersonalDriveLifecycleHook`.

The consequence for this feature: **`forbid_owner_role_change` is moot on a
personal drive.** The roster cannot change whether the flag is on or off.
A drift report that flags a personal drive as "weaker" because that knob is
false would be reporting something no admin can act on and that carries no
risk — and a compliance list with false positives is one nobody reads.

So each knob needs an **applicability per kind**, and it must govern three
places consistently:

| | personal | shared |
|---|---|---|
| `forbid_owner_role_change` | **forced by code** — not configurable, never compared | configurable |
| `read_only` | **not defaultable** — per-drive only, see below | **not defaultable** |
| `include_in_photo_index`, `include_in_music_index` | configurable (default `true` today) | configurable (default `false`) |
| everything else | configurable | configurable |

1. **Editor** — rendered disabled on the Personal card with the reason
   ("membership is fixed on personal drives"), not silently hidden.
2. **Defaults API** — a `PUT` setting a non-applicable knob for a kind is
   rejected, rather than stored and ignored.
3. **Drift comparison** — non-applicable knobs are skipped entirely.

Add this as a field on the existing `PolicyDef`
(`frontend/src/lib/utils/drivePolicies.ts`) and its Rust counterpart, so
applicability lives beside the knob and a future knob declares it once.

### 5. Restrictiveness is a lattice, not a ladder

"Less restrictive than default" needs a per-knob direction, because the
direction is **not uniform**:

| knob | stricter value |
|---|---|
| `forbid_sharing`, `forbid_public_links`, `forbid_external_sharing`, `forbid_cross_drive_move`, `forbid_owner_role_change`, `read_only` | `true` |
| `include_in_photo_index`, `include_in_music_index` | **`false`** (opt-ins to the global index) |

A drive can be stricter on one knob and weaker on another, so there is no
single "compliant" verdict — the report is **per knob**:
`DrivePolicies::weaker_than(default) -> Vec<&'static str>`.

**Build the comparator per knob, not as a boolean XOR** — one function per
knob answering "is A at least as strict as B". This is not hypothetical
tidiness: `max_public_link_days` (below) is a scalar, so the comparison has
a genuine non-boolean arm from day one.

### 6. One job, two finding kinds

New `RecoverableJobHandler` named **`drive_policies_consistency`**,
`Mutates::Never`, registered on-demand beside the others in
`di.rs:1494-1660`. The `_consistency` suffix is load-bearing:
`consistency_batch` auto-discovers children by `ends_with("_consistency")`
and `AdminJobsPanel` sorts on it — the prefix is free, so a two-word subject
costs nothing. `drive_policies_` over a bare `policies_` because the repo
already has other things that could sensibly grow policies; the job name
should say whose.

Template to copy: `src/infrastructure/services/drives_consistency_service.rs`
(cursor over 16 raw UUID bytes, keyset paging, cancel poll →
`RunOutcome::Paused`, `record_or_log(...)`, the one-hour grace window at
`:184` that avoids flagging in-flight writes).

**Drift is NOT one of this job's findings.** It was, at first, and that was
the wrong home for it. "Which drives are laxer than their kind's default" is
`storage.drives` against a two-row defaults table — no joins, dozens of rows
— so `DrivePolicyDefaultsService::drift` computes it live on page load and
`GET /api/admin/drive-policies/drift` serves it.

Live is not merely cheaper, it is *correct*. A finding is a snapshot of a
completed run, so a drive whose override the admin had just fixed stayed on
the report until somebody re-ran the job — the report started lying at
exactly the moment it became useful. The live list empties as the overrides
are corrected.

It also does not belong to this family. A `_consistency` job reports on
**data**; drift is **configuration posture** — a decision to revisit, not an
integrity problem. What the job keeps is the half that genuinely needs it:
shares and grants, which are real joins over tables that grow, and whose
findings are worth keeping.

Findings, all `severity='anomaly'`, into the existing `jobs.run_findings`:

- `share_violates_drive_policy` — `detail: {drive_id, knob, token_name, item_type}`
- `share_outlives_policy_cap` — `detail: {drive_id, cap_days, expires_at,
  token_name}`; a null `expires_at` is reported as never-expiring, not skipped
- `share_missing_required_password` — `detail: {drive_id, token_name,
  item_type}`
- `grant_violates_drive_policy` — `detail: {drive_id, knob, subject_type,
  subject_id, username, is_external, resource_type, role}`. Kept distinct
  from the link finding because a public link and a colleague's access are
  different problems with different remedies; an admin reading one combined
  count would not know which they were looking at.

The scan makes **two passes per drive**, because "share" means two
different rows.

**Anonymous links** (`storage.shares`), constrained by three knobs:
`forbid_public_links` (a token grant exists at all), `max_public_link_days`
(the token grant's `expires_at` exceeds the cap — or is null, which is
never-expires and therefore the laxest value, not an exemption), and
`require_public_link_password` (`shares.password_hash IS NULL`).
`forbid_sharing` also lands here, since forbidding sharing outright covers
links; it is reported only when `forbid_public_links` is off, so one link
never raises two findings for the same problem.

**User and group grants** (`storage.role_grants`), constrained by
`forbid_sharing` and `forbid_external_sharing` (subject with
`auth.users.is_external`). This pass is not optional garnish:
`storage.shares` is exclusively the anonymous-link table, so a scan
reading it alone reports "clean" while every file shared with a colleague
contravenes a freshly-enabled `forbid_sharing` — silent about the most
common kind of share there is.

Drive-level grants (`resource_type = 'drive'`) are excluded: that is
MEMBERSHIP, which `forbid_sharing` does not govern — the grant handler
skips Drive resources for the same reason. Including them would flag every
member of every drive.

Both queries are single statements over small tables — the job exists for
**surfacing**, not performance: `jobs.run_findings` + the findings drawer is
the only durable, drillable per-resource list an admin has.

The join, for reference — `files` and `folders` both carry `drive_id`
NOT NULL, so there is no walk through the folder tree, and `shares.item_id`
is **TEXT** so the cast is mandatory:

```sql
FROM storage.shares s
LEFT JOIN storage.files   f  ON s.item_type='file'   AND f.id  = s.item_id::uuid
LEFT JOIN storage.folders fo ON s.item_type='folder' AND fo.id = s.item_id::uuid
JOIN storage.drives_effective d ON d.id = COALESCE(f.drive_id, fo.drive_id)
LEFT JOIN storage.role_grants g ON g.subject_type='token' AND g.subject_id = s.id
```

There is no FK from shares to files/folders, so `d.id` can come back NULL for
a dangling share — itself worth a finding rather than a skipped row.

### 7. Discovery only

No automatic remediation. This follows the existing consistency rule
(memory `feedback_no_silent_auto_repair`): scans report, repair is a separate
explicit opt-in. Retroactively revoking links people are using is not
something a policy save should do silently.

### 8–9. Two further in-scope decisions

Written up after Verification because they are knob-level rather than
structural, but both ship with this work:

- **Two new knobs on the public-link path** — `require_public_link_password`
  and `max_public_link_days`.
- **`read_only` is deliberately NOT defaultable** — it stays per-drive.

## UX

New tab `policies` in Admin. Adding a tab is mechanical: `Tab` union,
`VALID_TABS`, a `tabLabel` case, a `loaded[tab]` dispatch
(`admin/[[tab]]/+page.svelte:195-229`, `:276-300`, `:1964-1992`), one
`ADMIN_LINKS` push in `AppShell.svelte:92-158`, one `{:else if}` block.

**Two default cards, side by side — Personal and Shared**, each embedding the
existing `PolicyList.svelte` in editable mode. Driving both off the same
`policyDefs` array (`frontend/src/lib/utils/drivePolicies.ts`) is what
guarantees the defaults editor covers *every* knob, including ones added
later — one push to that array and the knob appears in the admin defaults,
the per-drive modal, and the read-only drive view together.

A per-knob **"N drives override this"** count beside each toggle was in the
original design. Dropped: the live drift list below already names those
drives, per knob, and an aggregate count sitting above a list that spells
out the same thing is one more place to keep in sync for no extra answer.

**Save on change, then show what happened.** There is no
review-then-confirm step. It existed to forecast the blast radius before
committing, and once drift is computed live that forecast is redundant: the
admin toggles, it saves, and the list below shows which drives did not
follow — the actual consequence, a moment later, on the same screen. Showing
what happened beats predicting what would, and there is no modal in the way.

This is safe because a default is cheap to reverse — toggling back restores
it — and because enforcement is creation-time, so the write itself revokes
nothing that already exists. Writes are debounced (~600 ms): `PUT` replaces
the whole bag, so two quick clicks would otherwise race and the loser would
silently undo the winner. Coalescing to one write of the current state makes
the order irrelevant. The toggles are deliberately NOT disabled while a save
is in flight, or the list would grey on every click.

`?dry_run=true` stays on the endpoint. Nothing in the UI calls it now, but
it is a documented, tested capability for scripted callers, and it shares
`weaker_against` with the live drift view — so the two can never disagree
about what "weaker" means.

Below the cards, two lists that are deliberately different in kind:

- **Drives less restrictive than their default (N)** — **live**, loaded on
  arrival and re-read after every save, from
  `GET /api/admin/drive-policies/drift`. No scan, no button. Each row names
  the knobs and opens the per-drive editor **in place** via the shared
  `DrivePoliciesModal` — not `/config/drive/{uuid}` (read-only there) and
  not the Drives tab: the reason to be reading this list is to decide
  whether to correct the override, and navigating away loses the list that
  prompted it. The row disappears as soon as the override is fixed, because
  the list is recomputed rather than replayed.
- **Public links and grants that violate their drive's policy (M)** — from
  the latest `drive_policies_consistency` run via `listFindings`, reusing
  the generic `Finding` type, with an explicit "Run the scan" button. These
  need the joins and are worth keeping as findings.

**The two cards are independent settings, not a shared value with an
exception** — editing the Personal default never touches Shared. Knobs that
are forced by code on a kind (§4) render disabled with the reason on that
card rather than being silently absent, so an admin can see *why* a control
is unavailable instead of wondering whether it is missing.

i18n goes under a new nested `admin.drive_policies` block in
`frontend/static/locales/en.json`, matching `admin.jobs` / `admin.sessions`
convention (`tab`, `title`, `hint`, `col_*`, empty states). Knob labels are
**reused** from the existing `admin.drive_policy` block — not duplicated.

## Files

**Docs (step 0)**
- `docs/plan/drive-default-policies.md` — this plan, committed to the repo
  beside `drive.md` and the other plan docs. Written first so the design is
  reviewable in-tree before any code lands.

**Backend**
- `migrations/<ts>_drive_policy_defaults.sql` — table, seed, prune, view
- `src/domain/entities/drive.rs` — `DrivePolicyOverrides`, `resolve`,
  `weaker_than`, plus two new knobs: `max_public_link_days: Option<u32>` and
  `require_public_link_password: bool`
- `src/application/services/share_service.rs` — enforce both at
  `create_shared_link` (`:310-336`), beside the existing
  `refuse_public_links` gate
- `frontend/src/lib/components/ShareDialog.svelte` — make the password field
  required and cap the expiry picker from the drive's effective policies, so
  the limits show before submit rather than as a refusal
- `src/application/services/drive_policy_defaults_service.rs` — new; AuthZ in the service per `AGENTS.md:209-213`
- `src/interfaces/api/handlers/admin_handler.rs` — `GET/PUT defaults/{kind}` + dry-run
- `src/infrastructure/services/drive_policies_consistency_service.rs` — new job
- `src/common/di.rs` — register job + service
- Readers switching to `effective_policies`: `drive_pg_repository.rs`,
  `pg_acl_engine.rs`, `file_blob_read_repository.rs`, `trash_db_repository.rs`

**Frontend**
- `src/routes/admin/[[tab]]/+page.svelte` — tab plumbing + section
- `src/lib/components/AdminDrivePoliciesPanel.svelte` — new
- `src/lib/api/endpoints/admin.ts` — defaults get/put/preview
- `src/lib/components/PolicyList.svelte` — gains a second control type for
  the day-cap; every other knob stays a checkbox
- `src/lib/utils/drivePolicies.ts` — `max_public_link_days` def, control kind,
  per-kind applicability, `defaultable`. **The single place a knob is
  declared** — one push here surfaces it in the admin defaults, the per-drive
  modal and the read-only drive view together
- `frontend/static/locales/en.json` — `admin.drive_policies`

## Verification

1. `cargo test --workspace`; new unit tests for `resolve` and `weaker_than`
   covering: the inverted `include_in_*` direction; a drive stricter on one
   knob and weaker on another; a personal drive with
   `forbid_owner_role_change = false` producing **no** finding, because the
   knob is not applicable to that kind (§4); and the scalar arm —
   `None` (no cap) must compare as laxer than any `Some(n)`, which is the
   comparison most likely to be written backwards.
2. **Upgrade is a no-op**: on a DB snapshot taken before the migration, assert
   every drive's *effective* policies are byte-identical after it. This is the
   test that matters — the prune step must not change one drive's behaviour.
3. `tests/api/drive_policies.hurl` unchanged and green — proves the per-drive
   endpoint was not disturbed.
4. New `tests/api/drive_policy_defaults.hurl`: non-admin gets **403** — not
   the anti-enum 404 the per-drive endpoint uses, because that one names a
   specific drive whose existence must not leak while this one names no
   resource, and the whole `/api/admin` nest answers 403 via `require_admin`;
   set a default; a newly created drive inherits it; override one knob;
   flip the default and confirm the override holds while the rest follow.
   Also: setting the personal and shared defaults for the same knob to
   different values, and asserting each kind resolves to its own — the two
   must never bleed into one another.
5. Job: seed a drive with a public link, flip `forbid_public_links`, trigger
   `drive_policies_consistency`, assert one `share_violates_drive_policy`
   finding — and that the link **still works** (discovery only).
6. Cap and password: with both unset, create a link that never expires and
   has no password; set `max_public_link_days` and
   `require_public_link_password`; re-scan and assert one
   `share_outlives_policy_cap` (naming the null expiry) and one
   `share_missing_required_password`. Then assert a *new* link beyond the cap
   or without a password is **refused** at creation with an audit line, and
   one satisfying both succeeds — and that the pre-existing link still works.
7. Freeze stays per-drive: `PUT` a default containing `read_only` is
   **rejected** (not stored and ignored); the per-drive modal still sets it
   and still freezes that one drive; `read_only` never appears in the drift
   list, because a non-defaultable knob has no default to be laxer than.
8. Drift is live: override a knob so the drive is laxer, read
   `GET /api/admin/drive-policies/drift` and assert the drive appears with
   that knob; correct the override; read again and assert it is **gone
   without any scan having run**. That last assertion is the whole reason
   drift left the job — a finding could not pass it.
9. `just check`, `just test`, `just test-integration`, `just api-test` in that
   order (`AGENTS.md:119-131`). Regenerate `resources/gen/openapi.json`
   (`cargo run --bin generate-openapi`) and confirm a non-zero diff; add the
   new handlers to `paths(...)` and DTOs to `components(schemas(...))`.
10. Frontend `npm run check` + `npm run test:unit`.

## Two new knobs, both on the public-link path

`max_public_link_days` (`Option<u32>`, `None` = no cap) and
`require_public_link_password` (bool). They share one enforcement site —
`share_service::create_shared_link` (`share_service.rs:310-336`), beside the
existing `refuse_public_links` gate — so they cost one code path between
them, not two.

Together with `forbid_public_links` they give an admin the three decisions
that actually matter about an anonymous link: whether it may exist, how long
it may live, and whether it may be handed out without a secret.

### `require_public_link_password`

The cheapest knob available. `storage.shares.password_hash` already exists
and is already `NULL` for password-less links, so the compliance query is a
null check and every existing password-less link becomes a finding with no
new storage at all.

- Enforcement: `create_shared_link` refuses when the policy is on and no
  password was supplied, with the standard audit line.
- Finding: `share_missing_required_password`, `detail: {drive_id,
  token_name, item_type}`.
- Note the share dialog must ask for the password **before** submitting
  rather than surfacing a refusal — the FE reads the drive's effective
  policies already, so the field becomes required rather than optional.

### `max_public_link_days`

The **first non-boolean knob**. A public link on a drive under the cap may
not outlive it.

It earns its place because it is the only knob that makes the compliance
report say something *gradual* about links that already exist. The other
three share-relevant knobs are binary — a link is permitted or it is not. A
cap turns "this link outlives what the policy now allows" into a finding,
which is precisely the policy-got-stricter case this work is for.

The data is already in the scan's join: expiry lives on
`storage.role_grants.expires_at` for the token grant, **not** on
`storage.shares` — that column was dropped in
`20260601000000_rebac_expiry_and_perms_cleanup.sql:61-68`. `expires_at IS
NULL` means never expires, the laxest possible value and the obvious first
finding.

**Refuse, don't clamp.** The UI caps its own expiry picker so the cap is
visible before the user commits, and `share_service::create_shared_link`
(`share_service.rs:310-336`) refuses an out-of-range request with the
standard audit line. Clamping would silently hand back a link that expires
sooner than the user asked for, and a share dialog that quietly disagrees
with the person using it is worse than one that explains the limit.

Consequences to carry through the rest of the design:

- **Comparator** gains a real scalar arm: smaller is stricter, `None` is the
  laxest value of all. `None` vs `Some(n)` is the comparison most likely to
  be written backwards — it gets its own unit test.
- **`PolicyList.svelte`** grows a second control type. Today every row is a
  checkbox; the knob definition in `drivePolicies.ts` gains a control kind so
  the renderer switches on it rather than every caller special-casing.
- **Finding**: `share_outlives_policy_cap`, `detail: {cap_days, expires_at,
  token_name}` — with a null `expires_at` reported as never-expiring rather
  than skipped.
- **Enforcement is still creation-time.** Existing links keep working; the
  scan reports them. Nothing here retroactively expires anything.

## `read_only` is deliberately NOT defaultable

The freeze knob stays per-drive, in the admin modal where it already lives.
It is the one knob excluded from the defaults cards.

It is not a policy in the sense the other eight are. They express a standing
rule about what users may do with a drive's content — a posture. `read_only`
is an **operational state**: one drive, one reason, usually one duration
(a migration, a decommission, a legal hold, an incident).

Three consequences follow:

- **"New drives start frozen" is near-nonsensical.** A default answers how a
  drive of this kind should begin life, and a drive nobody can write to is
  not a configuration anyone wants as the standing rule.
- **Its drift finding would be noise.** "This drive is writable but the
  default says frozen" is not a compliance problem, it is normal operation —
  and noise is what makes an admin stop reading the report.
- **The blast radius has no upside to offset it.** Under live inheritance one
  toggle would freeze every non-overriding drive. The single legitimate
  version of that — freeze everything during an incident — wants to be an
  explicit reversible action, not a settings default that also silently
  applies to every drive created next month.

The engine agrees: `read_only_gate_applies` (`pg_acl_engine.rs:1379`) blocks
every permission except `Read` but keeps `Manage` on Drive permitted **so an
admin can unfreeze**. That escape hatch is the signature of an operational
toggle, not a policy posture.

So knob declarations carry a third property beside per-kind applicability:
**defaultable**. `read_only` is the only knob with `defaultable: false`
today. The defaults card renders it as an explicit one-line exclusion —
"set per drive; not a default" — rather than omitting it silently, so nobody
wonders whether it was forgotten.

If "freeze every drive at once" turns out to be a real operational need, it
belongs as its own admin action with its own confirmation, not as a side
effect of editing a default.

## `include_in_music_index` has no enforcement site — and that is correct

Worth writing down, because it looks like a bug on inspection and is not.

The knob is seeded, editable, audited and stored, yet no query reads it —
while its photo twin has two enforcement sites
(`file_blob_read_repository.rs:571` and `:699`). That asymmetry is
deliberate and documented at `docs/plan/drive.md:1430`: the Music section
today is **only playlists**, and a `/api/music/tracks` library view "added
later inherits this scope".

The code bears that out. Every endpoint in `music_handler.rs` is
playlist-scoped — `create_playlist`, `add_tracks`, `list_playlist_tracks`,
`get_audio_metadata` — and none of them enumerates audio across drives. A
playlist is explicit curation: the user put those tracks there, so there is
no drive-level index for the knob to filter. Photos differs precisely
because `/photos` and Places *do* enumerate across drives.

So the knob is a forward declaration waiting for its feature, not a
regression. Nothing to fix here. It still participates in defaults and drift
like any other knob — an admin setting it is expressing intent that the
library view will honour when it lands.

## Not in scope

**No retention policy.** Not built, not stubbed, not designed for. There is
no demonstrated need and it would be a knob invented to fill out a table.

## Future: candidates, and one structural idea

Recorded so the design does not foreclose them. None are part of this work.

**The structural one — ceiling vs default.** SharePoint/M365 treats the
tenant setting as a *ceiling*: a site may be at most as permissive as the
tenant, never less. Google Workspace is closer to what is planned here — a
default plus visibility. A ceiling turns a weaker override from *reported*
into *impossible*.

Both are legitimate; they answer different needs (guarantee vs flexibility).
The important point is that it stays cheap: a ceiling is the **same
comparator** applied at write time — reject an override that is weaker —
rather than at scan time. Building the comparator per knob (§5) is what keeps
that a later decision instead of a rewrite.

**Knob candidates**, ranked by (real need × enforcement point already exists):

1. `allowed_external_domains` — Google's allowlisted domains, M365's
   allow/block lists. **Deferred deliberately**: `OXICLOUD_EXTERNAL_EMAIL_DOMAINS`
   already covers much of this instance-wide, enforced at invite time
   (`magic_link_invite_service.rs:219`, `config.rs:4283`). The two are not
   duplicates — the env var gates *which external accounts may exist at all*,
   a policy knob would gate *which may be shared with per drive* — but the
   overlap is large enough that adding the narrow one before anyone has
   wanted it would be guessing. Revisit when a real case appears.
2. `forbid_download_for_viewers` — Google "prevent download/print/copy",
   M365 "block download". Table stakes against both, but advisory only
   (screenshots exist) and needs a new enforcement site on the download path.

Deliberately rejected: `default_link_scope` (a default, not a restriction —
and token grants are always Viewer today) and drive-creation restrictions
(a global role setting, not a per-drive policy). Already covered under
another name: Google's "can content be moved out of the shared drive" is
`forbid_cross_drive_move`.
