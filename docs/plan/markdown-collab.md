# Plan — Collaborative markdown & text editor (Phase A shipping bundle)

## Status (2026-09-14 update)

**Phase A partially shipped.** Three of the seven sub-phases from
[§ Phasing](#phasing) below already landed in sibling PRs — they
appear in this plan for context but are no longer scope for the
current branch:

- **C3 — Folder-live updates.** `MessageBusEvent::FileCreated / Deleted / …`
  publish sites are wired in `FileManagementService`, `FolderService`
  and `TrashService`; FE consumes via `useTopic` /
  `useFolderTopic` composables.
- **C4 — Notifications table + bell.** `notif.notifications`
  migration (`20261026000000_notifications.sql`), `NotificationService`,
  REST surface (`/api/notifications/*`), bell UI and toast are all
  live. `share.granted` firing was validated end-to-end.
- **Message-bus substrate itself.** WebSocket + JSON-RPC control
  plane (`rt.subscribe` / `rt.event` / `rt.revoked`) + AuthZ
  eviction is shipped and documented in
  [`docs/architecture/message-bus-and-notifications.md`](../architecture/message-bus-and-notifications.md).

**This branch scopes the remaining collab-specific work: C1, C2,
C5, C6, C7 (see § Phasing).** The plan sections describing
notifications, folder-live updates, and the bus protocol are kept
verbatim below as design record — but the arch doc linked above is
the durable reference for anything already live.

## Context

OxiCloud stores markdown, notes, plain-text files — but has no way
to open one in the browser and edit it, let alone edit it with
someone else in real time. This plan lands a Google-Docs-style
collab editor for `.md` and `.txt` files: multiple users, live
remote cursors, no conflicts, no data loss on reconnect.

It's the marquee feature of **Phase A** of
`docs/architecture/message-bus-and-notifications.md`. Because Phase A ships as one bundle,
this plan also covers the two adjacent surfaces that make the collab
UX feel complete:

- **Folder-live updates** so the folder you're editing from
  reflects other users' uploads/renames/deletes without a manual
  refresh.
- **Notifications table + bell** so "someone shared X with you",
  "you were mentioned", and "your job finished" surface as
  first-class in-app notifications.

The bus itself is out of scope here — it's the infrastructure
`docs/architecture/message-bus-and-notifications.md` delivers. This plan is what consumers of
that bus look like.

## Non-goals

- WYSIWYG rich text (later, on ProseMirror with `y-prosemirror` — see
  § Migration path). Bundling WYSIWYG into the first ship would
  collapse two risks (CRDT wire + editor round-trip fidelity) into
  one PR. Splitting them lets us pin the CRDT / eviction / flush
  path against the simpler `Y.Text` root before tackling
  markdown-parse round-trip fidelity.
- Comments / @mentions / reactions inside the editor — that's
  Phase B territory. Hook points reserved.
- Suggestion mode / tracked changes.
- Non-text files (photos, PDFs, spreadsheets, whiteboard).
- Chat between editors. Presence + cursors is enough; if you need
  words, add a comment.

## Stack decision

- **CRDT:** Yjs on the frontend, `yrs` (y-crdt Rust) on the backend.
  Wire-compatible by design. Same maintainer, mature.
- **Editor:** CodeMirror 6 + `@codemirror/lang-markdown` +
  `y-codemirror.next`. Remote cursors + selections come for free.
- **Transport:** the WebSocket + topic bus from
  `docs/architecture/message-bus-and-notifications.md`; binary frames on topics
  `collab:{file_id}` and `collab:{file_id}:awareness`.
- **Persistence:** `collab.doc_sessions` table for CRDT state; the
  underlying `.md` blob is flushed via `FileManagementService` so
  dedup, versioning, quota, and audit stay consistent.

## Data model — collab doc sessions

```sql
CREATE SCHEMA IF NOT EXISTS collab;

-- One row per file that has EVER had a collab session.
-- CRDT state persists across all clients disconnecting.
CREATE TABLE collab.doc_sessions (
    file_id                    UUID PRIMARY KEY
                               REFERENCES storage.files(id) ON DELETE CASCADE,
    state                      BYTEA NOT NULL,   -- serialized yrs::Doc snapshot
    state_vector               BYTEA NOT NULL,   -- for cheap catch-up on reconnect
    updates_since_snapshot     INTEGER NOT NULL DEFAULT 0,
    last_flushed_content_hash  TEXT,             -- content hash of last blob write
    last_flushed_at            TIMESTAMPTZ,
    last_activity_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at                 TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_doc_sessions_stale ON collab.doc_sessions(last_activity_at);
```

Notes:

- `file_id` keyed: matches the blob it flushes to; no ambiguity.
- `ON DELETE CASCADE` handles file trash / permanent-delete cleanly.
- **Compaction:** every 200 updates or 60 s, the session actor
  re-serializes as a single snapshot and resets
  `updates_since_snapshot`.
- **Version history (deferred):** if we want it, add
  `collab.doc_snapshots (file_id, bytes, label, created_at)` and
  take a snapshot on each flush-to-blob. Cheap durable history with
  authorship intact — see "Later" section.

## Data model — notifications (SHIPPED — kept as design record)

Landed as `migrations/20261026000000_notifications.sql`. Small
drift from the plan sketch below:

- **`kind`** is unconstrained TEXT (as sketched) — no CHECK, no
  enum. New kinds land as new string literals in the ingester with
  no migration.
- **`data JSONB`** — as sketched; used today for the resource
  descriptor block (name, drive, etc.) attached to `share.granted`
  events for the bell's rich preview.
- **`subject_type`** — as sketched, kept open-ended.
- The two indices are exactly as sketched.

```sql
CREATE SCHEMA IF NOT EXISTS notif;

CREATE TABLE notif.notifications (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    recipient_id  UUID NOT NULL,                       -- who sees it
    kind          TEXT NOT NULL,                       -- stable enum key
    subject_type  TEXT NOT NULL,                       -- 'file' | 'folder' | 'job' | 'session' | 'quota' | ...
    subject_id    UUID,                                -- nullable for non-resource subjects (quota, session)
    actor_id      UUID,                                -- who triggered it (nullable for system)
    data          JSONB NOT NULL DEFAULT '{}'::jsonb,  -- kind-specific fields
    read_at       TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_notif_recipient_unread ON notif.notifications(recipient_id, read_at, created_at DESC);
CREATE INDEX idx_notif_recipient_all    ON notif.notifications(recipient_id, created_at DESC);
```

**Kinds live in production today:** `share.granted` (direct + group
routes), the `notification_received` live-delivery envelope, plus
the placeholder rows the ingester writes ahead of enrichment. The
richer catalog below (`share.link_created`, `job.completed_for_you`,
`session.new_device_login`, `quota.threshold_reached`, …) is the
target set — each is an ingester hook not yet wired.

Initial `kind` catalog (stable strings — new denial cause = new kind,
never repurpose an existing one, per project convention):

- `share.granted` — subject = file/folder shared directly with the recipient
- `share.revoked`
- `share.granted_via_group_membership` — subject = file/folder reached
  through a group the recipient belongs to. Distinct kind so the FE can
  present the "via group X" attribution and so aggregation buckets it
  separately from direct grants. Never repurpose `share.granted` for
  this case.
- `share.revoked_via_group_membership`
- `share.link_created` — subject = file/folder the link points at;
  recipient = the link creator (public-link `Token` principal has
  nobody else to notify)
- `share.link_revoked`
- `job.completed_for_you` — subject = job the recipient started
- `job.failed_for_you`
- `session.new_device_login` — subject = session
- `quota.threshold_reached` — data = `{ pct: 80 | 95, drive_id? }`
- `magic_link.used_from_other_device` — security signal
- (Phase B) `comment.mentioned_you`, `comment.reply_to_you`

Delivery: `NotificationService::create` inserts the row **and**
publishes to `user:{recipient_id}:notifications`. The bus delivery
is a live-delivery optimization; the row is the source of truth.

## Backend

### `CollabSessionService` (application service)

Long-lived actor registry — not per-request.

```rust
pub struct CollabSessionService {
    sessions: DashMap<FileId, Arc<CollabSession>>,  // one actor per open doc
    bus:      Arc<dyn MessageBus>,
    files:    Arc<FileManagementService>,
    authz:    Arc<AuthorizationEngine>,
    repo:     Arc<dyn DocSessionRepository>,
    limits:   CollabLimits,   // max_doc_bytes, max_sessions, idle_ttl
}

impl CollabSessionService {
    pub async fn attach(&self, file_id: FileId, caller: UserId, socket: SocketId)
        -> Result<AttachHandle, CollabErr>;

    /// Called from the WS handler for each 0x01 binary frame from a
    /// client with Edit permission. Reader frames are dropped upstream.
    pub fn apply_update(&self, file_id: FileId, actor: UserId, bytes: Bytes);

    /// 0x02 frames: awareness. Never persisted; just fan out on
    /// bus topic collab:{doc}:awareness.
    pub fn apply_awareness(&self, file_id: FileId, actor: UserId, bytes: Bytes);

    pub async fn detach(&self, file_id: FileId, socket: SocketId);
}
```

Per-doc `CollabSession` actor owns:

- `yrs::Doc` (loaded from `collab.doc_sessions.state` or seeded from
  the current blob text)
- `awareness::Awareness`
- `HashSet<SocketId>` of currently-attached sockets
- Debounced flush-to-blob task

Flow:

1. **First attach:**
   - If row exists: `Doc::apply_update(state)`.
   - Else: read the blob text via `FileManagementService::get_content`,
     create a new `Doc` with a single `Y.Text`, insert the initial
     content, persist the seeded snapshot immediately.
2. **On CRDT update from a client (via `apply_update`):**
   - `doc.transact_mut().apply_update(&update)`.
   - `bus.publish(Topic::Collab(id), MessageBusEvent::CrdtUpdate {
     bytes })` — fans out to other attached sockets. Handler filters
     out the source socket via an origin marker.
   - Increment update counter; if past threshold, compact snapshot and
     persist.
   - Mark dirty; schedule debounced flush-to-blob.
3. **On awareness update:** `bus.publish(Topic::CollabAwareness(id),
   …)`. Never persisted.
4. **Debounced flush-to-blob** (default 15 s idle or 60 s max):
   - Extract `Y.Text` value → UTF-8 bytes.
   - Compute content hash; compare to `last_flushed_content_hash`;
     skip if identical.
   - Call `FileManagementService::write_content(caller =
     "collab-service", file_id, bytes)` — go through the normal API
     to preserve dedup, versioning, quota, and audit.
   - Update `last_flushed_content_hash` + `last_flushed_at`.
5. **Idle GC** (background task on the service): rows where
   `last_activity_at < now - idle_ttl` AND no attached sockets →
   force a final flush → drop the row and the actor. `idle_ttl`
   default 30 min.

### AuthZ

Collab piggybacks on the three-gate-class model from
`docs/architecture/message-bus-and-notifications.md` — the full matrix and the wire/audit reason
vocabulary live there; this section covers the collab-specific gates
and the write-side check that only applies to `0x01` frames.

**Three gate classes** (short-form recap; source of truth is the arch
doc's [§ The three AuthZ scopes](../architecture/message-bus-and-notifications.md#the-three-authz-scopes)):

- **Class 1 — Resource-scoped**: default gate is
  `AuthorizationEngine::require(caller, resource, Read)`; a few topics
  use a stricter permission (`Share`, `Comment`) when the topic itself
  would leak enumeration metadata.
- **Class 2 — Identity-scoped**: `caller_id == subject_uuid` equality
  only. No admin bypass — privacy is a hard rule.
- **Class 3 — Role-scoped**: `caller.role == Admin` (or a future
  admin sub-role).

**Collab-specific topic gates** (all Class 1):

| Topic | Permission | Notes |
|---|---|---|
| `collab:{file_id}` | `Read` on the file | Reader = view + own cursor; edit gate is enforced separately on write frames, not on subscribe. |
| `collab:{file_id}:awareness` | `Read` on the file | Presence broadcasts include the caller's own cursor to other readers/editors — matches Google Docs behaviour. |
| `file:{id}:comments` (Phase B) | Whatever the REST comments API decides — `Read` if comments are public to Readers; `Comment` if commenter-tier only | Consistency with REST. The bus never invents a new policy. |
| `file:{id}:shares` (Phase B) | `Share` (Owner-tier) | Share list is management metadata; Reader can read the file but not enumerate other grantees. Handled by the bus, listed here for completeness because the collab editor's share dialog subscribes to it. |

**Per-lifecycle checks** (collab-specific):

- **Subscribe** (JSON-RPC `rt.subscribe` on `collab:{file_id}`):
  `authz.require(caller, file, Read)`. Deny → JSON-RPC error object
  with `code: -32001, message: "no_read"` on the same request `id`.
  Audit `event = "message_bus.subscribe_denied"` with
  `reason ∈ {no_read, no_such_resource}` — anti-enumeration collapse
  is honoured (`no_read` and `no_such_resource` share the wire
  reason; audit records the truth).
- **Attach** (`0x03` sync-step-1 binary frame — only accepted from
  callers who already subscribed and passed the Read gate above):
  additional session-level checks that don't fit the subscribe path.
  Deny → close WS 1008 with reason. Audit
  `event = "collab.attach_denied"` with
  `reason ∈ {resource_deleted, too_large}`. Note: `no_read` /
  `no_such_resource` are caught at subscribe, not here.
- **Per-update frame** (`0x01` c→s — write path, NOT the subscribe
  path): `authz.require(caller, file, Edit)` cached for the session
  lifetime; only re-checked when `user:{caller}:authz` fires or on
  grant-revoke eviction. Drop the frame + emit a JSON-RPC
  notification `rt.write_denied` with `code: -32007, message:
  "no_edit"` (Class-1 write-side denial uses the same error-code
  vocabulary as subscribe denials — the canonical `error_code::*`
  constants live in `application/ports/message_bus_ports.rs` and are
  documented in the arch doc's [§ Wire protocol](../architecture/message-bus-and-notifications.md#wire-protocol)).
  Audit `event = "collab.write_denied"` with
  `reason ∈ {no_edit, session_evicted, external_write_conflict}`.
  This is a Class-1 topic where subscribe (Read) and write (Edit) use
  different permissions — enforced at different frame kinds, not at
  subscribe time.
- **Awareness frame** (`0x02`): no separate check — awareness is
  presence, gated only by subscribe (`Read`). A Reader broadcasts
  their cursor; that's by design.
- **Flush-to-blob** uses a synthetic caller identity
  (`user:collab-service`) but the audit trail records the last N
  contributors (embedded in Yjs itself via the `origin` field on each
  update), so post-hoc attribution to the responsible human callers is
  preserved.

**Eviction** — collab sessions honour the standard eviction path:
`AuthzChanged { affected: [file_id] }` on `user:{caller}:authz` →
WS handler drops the `collab:{file_id}` sub → the session actor's
socket-set shrinks → last socket out triggers idle-GC → forced final
flush + row cleanup. Audit `event = "message_bus.subscription_evicted"`
with `reason ∈ {grant_revoked, resource_deleted, group_membership_lost,
admin_kick}`.

Slice status (2026-09-21):

- ✅ **Graceful `rt.write_denied` on UPDATE denial** — the collab
  binary-frame router in `rt_ws.rs` no longer closes the WS on
  `CollabError::AuthzDenied { permission: "update" }`. Instead it
  emits an `rt.write_denied { file_id, reason: "no_edit" }` JSON-RPC
  notification and continues the session loop. FE `MessageBus` gains
  a per-file `registerWriteDeniedHandler(fileId)` (parallel to
  `registerBinaryHandler`); `CollabDoc.connect` registers one and
  flips `#canWrite` to false + fires `onCapabilities` — the editor
  drops into read-only immediately. Read-side denial (SYNC) still
  closes the socket — that's an eviction condition, not a per-frame
  refusal. Guarded by hurl S26 (Viewer sends UPDATE → rt.write_denied
  fires → rt.ping still succeeds).
- ✅ **Read-only editor for Viewers** — the `collab:<file_id>` subscribe
  ack now carries `capabilities.can_write` (from a second AuthZ pass
  on `Permission::Update`). The FE's `CollabDoc` reads it, suppresses
  outbound UPDATE frames when false, and the `CollabEditor` mounts
  CodeMirror through a live `EditorState.readOnly` compartment that
  starts read-only (fail-closed) and flips on the ack. A "Read only"
  status pill renders alongside the sync pill. Guarded by hurl S25
  (Owner sees `can_write: true`, Viewer sees `can_write: false` with
  a successful subscribe).
- ✅ **`resource_deleted`** — `FileManagementService::delete_and_cleanup_with_perms`
  calls `CollabSessionService::evict_sessions_for_file` before the
  trash / permanent-delete branches. Actor publishes
  `INTERNAL_KIND_EVICTED` (server-only wire kind `0xFE`) on the
  outbox; the WS forwarder translates that to an `rt.revoked
  { topic: "collab:<id>", reason: "resource_deleted" }` text frame
  and unwinds. Guarded by hurl S24 in `tests/api/rt_bus_check.sh`.
- ✅ **`grant_revoked` (file-scoped)** — `AuthzChanged` now carries
  `affected_files: Vec<Uuid>` alongside `affected_folders`. The
  grant-handler revoke path publishes `affected_files` when the
  resource is a `Resource::File`; the WS reader translates that
  into `SessionOut::EvictCollab`, and the main loop emits
  `rt.revoked { reason: "grant_revoked" }` on the affected
  `collab:<file_id>` topics — symmetric to the pre-existing
  folder-topic eviction cascade. Guarded by hurl S27 (user2 has
  a file-scoped Viewer grant, subscribes to collab, grant revoked
  → revoked entry appears with grant_revoked reason).

  **Folder-level revoke cascade to descendant collab sessions:
  intentionally NOT wired.** (Design considered 2026-09-21, rejected.)
  The reasoning for future implementors:

  - The per-frame Update AuthZ gate (see [`collab_session_service`'s
    write path](../src/application/services/collab_session_service.rs)
    + hurl S19 / S26) already blocks post-revoke UPDATEs at the wire.
    A caller who loses Update via a folder revoke sees `rt.write_denied`
    on their next keystroke (B's graceful denial); the FE's read-only
    slice flips `#canWrite` and reconfigures the editor to read-only
    within that same tick.
  - `CollabSessionService::evict_sessions_for_file` publishes the
    eviction control on the outbox and immediately breaks the actor
    loop — it does NOT flush pending dirt first. Adding a folder-cascade
    eviction path would therefore *lose* up to `debounce_max` (60s
    prod) of pre-revoke edits that were authorized when applied and
    are sitting in the debouncer waiting for the next flush tick.
  - Skipping the cascade preserves those authorized edits: the actor
    keeps running, the debouncer fires normally, pre-revoke edits
    land in the blob, only post-revoke ephemeral keystrokes (which
    the server refuses anyway) get discarded on the next reconnect.

  So the trade-off is:
  - ✅ Ship the cascade  → immediate "Disconnected" pill (feels
    responsive), at the cost of losing up to 60s of authorized
    pre-revoke work sitting in the debouncer.
  - ✅ Skip the cascade   → one-keystroke lag before the editor
    visibly drops to read-only, zero authorized-work loss.

  Skipping wins. If a future need makes the visual feedback more
  important than the data-loss window (e.g. an admin-kick action
  that MUST be visibly enforced within milliseconds), extend the
  eviction path to flush-before-shutdown FIRST, then wire the
  cascade. Without that flush, cascading folder revokes is a
  regression.

  Note that `resource_deleted` (S24) and `external_write` (S28)
  eviction paths are correct AS-IS without flush-before-shutdown:
  in both cases discarding pending dirt is the right behaviour
  (the file is going away, or the CRDT is already stale relative
  to the fresh external blob).
- ⬜ **`group_membership_lost`** — same wire path as
  `grant_revoked`, different producer. When a user is removed from
  a group that has a file grant, the group-membership service
  needs to publish `AuthzChanged { affected_files }` on
  `user:{removed_user}:authz`. Not wired yet; the plumbing
  through the WS handler is now in place.
- ✅ **`external_write`** — `FileLifecycleHook::on_file_updated`
  gains a `source: WriteSource` discriminator. External writers
  (REST upload replace, WebDAV PUT, WOPI PutFile, chunked-upload
  finalize — all routed through `FileUploadService`) pass
  `WriteSource::External`; the collab actor's own flush passes
  `WriteSource::CollabFlush`. A new `CollabEvictLifecycleHook`
  registered on the file-lifecycle fan-out fires
  `evict_sessions_for_file(file_id, "external_write")` on the
  External branch and short-circuits on `CollabFlush` so keystroke-
  driven flushes don't tear down their own sessions. Guarded by
  hurl S28 (upload → subscribe collab → external replace → revoked
  with reason external_write).

### Edge cases (design decisions, not TODOs)

- **File deleted while editing:** delete path publishes
  `AuthzChanged { affected: [file_id] }` → WS handler evicts everyone
  from `collab:{id}` with `revoked` reason `resource_deleted`.
  Session actor cleans up. Audited.
- **File renamed / moved:** file_id stays; UI updates via
  `file:{id}` metadata topic. No editor action needed.
- **Blob overwritten out-of-band** (WebDAV PUT while a collab
  session is active): flip a "conflict" flag on the session, drop all
  clients with `revoked` reason `external_write`, discard CRDT state.
  On next open, seed fresh from the blob. Simple, honest. This is
  the pragmatic call vs WOPI-style locking; document it in the
  end-user guide.
- **File larger than `max_doc_bytes`** (default 1 MiB): refuse
  `attach` with `CollabErr::TooLarge`; editor shows "Open in
  read-only viewer instead". Prevents pathological "collab-edit a
  400 MB log file".
- **Malformed CRDT bytes from a client:** WS handler catches `yrs`
  decode errors, closes socket with 1002 protocol error, audit
  `collab.protocol_violation`.

## Wire protocol — collab binary frames

The WS carries two coexisting wire formats:

- **JSON-RPC 2.0** (text frames) for the control plane — `rt.subscribe` /
  `rt.unsubscribe` / `rt.ping` requests and `rt.event` / `rt.revoked`
  notifications. Full spec in the arch doc's
  [§ Wire protocol](../architecture/message-bus-and-notifications.md#wire-protocol).
- **Yjs sync protocol** (binary frames) for CRDT ops — described here.

The WS handler classifies incoming frames by `MessageType` — text →
JSON-RPC, binary → Yjs sync protocol routed to
`CollabSessionService`. The two formats never interleave inside a
single message.

**Binary frame layout:** `[1 byte kind][16 bytes file_id][payload]`

- `0x01` c→s: local Yjs update
- `0x01` s→c: remote Yjs update (any other client's update)
- `0x02` c→s: local awareness update (cursor/selection)
- `0x02` s→c: someone else's awareness
- `0x03` c→s: sync-step-1 (state vector) — sent on attach
- `0x03` s→c: sync-step-2 (diff to catch client up)

Five frame kinds cover the whole Yjs sync protocol. `file_id` in the
frame header is what routes each binary payload to the correct
`CollabSession` actor — the JSON-RPC subscribe on `collab:{file_id}`
established the session's interest, but binary frames identify their
target inline so the WS handler doesn't have to correlate them to a
prior subscribe.

## Frontend

### Editor component (`lib/components/CollabEditor.svelte`)

- Props: `fileId`, `initialContent` (loading placeholder / offline
  fallback), `readOnly`, `filename` (`.md` vs `.txt` extension
  detection).
- Mounts CodeMirror 6 with:
  - `basicSetup`.
  - `markdown()` when `.md`; nothing for `.txt`.
  - Theme wired to `<html data-color-scheme>` per project convention.
  - `y-codemirror.next`'s `yCollab(ytext, awareness)` extension → the
    remote cursors + selection decorations, colored per user.
  - Custom `WsProvider` (not `y-websocket`) that plugs into the
    Plan-1 message-bus store.

### Custom Yjs provider (`lib/collab/wsProvider.ts`)

- Wraps `useTopic` for `collab:{fileId}` +
  `collab:{fileId}:awareness`.
- On mount: send sync-step-1 with local state vector; server responds
  with sync-step-2.
- On every `doc.on('update', ...)`: send binary frame kind `0x01`.
- On every `awareness.on('update', ...)`: send binary frame kind
  `0x02`.
- On incoming `0x01`: `Y.applyUpdate(doc, bytes)`.
- On incoming `0x02`: `awarenessProtocol.applyAwarenessUpdate(
  awareness, bytes, this)`.
- On disconnect: mark local `Doc` as detached; local edits still
  work (Yjs stays consistent); on reconnect, sync-step-1 again — no
  data loss.

### Preview pane (for `.md`)

- Reactive `$derived` on `ytext.toString()`, run through
  `unified` / `remark-parse` / `remark-rehype` / `rehype-stringify`
  client-side.
- Split-pane, resizable, remembered per-user preference.
- Not part of the CRDT; each user's preview is their own.

### Presence UI

- Avatars in the editor header, colored ring matches the cursor color.
- Hover an avatar → highlight that user's cursor in the doc.
- Click an avatar → scroll to their cursor.
- All driven by Yjs awareness — no extra state to manage.

### Where the editor opens from

- File tree row for `.md` / `.txt`: primary action becomes "Edit" (or
  an "Edit" secondary button next to "Download").
- Route: `/editor/[[fileId]]` (or a modal — decide separately in the
  FE issue).
- Uses the caller's Drive AuthZ role to decide read-only vs edit UI
  affordances.

## Folder-live updates (Phase A also ships this)

The collab editor lives inside a folder. If somebody else uploads a
file to that folder while you're editing, you should see it appear.
The bus + `folder:{id}` topic makes this a small amount of code on
both sides.

### Backend

Every service method that mutates a folder's children publishes,
**after commit**, a matching `MessageBusEvent` on `Topic::Folder(id)`:

| Service method | Event |
|---|---|
| `FileManagementService::create_file` | `FileCreated { file_id, name, parent_id, actor }` |
| `FileManagementService::delete_file` (soft or hard) | `FileDeleted { file_id, parent_id, actor }` |
| `FileManagementService::rename_file` | `FileRenamed { file_id, old_name, new_name, actor }` |
| `FileManagementService::move_file` | `FileMoved { file_id, from, to, actor }` — published on BOTH `Folder(from)` and `Folder(to)` |
| `FolderService::create_folder` | `FolderCreated { folder_id, parent_id, actor }` |
| `FolderService::delete_folder` | `FolderDeleted { folder_id, parent_id, actor }` |
| `FolderService::rename_folder` | `FolderRenamed { folder_id, old_name, new_name, actor }` |
| `TrashService::restore` | equivalent create-in-target event |
| `ShareService::grant` on a folder | `ShareGranted { file_id: folder_id, principal: PrincipalRef, role, affected_users }` published on `Folder(id)` so the share dialog updates. `principal` carries any `Subject` variant (`User` / `Group` / `Token`); `affected_users` is populated by the producer only for the group case. |
| `ShareService::revoke` on a folder | `ShareRevoked { file_id: folder_id, principal: PrincipalRef, affected_users }` — same shape as grant |
| `GroupService::add_member` | `GroupMemberAdded { group_id, user_id, actor }` on any topics that need it; the notification cascade is handled by the ingester (see below), not by the folder subscription |
| `GroupService::remove_member` | `GroupMemberRemoved { group_id, user_id, actor }` |

Rule (from `docs/architecture/message-bus-and-notifications.md`): publish **after commit only**.
Pattern: services return `(result, Vec<MessageBusEvent>)` from the tx
boundary; the calling layer fires `bus.publish` after commit.

### Frontend

Folder view (`routes/files/[...path]/+page.svelte`):

```ts
// `useTopic` hands the component the JSON-RPC `rt.event` notification
// params — with `topic`, `event`, `data`, `actor`, `ts` fields. The
// JSON-RPC envelope (jsonrpc/method) is stripped by the singleton
// store. Event discriminators are snake_case per the wire spec.
useTopic(`folder:${folderId}`, (evt) => {
  switch (evt.event) {
    case 'file_created':  files = [...files, /* fetch or synthesize */]; break;
    case 'file_deleted':  files = files.filter(f => f.id !== evt.data.file_id); break;
    case 'file_renamed':  files = files.map(f => f.id === evt.data.file_id
                            ? { ...f, name: evt.data.new_name } : f); break;
    case 'file_moved':    if (evt.data.from === folderId)
                            files = files.filter(f => f.id !== evt.data.file_id);
                          if (evt.data.to === folderId)
                            files = [...files, /* fetch */]; break;
    // ... folder events analogous
  }
});
```

- Payloads are thin facts — for anything past `name`/`id` the
  component fetches through the existing `lib/api/endpoints/files.ts`
  APIs, guaranteeing the FE never sees a field it couldn't already
  read.
- Optimistic-update reconciliation: if the current tab performed the
  action, ignore the echo (`evt.actor.user_id === session.user_id`)
  since local state already reflects it. Simple actor-id check.
- Event discriminator strings are the snake_case tag from the Rust
  `#[serde(tag = "event", rename_all = "snake_case")]` derive on
  `MessageBusEvent`. The AsyncAPI spec generated by
  `cargo run --bin generate-asyncapi` (see the arch doc's
  [§ Schema ownership](../architecture/message-bus-and-notifications.md#schema-ownership--asyncapi-vs-openapi))
  enumerates all valid values — Modelina already generates the typed
  TS shapes under `frontend/src/lib/generated/message-bus/`.

Same pattern lands on the trash view (`user:{u}:trash` topic) and on
the share dialog (`file:{id}:shares` topic — planned but low effort;
optional for Phase A).

## Notifications table + bell (Phase A also ships this)

### `NotificationService` (application service)

```rust
pub struct NotificationService {
    repo: Arc<dyn NotificationRepository>,
    bus:  Arc<dyn MessageBus>,
    mail: Option<Arc<dyn MagicLinkMailer>>, // reuse existing mail path
}

impl NotificationService {
    /// Create a persistent notification and fan it out live if the
    /// recipient has an active WS. Called from other services after
    /// their own tx commits.
    pub async fn create(&self, notif: NewNotification) -> Result<NotifId, NotifErr>;

    pub async fn list(&self, caller: UserId, opts: ListOpts) -> Result<Vec<NotifDto>, _>;
    pub async fn mark_read(&self, caller: UserId, id: NotifId) -> Result<(), _>;
    pub async fn mark_all_read(&self, caller: UserId) -> Result<(), _>;
    pub async fn unread_count(&self, caller: UserId) -> Result<u64, _>;
}
```

- `create` inserts a `notif.notifications` row **and** publishes
  `MessageBusEvent::Notification` to `user:{recipient_id}:notifications`.
- If the recipient has no WS attached AND the notification kind is
  configured as "email-if-offline", route through the existing
  `MagicLinkMailer`-style templating.

### Ingesters (call sites — one per initial `kind`)

Each of these is a small hook from an existing service, after commit:

- `ShareService::grant` — branches on principal type:
  - `User(u)`: one `share.granted` notification to `u`.
  - `Group(g)`: expand members via `GroupService::expand_transitive(g)`
    at the tx boundary (snapshot — do NOT re-expand at publish time or
    concurrent membership edits leak/duplicate deliveries); one
    `share.granted_via_group_membership` notification per member with
    `data.via_group = g`; one `AuthzChanged` on
    `user:{member}:authz` per member so any open WS re-evaluates its
    resource-scoped subs. If the expanded set exceeds
    `max_notification_fanout` (default 1000), drop the per-user
    notifications and audit `event = "notification.fanout_truncated"`;
    the `file:{id}:shares` bus event still fires so the share dialog
    stays accurate.
  - `Token(_)` (public share link): one `share.link_created`
    notification to the link creator (no other recipient to notify);
    no `AuthzChanged` fan-out (token holders have no WS sessions).
- `ShareService::revoke` — mirror of grant:
  - `User(u)`: `share.revoked` to `u`.
  - `Group(g)`: `share.revoked_via_group_membership` per (snapshotted)
    member; `AuthzChanged` per member; same fanout cap and coalescing.
  - `Token(_)`: `share.link_revoked` to the link creator.
- `GroupService::add_member(group=g, user=u)` — the "sideways" cascade:
  enumerate all grants on `g` and for each resource `R`, create one
  `share.granted_via_group_membership` notification for `u`
  (`data.via_group = g`); publish `AuthzChanged` on
  `user:{u}:authz` with the resource set so any open WS re-subs.
  Coalescing in `NotificationService::create` collapses duplicates
  when `u` was already reached via another group grant on `R`.
- `GroupService::remove_member(group=g, user=u)` — mirror: enumerate
  grants on `g`, for each `R` create one
  `share.revoked_via_group_membership` for `u` **unless** `u` still has
  access to `R` via a different grant (checked by the ingester before
  emitting — `AuthorizationEngine::resolve(u, R)`); one
  `AuthzChanged` for the true-loss set so subs evict.
- `JobRegistry` terminal transitions → `job.completed_for_you` /
  `job.failed_for_you` when the job's originator is set.
- `AuthApplicationService::login_success` when the client fingerprint
  is new for that user → `session.new_device_login`.
- `StorageUsageService` threshold crossings (80% / 95%) →
  `quota.threshold_reached`.
- `MagicLinkService::redeem` when the redeeming device fingerprint
  differs from any known session for that user →
  `magic_link.used_from_other_device`.

### REST surface

- `GET /api/notifications?unread_only=&limit=&cursor=` — list.
- `GET /api/notifications/unread_count` — number for the badge (may
  be replaced by a bus event later; polling on tab focus is fine).
- `POST /api/notifications/{id}/read` — mark one.
- `POST /api/notifications/read_all` — mark all.

Handlers stay thin: authenticate → call `NotificationService` →
serialize. AuthZ = the recipient is the caller; no cross-user reads.

### Frontend — bell + toast

- **`lib/stores/notifications.svelte.ts`** — singleton: on session
  start, fetches unread + subscribes (auto, no `useTopic` call — the
  singleton wires it once).
- **`lib/components/NotificationBell.svelte`** — icon + unread badge
  in the app shell; click opens a slide-out panel listing recent
  notifications (grouped by day). Actions: mark-read, mark-all-read,
  click-to-navigate (subject_type + subject_id decides the route).
- **`lib/components/NotificationToast.svelte`** — transient toast on
  incoming events, dismissable, respects a "quiet mode" per-user
  preference.
- **i18n** — every `kind` gets a locale-keyed template with
  interpolation for `actor.display_name`, `subject.name`, etc.

### Delivery semantics

| Recipient state | What happens |
|---|---|
| Online (WS attached) | Row inserted + live event → toast + bell increments |
| Online, no WS (very brief window during reconnect) | Row inserted; on WS reconnect, `unread_count` refetch surfaces it |
| Offline | Row inserted; on next login, `unread_count` shows it; kinds configured for `email_if_offline` also fire an email |

The bus is only the live path. The `notifications` table is the
source of truth. If the bus loses a message (slow subscriber,
reconnect gap), the row is still there.

## Limits & guardrails

| Limit | Default | Rationale |
|---|---|---|
| `max_doc_bytes` | 1 MiB | Editor UX degrades past this; textareas cope worse |
| `max_sessions` | 500 concurrent open docs / instance | Memory budget |
| `max_attached_sockets_per_doc` | 32 | Sane presence UX; more is symbol not signal |
| `flush_debounce_idle_ms` | 15_000 | Balance persist vs write amplification |
| `flush_debounce_max_ms` | 60_000 | Bounds worst-case data-at-risk on crash |
| `snapshot_after_updates` | 200 | Keeps hydrate/reconnect times bounded |
| `idle_ttl_seconds` | 1_800 | Drop sessions nobody's touched in 30 min |
| `notifications_per_recipient` | 10_000 | Background trim to keep table lean (oldest read first) |

Env vars: `OXICLOUD_COLLAB_MAX_DOC_BYTES`,
`OXICLOUD_COLLAB_MAX_SESSIONS`, etc. — full names, never
abbreviated, per project convention.

## Phasing

- **C1 — Backend skeleton.** `collab.doc_sessions` migration,
  `CollabSessionService` with load/apply/flush, in-memory actor
  registry, unit tests over `yrs`. ~4 days.
- **C2 — WS integration.** Binary frame routing in the bus WS
  handler, sync protocol, AuthZ gates, protocol audit lines. ~2 days.
- **C3 — Folder-live updates.** Publish sites in
  `FileManagementService` / `FolderService` / `TrashService`; FE
  `folder:{id}` subscription in the folder view; end-to-end test with
  two headless clients. ~3 days.
- **C4 — Notifications table + bell.** Migration, service, REST,
  6 ingesters, bell UI, toast, i18n. ~1 week.
- **C5 — Editor frontend.** CodeMirror 6 + collab extensions +
  custom provider + basic route. `.md` split-preview. ~5 days.
- **C6 — Presence polish (collab-scoped).** Avatar rail, cursor
  colors, hover-to-highlight, follow-cursor. ~2 days.
- **C7 — Robustness pass.** Out-of-band write eviction, oversized
  file refusal, idle GC, malformed-frame handling, audit lines,
  integration test with 5+ headless clients. ~4 days.
- **Later — Version history.** `collab.doc_snapshots`, "Name this
  version", restore. ~3 days.

## Risks & call-outs

- **Out-of-band writes are a real design constraint.** WebDAV / CLI
  users writing the same `.md` while a collab session is open will
  see the session evicted and their write win. Eviction rule is
  honest; WOPI-style locking is the alternative but heavy. Document
  in the end-user guide (see `docs/guide/`).
- **`.md` line-ending / BOM / trailing-newline normalization.** Diff
  churn if we don't normalize on both seed and flush. Pin one
  convention (LF, no BOM, single trailing newline) and enforce it in
  the flush path.
- **`y-codemirror.next` version drift.** Pin exactly; the Yjs / CM6
  combo has had subtle protocol nits. Vendor if churn continues.
- **Awareness storms with many viewers.** Yjs throttles at ~50 ms
  but a busy doc with 30+ presences is still a lot of frames.
  Fine at v1; if it becomes a load driver, throttle at the WS
  handler.
- **Notification kinds are a stable enum.** New denial cause = new
  kind, never repurpose an existing one, per project convention.

## Migration path if we add ProseMirror WYSIWYG later

- `yrs` doc structure switches from `Y.Text` to `Y.XmlFragment` —
  both supported cleanly.
- Backend service doesn't care what's in the doc — it persists +
  relays opaque updates.
- Only the editor component changes; the wire, service, table, and
  bus topics stay identical.
