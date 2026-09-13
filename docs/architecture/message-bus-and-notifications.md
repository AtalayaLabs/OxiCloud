# Message Bus & Persistent Notifications

OxiCloud has two coupled subsystems that together power its real-time
UX — a **live message bus** over WebSocket for "something just
happened, refresh your view", and a **persistent notifications
table** for "you need to know about this even if you weren't
online." This document explains how both work, how they authenticate
and authorize subscribers, and why the frontend deliberately drops
the WebSocket while a tab is hidden.

Design docs the shipped code implements: [`docs/plan/message-bus.md`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/message-bus.md)
+ [`docs/plan/templated-messages.md`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/templated-messages.md).

---

## The two channels

| | **Message bus (WebSocket)** | **Persistent notifications (REST + DB)** |
|---|---|---|
| Purpose | "Something changed, refresh your view" | "You need to know about this — later is fine" |
| Transport | JSON-RPC 2.0 over `/api/rt/ws` | `GET/POST/DELETE /api/notifications/*` + `notif.notifications` table |
| Delivery | Best-effort, in-memory, no replay | Durable, per-user rows, survive reboot / offline |
| Payload | Thin "poke" facts (id + verb) | Full per-kind DTO with all render data |
| Loss on disconnect | Yes (events during outage window are dropped) | No (rows are the source of truth) |
| Schema owner | AsyncAPI (`resources/gen/asyncapi.json`) | OpenAPI (`resources/gen/openapi.json`) |

