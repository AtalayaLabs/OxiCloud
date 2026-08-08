# Open Cloud Mesh (OCM) — Federated Sharing

Federate file-sharing with Nextcloud, ownCloud, Reva/OCIS, Seafile, and any
other OCM 1.1+ speaker. Track the design decisions here before code lands.

## Identity & auth model — DECIDED

OCM-federated recipients live as **shadow rows in `auth.users`** with:

- `is_external = true`
- `password_hash = NULL` (enforced by existing `users_external_no_storage`
  CHECK constraint)
- `email = federated-address` (e.g. `bob@nextcloud.example.com`)
- Federation columns (see "Schema rename — PRE-REQ" below for the
  migration path that turns today's OIDC-specific columns into these):
  - `federation_kind ∈ {NULL, 'magic_link', 'ocm', 'oidc'}` — distinguishes
    the trust chain that owns this identity. NULL for internal users.
  - `federation_issuer` — **THE AUTHORITY** that mints the subject id.
    NOT a display name. The full identity per federation kind is
    `(federation_issuer, federation_subject)`; the issuer is what makes
    a subject id meaningful — two different IdPs can independently mint
    `sub=1234` for two completely different people.
    - `oidc` — the `iss` claim from the id_token (also
      `discovery.issuer`), e.g.
      `https://sso.example.com/realms/main`. Immutable per
      spec (RFC 7519 §4.1.1).
    - `ocm` — the peer domain, e.g. `nextcloud.example.com`. The
      peer's domain IS the authority for its federated identities in
      OCM's model.
    - `magic_link` — NULL (identity is the local `email`, no external
      authority involved).
    - **Fixes an existing bug.** Today `oidc_provider` stores the
      human-readable `OXICLOUD_OIDC_PROVIDER_NAME` label (e.g. "SSO",
      "MockSSO"), not the issuer URL. Consequences of the current
      shape:
      1. Admin renaming `OXICLOUD_OIDC_PROVIDER_NAME` silently
         orphans every existing OIDC user — new lookups key on the
         new label, stored rows have the old one.
      2. BCL revocation (`revoke_user_sessions_by_oidc_subject`)
         keys on the label; same rename → BCL notifications from the
         IdP match no one. Silent security regression — users
         thought-kicked stay logged in.
      3. Multi-IdP futures (multi-realm, multi-tenant) would suffer
         cross-issuer subject collision if two IdPs share a display
         name.
    - Display name becomes a separate concern — either derived from a
      `peer_configs.issuer → display_name` lookup, or stored alongside
      as a denormalized `federation_display_name` column (admin-
      editable, no identity impact). Not part of the identity key.
  - `federation_subject` — the remote id string. Stability:
    - `oidc` — the `sub` claim, issuer-assigned, MUST NOT change per spec
      (RFC 7519 §4.1.2, OIDC Core §5.4). Email can change on the IdP
      side with no impact on this row.
    - `ocm` — the full federated address (`bob@nextcloud.example.com`).
      OCM 1.1 does NOT define a separate stable subject id; the address
      IS the identity. If a peer renames a user, we see a new address
      and create a new shadow row. Grants against the old row are
      orphaned until admin intervention. This is a protocol
      limitation — some peers ship extensions (Nextcloud Global Site
      Selector, ScienceMesh/Reva `opaqueUserId`) but they aren't in
      base OCM. Treat `federation_subject` as opaque; if a future OCM
      version adds a stable id, swap the value in without schema change.
    - `magic_link` — NULL (magic-link externals don't carry a federation
      subject; their identity is the local `email` column).
  - Composite UNIQUE index `(federation_kind, federation_issuer, federation_subject)`
    WHERE `federation_kind IS NOT NULL`.
- `storage_quota_bytes = 0`, no home folder provisioned

**Grants stay `subject_type = 'user'` on `storage.role_grants`.** No new
subject-type variant. The team's earlier refactor (2026-07-30) removing
`'external'` from the CHECK constraint is the precedent — external is a flag
on the user row, not a subject type.

**AuthZ engine unchanged.** Middleware for OCM WebDAV resolves sharedSecret →
`ocm.outbound_shares.recipient_user_id` (the shadow row), sets `CurrentUserId`
to that, and the existing `authz.require(subject, resource, permission)` runs
identically for local + federated subjects.

## Schema rename — PRE-REQ (ships before ANY OCM code)

Today's `auth.users.oidc_provider` / `oidc_subject` are semantically identical
to the generic `federation_issuer` / `federation_subject` above. Keeping both
would be drift risk. Rename first, then start OCM work on top of the clean
shape.

```sql
ALTER TABLE auth.users RENAME COLUMN oidc_provider TO federation_issuer;
ALTER TABLE auth.users RENAME COLUMN oidc_subject  TO federation_subject;
ALTER TABLE auth.users ADD  COLUMN federation_kind TEXT
    CHECK (federation_kind IN ('magic_link', 'ocm', 'oidc'));

-- Backfill kind: rows that had oidc_provider set were all OIDC-linked.
UPDATE auth.users SET federation_kind = 'oidc'
    WHERE federation_issuer IS NOT NULL;

-- Swap the anti-duplicate index.
DROP   INDEX idx_users_oidc;
CREATE UNIQUE INDEX idx_users_federation
    ON auth.users(federation_kind, federation_issuer, federation_subject)
    WHERE federation_kind IS NOT NULL;
```

**Value migration is more than a rename** — today's stored values in the
renamed `federation_issuer` column are DISPLAY LABELS (e.g. `'MockSSO'`,
`'MyIdP'`), not issuer URLs. Rewriting them to real issuer URLs
touches every existing OIDC row and can't happen in one atomic step without
losing rollback ability. Phase it:

### Rename PR — Phase A (schema)
- Add columns (`federation_kind`, `federation_issuer` renamed from
  `oidc_provider`, `federation_subject` renamed from `oidc_subject`) —
  the SQL block above.
- Application code DUAL-WRITES on OIDC login: still writes the display
  label to `federation_issuer` (unchanged semantics) AND stamps
  `federation_kind = 'oidc'`. Reads unchanged.
- Ships safely as a pure rename + kind-backfill. Rollback = drop the
  `federation_kind` column; the renamed columns keep working with the
  old code because their VALUES haven't changed.

### Rename PR — Phase B (progressive lazy rebind on OIDC login)

**Simplification** — no explicit backfill CLI needed. The id_token
carries the true `iss` claim on every OIDC login, so the migration
does itself organically:

- On every successful OIDC login: if the user's stored
  `federation_issuer` does not equal the id_token's `iss`, UPDATE it
  (with an audit line `event="federation.issuer_rebound"`,
  `reason="lazy_backfill"`). First login post-upgrade heals the row.
- Users who never log in stay on the legacy label — but they also
  can't do anything (no login → no action), so the stale value is
  harmless. It appears only in admin listings, where a "legacy label
  needs rebind" badge could surface if we care.
- Contradiction guard: if the observed `iss` is DIFFERENT from the
  stored issuer AND the stored issuer is already a valid URL (not a
  display label), that's a genuine identity change — refuse the login
  with an audit event and admin alert. Preserves "identity change is
  admin-mediated." Heuristic for label vs URL: starts with `http://`
  or `https://` and contains `/`.
- Same lazy-rebind logic covers BCL: if a BCL notification's `iss`
  matches a user's `federation_subject` but issuer differs, we can
  either rebind (safer bet: same peer just told us their true issuer)
  or refuse and audit. Recommend refuse-and-audit, since BCL is
  server-to-server and less user-driven.

**CLI subcommand shape (optional, ships later if needed)** — lives
under `oxicloud-cli federation …` (see `src/bin/oxicloud-cli.rs` for
the domain/action pattern; add `federation` module alongside
`opaque`). Two subcommands worth having if operators want proactive
control:

- `federation status` — show counts of users by `federation_kind` and
  how many still hold display-label-shaped `federation_issuer` values.
- `federation remap-issuer --from-label X --to-issuer https://...` —
  bulk-update the users still holding a specific legacy label, for
  operators who want to close the migration without waiting for
  every user to log in.

**All user-identifier arguments accept EITHER username OR user UUID.**
Not all users have a username (email-only signups per PR-18 leave it
NULL); the CLI must handle both, resolving via
`SELECT id FROM auth.users WHERE username = $1 OR id::text = $1`.
Applies to any subcommand that takes a user reference.

- New admin CLI or boot-time task: for each row where
  `federation_kind = 'oidc'`, try to derive the true issuer URL:
  - **Single-IdP deployment** (typical): if the current OIDC config's
    `issuer_url` is set AND no other candidate exists, UPDATE all rows
    to that issuer. One-line log per row updated.
  - **Multi-value case** (deployment has renamed
    `OXICLOUD_OIDC_PROVIDER_NAME` mid-life, so the same physical IdP
    has rows with different labels): auto-backfill would smear all
    labels into the SAME issuer — WRONG for rows that predate the
    label swap. **Refuse to auto-backfill**; emit a boot audit warning
    listing the distinct current values + affected user_ids; require
    operator to pick a mapping via CLI (`oxicloud federation
    remap-issuer --from-label X --to-issuer https://…`) or accept
    the current-config issuer for all rows via
    `--all-legacy-labels`.
  - **Genuine multi-IdP** (rare today, forward-looking): fail loud;
    no automatic mapping is safe. Manual mapping per label.
- Log each mapping decision to `audit` for post-hoc traceability.
- Ship after Phase A has been in production long enough to prove
  stable (one release cycle at minimum).

### Rename PR — Phase C (read switch + lazy rebind)
- Application code reads anti-duplicate lookups AND BCL revocation
  keying on `(federation_kind, federation_issuer, federation_subject)`.
- On EVERY successful OIDC login: if the row's current
  `federation_issuer` value doesn't equal the id_token's `iss` claim,
  UPDATE it (with an audit line). This catches rows Phase B couldn't
  resolve automatically — the first login after rollout self-heals.
- Dual-write continues (kind + issuer both stamped from id_token now).
- Hard failure mode to watch for: if the `iss` on the id_token
  contradicts a Phase-B-backfilled value for existing users, we've
  either (a) misconfigured the IdP, or (b) the IdP is a completely
  different one than we backfilled to. Refuse the login with an audit
  event and admin alert rather than silently rebind — preserves the
  principle "identity change is admin-mediated".

### Rename PR — Phase D (drop legacy compatibility)
- Precondition check: no rows remain with a `federation_issuer` value
  that looks like a display label (heuristic: doesn't start with
  `http://` or `https://`, doesn't contain `/`).
- Remove any dead code paths that assumed display-label semantics.
- No column drop needed — the columns are already renamed. This phase
  is code-only.
- Ships when telemetry from Phase C shows zero unresolved
  federation_issuer values across the fleet.

Total: rename in one release cycle, value backfill in the next, read
switch after that, cleanup at leisure. Each phase reversible. Operator
warnings surface the exceptional cases (multi-label, multi-IdP) so
they don't get silent-defaulted into a broken state.

Rust-side rename (~15-20 sites, mechanical):
- `User` entity fields (`oidc_provider` → `federation_issuer` etc.)
- `UserPgRepository` SQL statements
- `OidcIdClaims → User` mapping in `auth_application_service` (sets
  `federation_kind = 'oidc'`)
- `admin_settings_service`'s env-override table
- Any DTOs that leaked `oidc_provider` externally — check `UserDto` before
  the migration in case FE reads the field
- Tests

Why rename rather than coexist:
- `oidc_provider` is a misleading name anyway — a value like
  `MyIdP` isn't a "provider" (that's a category), it's the IdP's
  identity/name. `federation_issuer` reads more truthfully.
- Adding OCM alongside without renaming forces every future query that asks
  "is this user externally-owned?" to check two column pairs. One canonical
  shape is cheaper.

The rename can ship as an independent PR before OCM work starts. Backfilling
`kind = 'oidc'` for existing rows is monotone (no NULL flips), so the
migration is reversible if we need to abort.

## Magic-link on federated addresses — DECIDED

Default: **`federation_kind='ocm'` users CANNOT use magic-link login.**
Same rule shape as the existing OIDC-master rule
(`feedback_oidc_master_no_magic_link_bypass`).

Enforcement in `is_magic_link_login_allowed_for(&user)`:

```
if user.federation_kind == Some(FederationKind::Ocm) {
    return false;  // audit: reason="ocm_federated"
}
```

Rationale:
- OCM's identity boundary is the remote peer's authentication (which may
  enforce MFA, geo-fence, hardware keys). Magic-link would downgrade that to
  mailbox-strength.
- Federation revocation on the remote side (peer disables bob) should NOT
  leave bob with a working login on our side via his mailbox.
- Same anti-enumeration shape as `oidc_user` rejection — 404 to the caller,
  audit line internally.

### Deferred opt-in: `allow_magic_link_for_ocm_federated`

Some deployments will want unified UX (federated invite doubles as a browser
account). Ship as an additive policy on `OXICLOUD_AUTH_POLICIES`:

```
OXICLOUD_AUTH_POLICIES=allow_magic_link_for_ocm_federated
```

Same shape as `permit_magic_link_for_password_users` — off by default,
operators who enable it accept the auth-boundary trade-off. Document alongside
the shared-computer / weakened-boundary caveat.

**Not implemented in the initial OCM ship.** Add when a deployment actually
asks for it.

## Reverse case: address already exists as magic-link external

If `bob@example.com` is already in our DB as a magic-link external (a local
user invited him last month) AND a peer sends us an OCM share addressed to
`bob@example.com`, do NOT upgrade in place. Create a separate shadow row
with `federation_kind='ocm'`. Different trust chains, different identities.
The composite UNIQUE index on `(federation_kind, federation_issuer,
federation_subject)` allows the two rows to coexist.

Consequence: magic-link bob CAN log in; OCM bob CANNOT. Same address, two
accounts, two postures. Honest about the different trust semantics.

## New DB shape

```
ocm.trusted_peers   -- allowlist of federation partners
ocm.outbound_shares -- shares WE created for remote users (with acceptance state)
ocm.inbound_shares  -- shares OTHER servers created for our users (with acceptance state)
ocm.notifications   -- wire-level OCM notifications (retry queue + dedup + forensic audit)
```

### `ocm.notifications` in detail

**Not to be confused with acceptance state.** When a user clicks
Accept/Decline on a pending invitation, that mutates
`ocm.inbound_shares.state` — the user's decision lives there. This
table stores the *wire message* we then send to the peer telling them
"our user accepted." The two are logically distinct: acceptance is
share-scoped workflow state, notifications are transport-layer events.
Future readers: don't conflate.

Given that, `ocm.notifications` serves FOUR overlapping purposes.
Collapsing them into one table (instead of parallel queue / dedup /
audit tables) avoids drift.

**1. Retry queue for outbound.** OCM 1.1 requires exponential-backoff
retry when the peer's `POST /notifications` fails. Rows with
`direction='outbound' AND processing_result='pending' AND
next_retry_at <= now()` are what a background worker picks up.

**2. Idempotency / dedup for inbound.** Peers may resend after network
hiccups. `UNIQUE(peer_domain, remote_notification_id)` short-circuits.

**3. Debugging.** "Why did that share disappear?" — grep by
`related_share_id`. Payload preserved verbatim as JSONB.

**4. Security audit.** Federated state transitions cross trust
boundaries; every one is auditable, joinable to peer domain + share.

Sketch:

```sql
CREATE TYPE ocm.notification_direction AS ENUM ('inbound', 'outbound');
CREATE TYPE ocm.notification_kind AS ENUM (
    'share_accepted', 'share_declined', 'share_revoked',
    'share_unshared', 'user_removed', 'other'
);
CREATE TYPE ocm.notification_result AS ENUM (
    'pending', 'success', 'failed', 'ignored'
);

CREATE TABLE ocm.notifications (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    direction              ocm.notification_direction NOT NULL,
    peer_domain            TEXT NOT NULL,
    related_share_kind     TEXT CHECK (related_share_kind IN ('inbound','outbound')),
    related_share_id       UUID,        -- points at inbound_shares.id OR
                                        -- outbound_shares.id; typed union
                                        -- isn't natural in Postgres so no
                                        -- hard FK — verify at insert
    kind                   ocm.notification_kind NOT NULL,
    payload                JSONB NOT NULL,
    remote_notification_id TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at           TIMESTAMPTZ,
    processing_result      ocm.notification_result NOT NULL DEFAULT 'pending',
    attempt_count          INTEGER NOT NULL DEFAULT 0,
    next_retry_at          TIMESTAMPTZ, -- outbound only
    error_message          TEXT,
    UNIQUE (peer_domain, remote_notification_id)
);

CREATE INDEX idx_ocm_notif_related
    ON ocm.notifications(related_share_id, related_share_kind);
CREATE INDEX idx_ocm_notif_retry
    ON ocm.notifications(next_retry_at)
    WHERE direction = 'outbound' AND processing_result = 'pending';
CREATE INDEX idx_ocm_notif_peer
    ON ocm.notifications(peer_domain, created_at DESC);
```

**Retry policy** — attempt 1 immediate; 2-5 with exponential backoff
(30s, 2m, 10m, 1h); after attempt 5, mark `failed` and emit
`event="ocm.notification_delivery_gave_up"` with admin alert.
Configurable via `OXICLOUD_OCM_NOTIFICATION_MAX_ATTEMPTS` (default 5).

**Retention** — 90 days default, purged by a nightly consistency-job
handler (`notifications_cleanup`), fits the existing recovery-jobs
pattern. `failed` rows kept indefinitely until admin resolution
(paper trail). Configurable via
`OXICLOUD_OCM_NOTIFICATION_RETENTION_DAYS`.

**Inbound security invariants** — verify BEFORE mutating any state:

- Peer domain matches the peer that owns the referenced share
  (`peer_mismatch` → mark `ignored`, audit).
- `related_share_id` points at an existing share in the correct table
  (`unknown_share` → `ignored`).
- Notification kind is compatible with current share state
  (`state_conflict` → `ignored`, e.g. accept on an already-declined
  share).

Each rejection emits an `event="ocm.notification_rejected"` audit line
with a stable `reason=` field (`peer_mismatch | unknown_share |
state_conflict | duplicate`) — same anti-drift discipline as other
structured audit events, per `feedback_enum_over_string_literals_in_logs`.

Plus `auth.users` new columns above.

Plus `storage.resource_kind` extension for a `RemoteShare` resource type
(pointing at `ocm.inbound_shares.id`), so accepting an inbound share is a
regular grant row from the accepting user to the RemoteShare resource.

## Invitation workflow — where accept/decline lives

Local grants have no accept/decline concept — the grant IS the access.
OCM introduces one because the protocol is push-driven (peer POSTs a
share, our user decides whether to surface it). Solved WITHOUT adding
state to `role_grants`:

**Inbound shares (peer → us):**
- `POST /ocm/shares` creates an `ocm.inbound_shares` row (pending). NO
  role_grant created yet.
- User sees pending invitation in `/shared-with-me` → `Federated` tab
  (new), plus a badge on AppShell.
- **Accept** → insert `role_grants` row (subject = local user, resource
  = a new `RemoteShareResource` entry, permission = as offered), POST
  accept notification to peer, mark `ocm.inbound_shares.accepted_at`.
- **Decline** → no grant created; POST decline notification, mark
  `ocm.inbound_shares.declined_at`. Row kept for audit.
- Modal prompt on next login if pending count changed since last login.

**Outbound shares (us → peer):**
- Grant is created immediately in `role_grants` (subject = shadow user
  for the federated recipient). AuthZ from OUR side is active the
  moment the OCM POST succeeds — remote user CAN consume via WebDAV.
- `ocm.outbound_shares` tracks delivery + acceptance state
  (`delivering | delivered | accepted | declined_by_recipient |
  undeliverable | revoked`), surfaced in `/shared` (outgoing view) as
  a status column.
- Share dialog recognizes `user@remote-domain` and shows a channel
  badge on the resolved recipient (see
  [federated-login.md § Same discovery layer serves invitation](./federated-login.md)).
- Peer sends decline notification → auto-revoke the local grant
  (recipient said no, keeping access is pointless). Configurable
  behaviour deferred.

**ReBAC extensions needed** (all local to the OCM code path, no engine
churn):
- New `resource_type = 'remote_share'` on `role_grants`, resource_id
  pointing to `ocm.inbound_shares.id`. AuthZ dispatches to a
  WebDAV-proxy handler for this type.
- `ocm.inbound_shares.state` and `ocm.outbound_shares.state` columns
  hold acceptance metadata. `role_grants` stays state-free.
- OCM notification handler translates peer messages into local state
  transitions (create/delete grant, update state column).

## Future — merge with a generic grant-request workflow

`ocm.inbound_shares.state` is a purpose-specific state machine for the
MVP. If OxiCloud later gains a local pending-approval workflow (Alice
asks Bob for Read on his folder; Bob approves) — a real
enterprise-features ask — the natural refactor is to extract a shared
workflow spine:

- New table `storage.grant_requests` with
  `(subject, resource, requested_permission, approval_authority, state,
  decided_at, decided_by)`. Approval_authority distinguishes who holds
  decide-power: `owner` (local approval), `recipient` (OCM inbound
  accept, magic-link redeem), `admin` (policy gate).
- `ocm.inbound_shares` keeps its federation-specific columns
  (peer_domain, remote WebDAV URL, sharedSecret) and gains a
  `grant_request_id` FK. Accept = flip request to `approved` + create
  `role_grant`. Decline = flip to `rejected`.
- `magic_link_tokens` with `resource_kind ≠ NULL` (invitation-purpose
  tokens) gets the same FK. Redeem = flip to `approved` + create
  `role_grant`.
- **`role_grants` stays approved-only.** Hot AuthZ path doesn't gain a
  state filter — no regression on read paths.

**NOT** a merge of the tables themselves — collapsing OCM /
magic-link / local-request into `role_grants` with a state column
would leak state-machine logic into every AuthZ check. The merge is at
the ABSTRACTION level: three flavors of "prospective grant" today
(magic-link invite, OCM inbound, hypothetical local request) share ONE
workflow table; channel-specific data stays in satellites.

Migration is additive and non-breaking: add tables + FK columns,
backfill existing rows as synthetic `approved` requests for audit
continuity, refactor callers one at a time. No ReBAC engine change.

Not urgent — the OCM MVP works fine without it. Recorded so the
option isn't forgotten if local approval workflow ever gets requested.

## Phasing (rough)

1. **Phase 0** — Discovery (`/ocm-provider`) + inbound receive + FE listing
   of received shares (metadata only, no consumption yet). ~1 week.
2. **Phase 1** — Consume remote shares (server-side WebDAV proxy). ~1-2 weeks.
   Needs a WebDAV client — none exists in the tree today.
3. **Phase 2** — Outbound sharing (share dialog recognizes `user@remote`).
   ~3-5 days.
4. **Phase 3** — OCM 2.0 invite handshake (ScienceMesh trust flow). ~1 week.
5. **Phase 4** — Address book / federated-contact registry. ~1 week.

## Open design questions (not yet decided)

- **Trust model** — open federation vs allowlisted peers? Recommend
  allowlist-by-default (safer for self-hosted).
- **Recipient resolution rule** — `username@LOCAL_DOMAIN` vs `email` match?
  Recommend username-based (matches Nextcloud, deterministic).
- **Quota accounting** — do consumed remote shares count against the
  recipient's `storage_quota_bytes`? Recommend no (remote storage is remote).
- **Notification authenticity** — bind every OCM notification to its
  `inbound_share.id`, refuse if peer domain doesn't match the domain we
  recorded at share creation.
- **Group grants** — local group containing OCM-federated user is weird.
  Semantically fine but the share dialog should probably discourage it.

## Interop targets

- Nextcloud in Docker as the primary peer (both directions).
- OCIS/Reva as the CS3-blessed reference behaviour.
- SciMesh integration tests once Phase 3 is in.

## References

- OCM 1.1 spec: <https://cs3org.github.io/OCM-API/>
- Nextcloud OCM docs (implementation quirks): <https://docs.nextcloud.com/server/latest/admin_manual/configuration_files/federated_cloud_sharing.html>
- Reva (Go reference impl): <https://github.com/cs3org/reva>