The two work together: an ingester that wants to notify a user
writes **both** — the DB row (for durability + the bell's history)
AND publishes a bus event on `user:{u}:notifications` (so online
sessions refetch instantly instead of waiting for the next
mount). The wire event on that topic is a **pure poke** — empty
`data: {}`. The row's real content only ever crosses the REST wire.

---

## Schema ownership — AsyncAPI vs OpenAPI

The bus and the REST endpoints have separate wire specs. The rule
the codebase adopts to keep them from drifting:

> **AsyncAPI defines the envelope + transport for clients.
> OpenAPI defines the payload.**

Concretely:

| Type | Home | How it stays in sync with Rust |
|---|---|---|
| **Bus events** (`MessageBusEvent`, subscribe / unsubscribe frames, revoked notifications, envelope shape) | **AsyncAPI** — `resources/gen/asyncapi.json` | Hand-written in `src/bin/generate-asyncapi.rs` via `json!` macros; kept in lockstep with `MessageBusEvent`'s serde shape. Small drift risk — Rust is truth. |
| **REST DTOs** (response bodies, request bodies, per-kind notification payloads) | **OpenAPI** — `resources/gen/openapi.json` | `#[derive(utoipa::ToSchema)]` on the Rust struct. Utoipa walks `#[utoipa::path(...)]` handlers + registered schemas. **No drift possible** — projection is derived from Rust. |
| **Types on both wires** (rare; none today) | Would live as one Rust struct with both derives, or wait for single-source codegen | — |

### Why this split, not one unified spec

The instinct is to put the notification payload schema in AsyncAPI
alongside the bus event that triggers a refetch. It looks cleaner
until you realize the payload **never travels on the bus wire** —
the bus event is `NotificationReceived` with empty `data: {}`, a
pure cache-invalidation poke. The FE fetches the payload from
`GET /api/notifications`, which is REST → OpenAPI's territory.
Putting the payload schema in AsyncAPI would mean "documenting
this shape on a transport it doesn't travel on" — a conceptual
stretch that adds a drift risk for zero gain.

### Rejected alternatives

- **Dual-spec (same type declared in both AsyncAPI + OpenAPI).**
  Guaranteed drift unless both come from a single codegen. Nothing
  in the tooling today produces both, so we'd hand-maintain two
  copies of every shared type. Bug factory.
- **Cross-spec `$ref`** — AsyncAPI 3.0 allows `"$ref":
  "openapi.json#/components/schemas/Foo"`, and utoipa's
  `components(schemas(...))` can publish orphan types (no
  `#[utoipa::path]` reference) so OpenAPI advertises "internal"
  schemas. Technically workable but: Modelina + Swagger UI + Redoc
  handle external refs inconsistently, OpenAPI stops being "the
  REST contract" and becomes "a general schema registry",
  reviewers get confused. Legal, fragile, avoided.
- **Bus event carries the full payload** (revert the pure-poke
  design). Would put per-kind payload schemas in AsyncAPI as
  `MessageBusEvent::NotificationReceived { granter_id, resource_id,
  … }`. Rejected because the FE has to REST-fetch anyway (bell
  reads from DB for history + persistence), so the fields on the
  wire are dead weight — same-content overlap between the two
  specs, no consumer benefit.
- **Session-resume tokens** (`rt.subscribe { since: N }` +
  server-side ring buffer). Would let the bus deliver missed rows
  directly on reconnect, saving one REST round-trip. Rejected for
  **backward compatibility with `OXICLOUD_MESSAGEBUS_ENABLE=false`**:
  ops who disable the WS rely on the bell falling back to REST;
  bus-only replay would leave those deployments with no catch-up
  path. The REST `?after=` cursor works in every mode (bus on, bus
  off, network gap); the bus stays purely "instant-poke".

### What this looks like in the tree

- `src/application/ports/message_bus_ports.rs` — `MessageBusEvent`
  enum (Rust source of truth for bus wire shapes).
- `src/bin/generate-asyncapi.rs` — projects those Rust variants
  into `resources/gen/asyncapi.json`.
- `src/domain/entities/notification.rs` — `SharegrantedPayload`
  and its siblings, `#[derive(ToSchema)]`, source of truth for
  REST payload shapes.
- `src/interfaces/api/mod.rs` — utoipa `#[openapi(components(schemas(SharegrantedPayload, ...)))]`
  registers the payload in OpenAPI even though `NotificationDto.payload`
  stays `serde_json::Value` on the response type. (FE type-narrows
  on `row.kind` and casts to the right shape.)
- Nothing lives in both specs today.

### Adding a new bus event

1. Add a variant to `MessageBusEvent`.
2. Add the variant to `generate-asyncapi.rs`'s `event_kind` enum
   and (if the variant has payload fields) a schema function.
3. Regenerate AsyncAPI + FE DTOs via `just asyncapi` +
   `npm run gen:message-bus`.
4. **Do not** add the variant to OpenAPI. Bus events don't
   travel on REST.

### Adding a new notification kind's payload

1. Add a Rust struct in `domain/entities/notification.rs` with
   `#[derive(Serialize, Deserialize, ToSchema)]`.
2. Register it in `src/interfaces/api/mod.rs`'s
   `components(schemas(...))` list.
3. Regenerate OpenAPI via `just openapi`.
4. **Do not** add the struct to AsyncAPI. Notification payloads
   only cross the REST wire.

---

## Message bus

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ SERVICE LAYER                                               │
│                                                             │
│  ShareService.grant()  ── after commit ──▶ bus.publish(…)   │
│  FileMgmtService.…()   ── after commit ──▶ bus.publish(…)   │
│  NotificationService   ── after commit ──▶ bus.publish(…)   │
│  Scheduler engine      ── on run start/end ──▶ bus.publish  │
└──────────────────────────┬──────────────────────────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────┐
│ MessageBus port  (application/ports/message_bus_ports.rs)   │
│                                                             │
│   InProcessMessageBus                                       │
│     DashMap<Topic, tokio::broadcast::Sender<Event>>         │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           │  (optional replicator seam)
                           ▼
                    NoopReplicator (v1)
                    PgListenReplicator (deferred)
                    BrokerReplicator (RabbitMQ / NATS, deferred)
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│ WS handler   (interfaces/api/handlers/rt_ws.rs)             │
│                                                             │
│   One session per socket:                                   │
│     HashMap<wire_key, Sub>  +  outbound mpsc                │
│                                                             │
│   rt.subscribe / rt.unsubscribe frames                      │
│   rt.event / rt.revoked / rt.pong notifications             │
└─────────────────────────────────────────────────────────────┘
```

### Topics — a typed enum, not a string

```rust
pub enum Topic {
    Folder(Uuid),              // "folder:{uuid}"
    UserAuthz(Uuid),           // "user:{uuid}:authz"
    UserNotifications(Uuid),   // "user:{uuid}:notifications"
    Job(String),               // "job:{name}"
}
```

Defined in `application/ports/message_bus_ports.rs`. Encoded to a
stable dotted wire form; parsed back with strict validation. The
wire form doubles as a routing key for future broker replicators
(RabbitMQ topic exchanges, NATS subjects).

### Events

`MessageBusEvent` (same module) is the discriminated union of every
payload a publisher can produce — `FileCreated`, `FolderMoved`,
`AuthzChanged`, `JobRunStarted / Progress / Ended`,
`NotificationReceived`, etc. Serde tags with `#[serde(tag = "event",
rename_all = "snake_case")]`, so the wire is
`{"event": "file_created", "file_id": "...", "actor": "..."}`.

Payloads are **thin facts**: the ID of the changed resource + the
actor + the verb. Clients refetch details via REST if they need
them. Keeps the AuthZ surface small (thin payloads can't leak
fields the caller couldn't already read via REST) and keeps events
well under any future broker's message-size cap.

### Wire protocol

JSON-RPC 2.0 over text frames. Full protocol in [`docs/plan/message-bus.md § Wire protocol`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/message-bus.md).

```jsonc
// Client → server
{"jsonrpc":"2.0","id":1,"method":"rt.subscribe","params":{"topic":"folder:abc-…"}}

// Server → client (ack)
{"jsonrpc":"2.0","id":1,"result":{"subscribed":"folder:abc-…"}}

// Server → client (push, id-less notification)
{"jsonrpc":"2.0","method":"rt.event","params":{
  "topic":"folder:abc-…",
  "event":"file_created",
  "data":{"file_id":"…","name":"…","parent_id":"…","actor":"…"}
}}
```

### Authentication for the WebSocket upgrade

Two paths, both accepted by the same handler:

| Client kind | Path | Why |
|---|---|---|
| **Programmatic** (CLI, test helper) | `Authorization: Bearer <jwt>` on the upgrade | The `new WebSocket()` API in browsers can attach `Sec-WebSocket-Protocol` but NOT arbitrary headers, so browsers can't do this. |
| **Browser** | `POST /api/rt/ticket` (with the full DPoP + CSRF middleware chain) mints a one-shot 30-second opaque UUID; the browser opens the WS with `Sec-WebSocket-Protocol: oxi.ticket.<uuid>` | DPoP-bound sessions cannot attach a `DPoP:` header to `new WebSocket()`. The ticket flow moves the DPoP check to a normal POST that DOES support headers, and the WS upgrade just redeems the opaque token. |

Tickets are single-use, TTL 30 s, stored in a `RtTicketStore`
(in-memory). Redemption removes the entry — replay is impossible.

The WS route is deliberately mounted **outside** the
`protected_api` middleware stack — otherwise the DPoP-required
layer would 401 every browser on upgrade before the ticket flow
could kick in.

---

## The three AuthZ scopes

`Topic::required_perm(&self) -> AuthzCheck` dispatches every
subscribe attempt into exactly one of three classes. This is the
authoritative diagram of what the WS handler enforces:

```
                   ┌──────────────────────────────────────────┐
                   │       Topic::required_perm()             │
                   └─────┬─────────────┬─────────────┬────────┘
                         │             │             │
             ResourceRead │ IdentityMatch │  RoleAdmin
                         ▼             ▼             ▼
              ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
              │ Class 1     │ │ Class 2     │ │ Class 3     │
              │ per-resource│ │ per-user    │ │ per-session │
              │             │ │             │ │  (role)     │
              └─────────────┘ └─────────────┘ └─────────────┘
```

### Class 1 — Per-resource (`AuthzCheck::ResourceRead`)

**Topics:** `folder:{id}`, and (future) `file:{id}`,
`drive:{id}`, `calendar:{id}`, `addressbook:{id}`.

**Rule:** the caller must hold **`Read`** on the resource via the
same `AuthorizationEngine` that guards every REST endpoint. Owner
short-circuits pass; direct grants pass; group-mediated grants
pass; drive-membership cascades pass. Everything else is denied.

**Wire response on denial:** JSON-RPC error object with
`code = -32001`, `message = "no_read"`. Same shape whether the
resource doesn't exist OR the caller lacks the grant — **anti-
enumeration invariant**. Audit reason (`no_read` /
`no_such_resource`) distinguishes internally.

**On grant revocation:** the WS handler auto-subscribes each
session to `user:{caller}:authz` (Class 2 below). When a
`MessageBusEvent::AuthzChanged { affected_folders }` fires, the
session's reader translates it to an internal `EvictFolders`
signal → the main loop walks the sub set and drops any Class-1
subscription whose resource was affected, emitting a client-visible
`rt.revoked` notification per evicted topic. Same pattern applies
to any Class-1 topic when the AuthZ model widens beyond folders.

### Class 2 — Per-user, strict privacy (`AuthzCheck::IdentityMatch`)

**Topics:** `user:{u}:authz`, `user:{u}:notifications`.

**Rule:** direct UUID equality — `caller_id == u`. **No admin
bypass, no group indirection, no owner short-circuit.** Admins
cannot subscribe to other users' `:authz` or `:notifications`
streams; that's a privacy invariant, not a mere policy choice.

**Wire response on mismatch:** `topic_forbidden` — the **same
wire shape as an unknown topic**. An attacker probing
`user:{someone_else_uuid}:authz` cannot distinguish "user exists
but not me" from "no such user."

**Auto-subscription:** the WS handler auto-subscribes every session
to its own `user:{caller}:authz` AND `user:{caller}:notifications`
at session open. No `rt.subscribe` frame is needed from the client
for these — they're always active for the caller's own UUID.

### Class 3 — Per-session role (`AuthzCheck::RoleAdmin`)

**Topics:** `job:{name}` today. Future `admin:*` topics land here.

**Rule:** the session's snapshotted role at open time must be
`admin`. The handler resolves `caller_role` once during session
setup via `resolve_live_role` and stores it on the session state —
no per-subscribe DB round-trip.

**Wire response on non-admin:** `topic_forbidden` — same anti-enum
shape as Class 2. A non-admin probing job topics cannot enumerate
which jobs are registered.

**Why snapshot at session open, not per subscribe:** admin role
loss is rare + trivially recoverable (the user closes the tab and
reopens, hitting the fresh role check). Per-subscribe checks would
be an extra DB round-trip on every frame with no meaningful
security gain — the WS session itself was authenticated at upgrade
time under the current role.

### Adding a new topic

Every new topic variant must decide which class it belongs to at
`Topic::required_perm`. The compiler enforces exhaustiveness — a
new variant with no branch fails to build, which is deliberate. New
topics get audited before shipping precisely because the
`required_perm` match forces the author to state the class
explicitly.

---

## Tab-visibility grace-close — reducing idle connections

Every open browser tab holds one WebSocket to the server. A user
with five tabs open holds five sockets. A user who leaves a tab
open all day but only uses one holds five sockets, four of them
serving nothing.

The frontend closes the WebSocket **after 60 seconds of tab
hidden** and reopens it when the tab becomes visible again. The
subscription state is preserved locally through the outage — every
subscriber's release handle stays live, the reactive store still
holds the last-known list — but the wire is silent while the tab
is hidden.

### Implementation

Frontend `MessageBusClient` (`frontend/src/lib/message-bus/client.svelte.ts`)
attaches a `visibilitychange` listener on construction:

- **Tab hidden** → starts a 60-second timer.
- **Timer fires while still hidden** → close the WS via a
  `#closeForHidden` path that sets state to `disconnected` but
  preserves `#subs` for later replay. The `#onClose` handler is
  guarded by `#tabIsHidden()` — an auto-reconnect won't fire while
  the tab remains hidden.
- **Tab visible again** → cancels the timer if it hadn't fired
  yet; if the WS was closed, kicks off a normal reconnect.
- **On reopen**, the WS handler auto-subscribes to `:authz` and
  `:notifications` again, and the client replays every entry in
  `#subs` as `rt.subscribe` frames. From the user's POV, the state
  is identical to what they left behind.

### What this trades

- **Saved**: N-1 idle sockets per user with N tabs open, over the
  hidden-tab window. Meaningful at scale (100 users × 5 tabs × 8
  idle hours = 4000 idle-tab-hours of connection state to keep
  alive per day).
- **Lost**: bus events published during the 60-second delay
  (transient) + the whole grace-close window (indefinite while
  hidden) are dropped for that session. **Recovery**: on reopen,
  every consumer that cares refetches. See "Reconnect catch-up"
  below.

### Why 60 seconds

Short enough that leaving a tab for a coffee break doesn't burn
the connection. Long enough that the momentary focus-shifts
users do all day (Cmd-Tab to another app, back within seconds)
don't churn the socket. Not tunable per-user — the value is
hard-coded in `HIDDEN_GRACE_MS`.

### Reconnect catch-up — the `?after=` cursor

For consumers whose state can't be reconstructed by refetching a
current listing (specifically: **notifications**, whose bell must
show rows that landed during the outage), the FE issues a delta
fetch:

- Store tracks `#lastReceivedAt` — the newest `created_at` seen
  before disconnect.
- On `messageBus.onReconnect(...)` fire, calls
  `GET /api/notifications?after=<lastReceivedAt>&limit=100`.
- Merges the returned rows into local state via `mergeById` —
  duplicates are resolved with **incoming wins** (server value
  overrides local, so a `read_at` flip on another device shows
  up correctly).

The server-side `?after=` predicate is **strict `>`** — a row at
exactly `lastReceivedAt` is excluded. This makes the WS push (which
delivers a row at time T) and the delta fetch (which asks for
"anything after T") non-overlapping by construction. `mergeById`
handles the case where the two paths race and both deliver the
same row.

For consumers whose state IS a current listing (folder view: the
files/subfolders in a folder), reconnect just refetches the listing
via the normal REST endpoint. `useReconnect` composable exposes
`onReconnect(cb)` as a one-liner for that pattern.

---

## Persistent notifications (the bell)

### Data model

```sql
notif.notifications (
    id           UUID  PRIMARY KEY,
    user_id      UUID  NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    kind         TEXT  NOT NULL,     -- 'share_granted' | 'new_login_from_new_device' | …
    payload      JSONB NOT NULL,     -- per-kind shape (see below)
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at      TIMESTAMPTZ         -- NULL = unread
)
```

Two indexes:

- `(user_id, created_at DESC) INCLUDE (read_at, kind)` covers the
  bell's list query + the mark-all-read filter.
- `(read_at) WHERE read_at IS NOT NULL` — partial, tiny on healthy
  DBs; feeds the retention job's DELETE.

### Ingesters

An ingester is a code path that calls
`NotificationApplicationService::create(NewNotification)`. The
service atomically:

1. `INSERT INTO notif.notifications RETURNING …` — durable row.
2. `bus.publish(Topic::UserNotifications(user_id), NotificationReceived)` — the fast-path poke.

Publish happens **after** the DB write succeeds, never inside a
transaction — a rolled-back INSERT would otherwise fan out a lie.

**Currently shipped ingester:** `share_granted` in
`interfaces/api/handlers/grant_handler.rs::create_grant`. Fires
after `authz.set_role(...)` succeeds. Fans out to every resolved
recipient user:

- `Subject::User(id)` → one row for that user.
- `Subject::Group(id)` → one row per transitive member (via
  `SubjectGroupService::list_transitive_users`).
- `Subject::Token(_)` → no row (anonymous share links have no
  target user).

Self-shares (owner grants themselves via a group they belong to)
skip. Failure to write is best-effort — a warn log; the grant row
stays durable in `role_grants`, the recipient can still discover
the share via `/api/grants/shared-with-me`.

**Planned but not wired** (each needs a prerequisite subsystem
listed in [`docs/plan/message-bus.md § Deferred`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/message-bus.md)):

| Kind | Prerequisite |
|---|---|
| `new_login_from_new_device` | Device-fingerprint tracking table |
| `job_completed_for_you` | Scheduler engine threading the trigger caller's `caller_id` through `dispatch()` |
| `storage_quota_threshold` | Per-user usage/quota comparator with threshold-crossing detection |

### Payload shape — typed per kind

The `payload` column is JSONB (schema-free at the DB layer). Each
kind's Rust shape lives in `domain/entities/notification.rs` with
`#[derive(Serialize, Deserialize, ToSchema)]`. OpenAPI picks up the
struct automatically. Adding a new field is additive on JSONB — no
migration.

Example — `share_granted`:

```rust
pub struct SharegrantedPayload {
    pub granter_id:         Uuid,
    pub resource_type:      String,   // 'folder' | 'file' | 'drive' | …
    pub resource_id:        Uuid,
    pub resource_name:      Option<String>,   // snapshot at grant time
    pub resource_path:      Option<String>,   // storage path snapshot
    pub navigate_folder_id: Option<Uuid>,     // FE routing target for drives
    pub role:               String,
    pub expires_at:         Option<DateTime<Utc>>,
}
```

The name/path fields are **snapshotted at grant time**. If the
folder is later renamed or moved, the notification still reflects
what it was called when the share happened. Same principle as
email invitations or activity feeds: the record is a fact about
what was true at the moment, not a live pointer.

### Schema-ownership rule — AsyncAPI vs OpenAPI

> **AsyncAPI defines the envelope + transport for clients.
> OpenAPI defines the payload.**

The bus event `MessageBusEvent::NotificationReceived` is a **unit
variant** — it serializes to `{"event":"notification_received",
"data":{}}` with no fields on the wire. The topic identifies the
semantic; the FE responds by refetching from REST.

The payload's shape lives in OpenAPI via `ToSchema` on
`SharegrantedPayload` (and its future siblings), auto-derived from
Rust. AsyncAPI never sees these types — payloads don't travel on
the bus wire.

**Why this split** — it eliminates schema drift between the two
specs. A payload edit changes Rust → OpenAPI updates on regen
(mechanical). AsyncAPI stays stable (hand-written, but it never
touches payloads). Same rule applies to any future bus consumer
that also has a REST DTO — Rust is the source of truth; each spec
projects the parts of Rust that travel on its transport.

Design rationale + rejected alternatives (dual-spec, cross-`$ref`,
per-kind Svelte components) in [`docs/plan/templated-messages.md § Schema ownership`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/templated-messages.md).

### REST surface — `/api/notifications`

| Method | Path | Purpose |
|---|---|---|
| `GET`    | `/api/notifications` | List newest-first. Query params: `unread` (bool), `before` / `after` (cursors), `limit` |
| `GET`    | `/api/notifications/unread` | Badge-only fast path (returns just `unread_count`) |
| `POST`   | `/api/notifications/{id}/read` | Mark one row read (idempotent, always 204) |
| `POST`   | `/api/notifications/read-all` | Bulk mark-all-read (returns rows updated) |
| `DELETE` | `/api/notifications/{id}` | Hard-delete one row (idempotent, always 204) |

**Anti-enumeration:** mark-read and delete always respond 204 —
whether the row existed and belonged to the caller, or didn't
exist at all, or belonged to someone else. Every mutating endpoint
scopes on `caller_id` at the SQL layer; the response shape is
identical across the three outcomes.

### Retention

The `notifications_cleanup` scheduled job (daily, same tier as
`trash_cleanup`) DELETEs read rows older than
`OXICLOUD_NOTIFICATIONS_RETENTION_DAYS` (default 30). **Unread
rows are preserved unconditionally** — a user offline for a month
still sees the share-granted notice when they log back in.

Runtime override via the job's `retention_days` parameter on the
admin panel's trigger — the env default seeds it, the panel
overrides at trigger time.

### Frontend rendering

`frontend/src/lib/composables/useNotifications.svelte.ts` owns the
module-scoped store — one instance per SPA session. Exposes:

- `notifications.items` — reactive list (newest first)
- `notifications.unread` — reactive badge count
- `notifications.refresh()` / `refreshDelta()` / `markRead(id)` /
  `markAllRead()` / `delete(id)`

`NotificationRow.svelte` handles the actual rendering — one file,
one `switch` on `row.kind`, one rich template per shipped kind
(`share_granted` today; the others fall back to a generic string).
Extraction into per-kind components is deferred until a single
kind's block exceeds ~30 lines or two kinds start needing the same
sub-component (see [`docs/plan/templated-messages.md § Rendering`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/templated-messages.md)).

### Notification click routing

| `resource_type` | Route | Data used |
|---|---|---|
| `folder` | `/files/{resource_id}` | `resource_id` |
| `file`   | `/shared-with-me?file={resource_id}` | `resource_id` — the `/files/{uuid}` route requires a **folder** id, and a file-scoped grant may not include parent-folder access. `/shared-with-me` is the guaranteed-accessible home for every recipient of a `share_granted`, and its `?file=` deep link opens the inline `FileViewer`. |
| `drive`  | `/files/{navigate_folder_id}` | Drives have no browsable URL of their own; `navigate_folder_id` is the drive's `root_folder_id`, enriched at ingest via `DriveRepository::get_by_id`. |
| `calendar` / `address_book` / `playlist` | no link (bold text) | Not addressable via `/files/*`. |

The bell also fires a **transient toast** (via the existing
`ui.notify(...)` mechanism) on every fresh row that arrives via
delta — the toast fades out after ~4 s while the persistent row
stays in the bell's history section. Same bell icon, same badge
count, no duplicate UX.

---

## Feature flags & config

| Env var | Default | Effect |
|---|---|---|
| `OXICLOUD_MESSAGEBUS_ENABLE` | `true` | Master switch. When `false`, the `/api/rt/ws` and `/api/rt/ticket` routes are **not registered** at boot (Axum returns 404), and the FE `useNotifications` composable skips the WS setup entirely. The bell falls back to REST-only mode — polling on mount, delta on manual refresh. Zero client-side error spam. |
| `OXICLOUD_MESSAGEBUS_KEEPALIVE_SECONDS` | `30` | Server-initiated RFC 6455 Ping interval on each WS connection. Prevents intermediate proxies (nginx, Traefik, Cloudflare) from reaping the TCP session as idle. |
| `OXICLOUD_NOTIFICATIONS_RETENTION_DAYS` | `30` | Retention window for read notifications. Unread rows are always preserved. The `notifications_cleanup` job clamps to a minimum of 1 day. |

Client discovers all of these via `GET /api/config` — no in-band
"does the server support the bus?" probe needed. The FE reads
`serverConfig.features.message_bus` at boot and skips WS setup
entirely when it's false.

---

## Failure modes

| Scenario | Behavior |
|---|---|
| Bus is disabled server-side (`OXICLOUD_MESSAGEBUS_ENABLE=false`) | `/api/rt/ws` returns 404. `useTopic` in the FE returns early. Bell works via REST only. |
| Network drops mid-session | Client-side jittered exponential backoff (250 ms → 30 s cap, 20-failure circuit breaker). On reconnect, WS handler re-auto-subscribes to `:authz` + `:notifications`; `useReconnect` composable fires `onReconnect` callbacks so views refetch. |
| Server restart | Same as network drop — the WS breaks, client backs off, reconnects when server is back. Events published during the outage are lost (no persistent event log by design); consumers refetch. |
| Tab hidden > 60 s | WS closed via `visibilitychange` grace-close. State preserved locally. On visibility return, reconnect + replay subscriptions. |
| Publish before commit | Not allowed. Every publish site is documented as "after commit". A publish inside a transaction that rolls back would fan out a lie. |
| Broker replicator failure (future) | The local `InProcessMessageBus` publishes still succeed — the replicator is beside the bus, not in front. Broker-hop failures affect multi-instance fanout but never local delivery. |

---

## Testing

The api-test suite exercises the full stack end-to-end via `rt-hurl-helper` (a small Rust binary gated on `test_utils`) — Hurl alone can't drive a WebSocket. Sixteen scenarios in `tests/api/rt_bus_check.sh`:

- Positive delivery, topic isolation
- AuthZ denial (Class 1 folder), unknown-topic anti-enum
- Server keepalive, delete emits, move fan-out
- Grant-revoke eviction (`AuthzChanged` → `rt.revoked`)
- Cross-user identity gate (Class 2)
- Ticket happy path + single-use replay refused
- Admin-only job topic (Class 3)
- `notification_received` wire push, DB row via `GET /api/notifications`
- Cross-user notifications identity gate (Class 2, notifications topic)
- `?after=` cursor with strict-`>` boundary invariant

`mergeById` — the FE's WS/reconnect race dedup — has its own
Vitest with 5 covered cases (empty, non-overlap, exact-dup,
stale-read-at overwrite, mixed overlap).

---

## Further reading

- Plan doc: [`docs/plan/message-bus.md`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/message-bus.md) — full design rationale, roadmap, and deferred slices (Yjs collab, broker replicator, SharedWorker, Web Push).
- Plan doc: [`docs/plan/templated-messages.md`](https://github.com/oxicloud/oxicloud/blob/main/docs/plan/templated-messages.md) — schema-ownership rule, rendering shape, notification routing decision.
- [ReBAC Authorization](/architecture/rebac-authorization) — the engine every Class-1 topic gate calls.
- [Background jobs](/architecture/jobs) — `notifications_cleanup` is one of them; `Topic::Job` publishes on job lifecycle.
