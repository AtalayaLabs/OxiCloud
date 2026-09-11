# Plan — Message bus over WebSocket

## Context

OxiCloud today has no server-push channel. Every "live-ish" surface
(folder listing, job dashboard, share dialog, admin session count) is
either stale-until-refresh or polled by the SPA. That leaves a whole
category of features unreachable — collab editing, presence,
notifications, sync-client push invalidation — and it makes existing
surfaces feel dated compared to Google Drive, Notion, Nextcloud, and
M365.

This plan introduces a single message bus over WebSocket that any
service can publish facts to and any client can subscribe to. Collab
editing is one consumer on top; folder-live updates, notifications,
job progress, presence, and sync-client push invalidation follow with
almost no extra scaffolding.

## Status — 2026-09-11

The `feat/message-bus` branch delivers **D + F + follow-ups shipped
end-to-end** on the FE and BE, verified by S1–S11 in the api-test
smoke suite plus manual multi-user E2E. Live today:

- **Bus core** — `MessageBus` port + `InProcessMessageBus` +
  `NoopReplicator`. `📤 bus publish` trace under
  `RUST_LOG=oxicloud::message_bus=debug`.
- **WS handler** (`/api/rt/ws`) — JSON-RPC 2.0, `rt.subscribe /
  unsubscribe / event / revoked / ping / pong`, server-initiated RFC
  6455 keepalive Ping.
- **Auth for the WS upgrade** — **F ticket flow shipped**.
  `POST /api/rt/ticket` mints a one-shot 30 s ticket under the full
  auth+DPoP+CSRF chain; the browser passes it via
  `Sec-WebSocket-Protocol: oxi.ticket.<uuid>`. Also accepts
  `Authorization: Bearer <jwt>` for programmatic clients
  (`rt-hurl-helper`). Route mounted OUTSIDE `protected_api` so the
  standard DPoP-required middleware doesn't 401 browsers that can't
  attach a `DPoP:` header to `new WebSocket()`. See
  `handlers/rt_ws.rs` module doc.
- **Events firing end-to-end** — every `MessageBusEvent` variant
  except CRDT-flavoured ones:
  - `FileCreated / Renamed / Moved / Deleted` (via
    `FileUploadService` + `FileManagementService`)
  - `FolderCreated / Renamed / Moved / Deleted` (via `FolderService`
    for direct paths; `TrashService` publishes `FolderDeleted` on the
    trash-first path — the FE hits that path via
    `DELETE /api/folders/{id}`)
  - `AuthzChanged` (via `ShareService::revoke_grant`) drives the
    grant-revocation eviction cascade.
- **Grant-revocation eviction (Slice C)** — WS handler
  auto-subscribes each session to `user:{caller}:authz`; on
  `AuthzChanged` the reader translates to `SessionOut::EvictFolders`
  and emits `rt.revoked` per evicted topic. Scope is per-topic; the
  session itself and unrelated subscriptions survive.
- **FE composables** — `useTopic` (generic), `useFolderTopic`
  (folder-view sugar with per-verb + `onRevoked` + `onReconnect`
  handlers), `useReconnect` (session-level, fires after 2nd+ open).
  Types generated from AsyncAPI via `@asyncapi/modelina` in
  `frontend/src/lib/generated/message-bus/`; `check-message-bus-spec`
  CI + `pre-pull-request` block on drift.
- **Folder-view live refresh** — `+page.svelte` wires every
  `onFile*`/`onFolder*` handler to `scheduleLiveReload`, 100 ms
  coalesce. Revocation → toast + `goto('/files')`. Reconnect →
  refetch via `onReconnect` (bridges the "events lost during outage
  window" gap; see `project_message_bus_reconnect_gap` memory).
  Actor-echo skip was REMOVED for multi-tab correctness — refetch is
  idempotent, ~30 ms per self-mutation.
- **Client-side resilience** — jittered exponential backoff
  (250 ms → 30 s cap), circuit breaker at 20 consecutive failures
  (~5 minutes of retry — covers a cargo-release restart), `untrack`
  in every mutation entry point so `$state` reads don't leak into
  caller `$effect` deps.
- **AuthZ tested** — S3 (folder no_read), S4 (nonexistent folder =
  anti-enum parity), S9 (cross-user identity topic → `topic_forbidden`).
- **Ticket tested** — S10 (happy path), S11 (single-use replay
  rejected).

Active — still under Phase A, ordered by priority:

- **Job dashboard live** (next) — `JobRegistry` publishes step
  progress + terminal state on `job:{id}`; the admin jobs view
  subscribes and drops its polling. Small; same shape as folder-live.
  Value: an operator who triggers a long-running job (backend
  migration, thumbnail import, etc.) can navigate to another admin
  page and come back without losing progress visibility.
- **Notifications table + bell** (E) — topic + producer + auto-sub
  land here. Same pattern as `:authz`. Larger; unblocks Phase-B
  `@mentions`.

Deferred — see the Roadmap section's `## Deferred` block and the
`project_message_bus_reconnect_gap` memory:

- **Workspace UX** (was Phase B) — presence, comments, reactions,
  `@mentions`, `NotificationService` as bus subscriber.
- **Infrastructure payoff** (was Phase C) — sync-client push
  invalidation, album live, slideshow sync.
- **Yjs collab** — `docs/plan/markdown-collab.md`, depends on the
  binary-frame routing this plan sketches but doesn't ship.
- **Broker replicator** (Postgres LISTEN/NOTIFY or Redis) — for
  multi-instance and durable event log. `BusReplicator` port
  declared, `NoopReplicator` wired today.
- **Session-resume tokens** — `rt.subscribe { since: N }` + a
  server-side per-topic ring buffer with sequence numbers.
  Replaces "full refetch on reconnect" with delta replay. Pairs
  with the collab editor slice (Yjs) where refetch cost is high.
- **SharedWorker for multi-tab dedup** — one WS per user per
  browser profile, shared across every same-origin tab. Turns "5
  tabs open" into 1 WS instead of 5. Ship when the "Live WS
  sessions" admin card sits persistently at N × user count.
- **Web Push for offline delivery** — pairs with Slice E
  (notifications bell). Delivers to closed browsers via FCM /
  Mozilla autopush / Apple Push through a service worker.

## Non-goals

- Persistent event log with "you missed these" replay. Durable state
  lives in real tables (`notifications`, `collab.doc_sessions`, …);
  the bus is a live-delivery optimization, always best-effort.
- Chat / DM / voice / video / screen share. Explicitly out of scope
  for OxiCloud — that's Nextcloud Talk territory, not a file-server
  job.
- Wildcard subscriptions (`folder:*`). Breaks per-subscribe AuthZ and
  makes revocation semantics fuzzy.
- Cross-user subscriptions. Privacy + AuthZ risk. Admins subscribe to
  `admin:*` topics, never to another user's private feed.

## Architecture — 3 layers, clean seams

```
┌──────────────────────────────────────────────────────────────────────┐
│ SERVICE LAYER                                                        │
│                                                                      │
│  FileMgmtService.create_file() ── after commit ──▶ bus.publish(...)  │
│  ShareService.grant()          ── after commit ──▶ bus.publish(...)  │
│  JobRegistry step progress     ─────────────────▶ bus.publish(...)   │
│  CollabSessionService.apply()  ─────────────────▶ bus.publish(...)   │
│                                                                      │
└───────────────────────────────┬──────────────────────────────────────┘
                                │  publish(&Topic, MessageBusEvent)
                                ▼
┌──────────────────────────────────────────────────────────────────────┐
│ MESSAGE BUS   (MessageBus trait — application/ports)               │
│                                                                      │
│   InProcessMessageBus (v1)                                          │
│     DashMap<Topic, broadcast::Sender<MessageBusEvent>>                 │
│                                                                      │
└──────┬───────────────────────────────────────────────────────────────┘
       │
       │       ┌──────────────────────────────────────────────────────┐
       │       │ REPLICATOR (optional, feature-flagged)               │
       │       │                                                      │
       │       │  BusReplicator trait ── separate port                │
       │       │    - v1: NoopReplicator (single-instance)            │
       │       │    - v2: PgListenReplicator (pg_notify)              │
       │       │    - v3: BrokerReplicator (RabbitMQ / NATS)          │
       │       │                                                      │
       │       │  Sits BESIDE InProcessMessageBus, forwards          │
       │       │  local publishes outbound + inbound events           │
       │       │  from the broker back into local publish.            │
       │       └──────────────────────────────────────────────────────┘
       ▼
┌──────────────────────────────────────────────────────────────────────┐
│ WS HANDLER   (interfaces/api/handlers/rt_ws.rs)                      │
│                                                                      │
│   One BusSession per WS: HashSet<Topic> + outbound mpsc         │
│   - subscribe/unsubscribe frames → bus.subscribe(topic)              │
│   - each subscribed stream drains into the outbound mpsc             │
│   - AuthZ at subscribe (once), evict on grant-revoked                │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

**The seam that keeps RabbitMQ/NATS doors open is the replicator, not
the bus.** Services and the WS handler only ever see the local
`MessageBus`. A future `BrokerReplicator` publishes outbound + injects
inbound. Zero touch to callers.

## Backend components

### 1. Port + event types (`application/ports/message_bus_ports.rs`)

```rust
// Topic is a typed enum, not a string. Prevents typos, gives
// exhaustive matching for the AuthZ gate, encodes stably to
// wire keys for any broker (RabbitMQ topic exchange, NATS subject).
pub enum Topic {
    Folder(FolderId),
    File(FileId),
    Drive(DriveId),
    UserNotifications(UserId),
    UserAuthz(UserId),
    UserSessions(UserId),
    UserUploads(UserId),
    Job(JobId),
    Collab(FileId),
    CollabAwareness(FileId),
    FolderPresence(FolderId),
    FilePresence(FileId),
    FileComments(FileId),
    Calendar(CalendarId),
    AddressBook(AddressBookId),
    AdminSessions,      // admin-only
    AdminAudit,         // admin-only, sampled
}

impl Topic {
    pub fn to_wire_key(&self) -> String;        // stable dotted form
    pub fn parse(s: &str) -> Result<Self, ParseTopicErr>;
    pub fn required_perm(&self) -> AuthzCheck;  // used by the AuthZ gate
}

/// Wire mirror of `domain::services::authorization::Subject`.
/// Kept as its own type so the bus payload schema can evolve
/// independently of the domain enum.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipalRef {
    User  { id: UserId },
    Group { id: GroupId },
    Token { id: TokenId },   // anonymous public share link
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum MessageBusEvent {
    // Folder / File verbs — thin facts only, client refetches details.
    FileCreated  { file_id: FileId, name: String, parent_id: FolderId, actor: UserId },
    FileDeleted  { file_id: FileId, parent_id: FolderId, actor: UserId },
    FileRenamed  { file_id: FileId, old_name: String, new_name: String, actor: UserId },
    FileMoved    { file_id: FileId, from: FolderId, to: FolderId, actor: UserId },
    FolderCreated { folder_id: FolderId, parent_id: FolderId, actor: UserId },
    // Jobs
    JobStep      { job_id: JobId, step: u32, message: String },
    JobFinished  { job_id: JobId, outcome: JobOutcome },
    // Notifications
    Notification { notification_id: NotifId, kind: NotifKind },
    // Sharing — principal is any Subject (user, group, or public-link token).
    // `affected_users` is populated by the producer only for the group case,
    // so consumers of `file:{id}:shares` don't need to expand membership.
    ShareGranted {
        file_id: FileId,
        principal: PrincipalRef,
        role: GrantRole,
        affected_users: Option<Vec<UserId>>,
    },
    ShareRevoked {
        file_id: FileId,
        principal: PrincipalRef,
        affected_users: Option<Vec<UserId>>,
    },
    // Group membership changes — cascade grants to/from the affected user.
    GroupMemberAdded   { group_id: GroupId, user_id: UserId, actor: UserId },
    GroupMemberRemoved { group_id: GroupId, user_id: UserId, actor: UserId },
    // AuthZ eviction / re-evaluation signal
    AuthzChanged { affected: Vec<ResourceId> },
    // Presence (Phase B)
    PresenceJoined { user_id: UserId, display_name: String, color: String },
    PresenceLeft   { user_id: UserId },
    PresenceCursor { user_id: UserId, position: PresencePosition },
    // Collab (binary bytes on the wire, kept opaque in the enum)
    CrdtUpdate    { bytes: Bytes },
    // ... one variant per verb; enum > strings per project convention.
}

#[async_trait]
pub trait MessageBus: Send + Sync {
    /// Fire-and-forget. SYNC (not async) — services must not await
    /// under a DB transaction.
    fn publish(&self, topic: &Topic, event: MessageBusEvent);

    /// Returns a Stream so the impl can change (broadcast, mpsc,
    /// pg listener) without churn.
    fn subscribe(&self, topic: &Topic) -> Pin<Box<dyn Stream<Item = MessageBusEvent> + Send>>;
}

/// Kept SEPARATE from MessageBus so v2/v3 wiring is drop-in.
#[async_trait]
pub trait BusReplicator: Send + Sync {
    /// Called whenever the local bus publishes; may forward to broker.
    fn on_local_publish(&self, topic: &Topic, event: &MessageBusEvent);

    /// Long-running consumer task: reads remote messages and
    /// re-publishes locally. Started by DI, returns on shutdown.
    async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<(), BusErr>;
}
```

#### Group principals and fan-out

Grants target any `Subject` — `User(Uuid)`, `Group(Uuid)`, or
`Token(Uuid)` (public share link). A single `ShareGranted` event
therefore has **two distinct audiences with different delivery
paths**:

| Audience | Topic | Payload use |
|---|---|---|
| Share dialog on the resource (anyone with `Share` watching it) | `file:{F}:shares` | Thin fact: "new grantee X with role R"; UI refetches grant list |
| Each affected user (persistent "shared with you") | `user:{member}:notifications` (one publish per member) | Becomes a `notif.notifications` row via `NotificationService::create` |

The bus **never expands groups**. `NotificationService` is the
group-expansion boundary. `MessageBus` only fans out topics that
already exist as concrete `user:*` streams.

Post-commit sequence for `ShareService::grant(file=F, principal=Group(G), role=R)`:

```
1. Insert grant row, COMMIT.
2. members = GroupService::expand_transitive(G)   // via closure table
3. bus.publish(Topic::File(F).shares(),
       ShareGranted {
           principal: PrincipalRef::Group { id: G },
           role: R,
           affected_users: Some(members.iter().copied().collect()),
       })                                          // share-dialog fan-out
4. for member in &members {
       notification_service.create(NewNotification {
           recipient_id: *member,
           kind: "share.granted_via_group_membership",
           subject_type: "file", subject_id: F,
           actor_id: caller,
           data: json!({ "via_group": G, "role": R }),
       });
       // create() inserts row AND publishes to user:{member}:notifications
   }
5. for member in &members {
       bus.publish(Topic::UserAuthz(*member),
           AuthzChanged { affected: vec![Resource::File(F).into()] });
   }                                                // WS handler re-checks subs
```

`Token(_)` principals (public share links) fan out to `file:{F}:shares`
and to a single `user:{creator}:notifications` (kind
`share.link_created` / `share.link_revoked`). No `AuthzChanged` — token
holders don't have WS sessions in this model.

`GroupMemberAdded { group_id, user_id }` triggers the mirror cascade:
enumerate the group's grants → synthesize one
`share.granted_via_group_membership` notification per affected resource
for the new user → one `AuthzChanged` for the resource set. `Removed`
runs the revocation mirror.

Bounded fan-out is baked in from day 1 (see § Limits & backpressure):
groups larger than `max_notification_fanout` (default 1000) drop the
per-user notifications with an `event = "notification.fanout_truncated"`
audit line; the `file:{F}:shares` event still fires, so the share dialog
stays accurate, and the recipients discover the grant via UI on next
visit. This case is realistically the "all-employees" scenario where
individual bell pings are noise anyway.

Membership snapshotting: `GroupService::expand_transitive` runs at the
transaction boundary — we do NOT re-expand at publish time, because a
concurrent membership edit would then leak or duplicate deliveries. The
`members` set is captured then closed over into the post-commit block.

Coalescing: `NotificationService::create` de-dupes on
`(recipient, kind, subject_type, subject_id, day)` within a short
window. Alice in both `G1` and `G2`, both granted `F`, gets one
notification, not two.

### 2. In-process impl (`infrastructure/services/in_process_message_bus.rs`)

- `DashMap<Topic, broadcast::Sender<MessageBusEvent>>`, capacity 256 per topic.
- `subscribe` creates the entry lazily; wraps `Receiver` in
  `BroadcastStream` (converts `Lagged` into a stream-level marker; WS
  handler kills that session with a `revoked` frame, reason
  `slow_consumer`).
- Background GC: when a topic's `receiver_count() == 0` for >60 s,
  drop the sender.

### 3. Replicator scaffolding (day-1)

- `NoopReplicator` in v1. Wired in DI as `Arc<dyn BusReplicator>`.
- `InProcessMessageBus::publish` calls
  `replicator.on_local_publish(...)` **after** local fan-out.

Futures:

- **v2 — `PgListenReplicator`.** `pg_notify('oxi_rt', serde_json::to_string(event))` outbound; dedicated `sqlx::PgListener` connection inbound. Payload cap ~8 KB fine because events are thin. No new deployed service — reuses the existing PG.
- **v3 — `BrokerReplicator`.**
  - **RabbitMQ:** topic exchange `oxi_rt`, per-server exclusive auto-delete queue bound to `#` (or per-topic bindings for broker-side filtering). Non-durable messages, no user-level queues.
  - **NATS:** subject hierarchy = `oxi.rt.folder.{id}`, `oxi.rt.job.{id}`, etc. `Topic::to_wire_key()` maps directly. Core NATS (no JetStream) — ephemeral is the point.

**Invariant for any broker impl:** no user- or session-scoped state at the broker. Servers hold sessions; the broker is stateless fan-out. Keeps replicator swaps painless and prevents per-user queue leaks.

### 4. WS handler (`interfaces/api/handlers/rt_ws.rs`)

- Route `GET /api/rt/ws`.
- **Auth strategy — three accepted paths, all reuse the existing
  auth middleware:**
  - **Browser session cookie** (`oxicloud_access` or whichever cookie
    the auth middleware validates on REST). The WS upgrade request
    carries cookies by default; the same `auth_middleware` +
    `CurrentUserId` extractor produces `caller_id`. No new code path.
  - **Bearer JWT via `Sec-WebSocket-Protocol`** —
    `Sec-WebSocket-Protocol: oxi.rt.v1, authorization.bearer.<jwt>`.
    Standard workaround for browser `WebSocket` (which can't set an
    `Authorization` header) and native clients like our
    `rt-hurl-helper`. The handler reads the second subprotocol
    element, validates the JWT via the same path as the REST auth
    middleware, and echoes back the base subprotocol name in the
    handshake response.
  - **Ticket flow** (deferred until DPoP deployments matter) —
    `POST /api/rt/ticket` issues a 30-second one-shot ticket, URL is
    `/api/rt/ws?ticket=…`. NOT MVP; added when DPoP-strict
    deployments make cookie/bearer over WS awkward. Callers today
    have no functional need for it.
- On upgrade:
  - Extract `caller_id` from the auth mechanism above.
  - Auto-subscribe to `user:{caller}:notifications`,
    `user:{caller}:authz`, `user:{caller}:sessions`.
  - Spawn `BusSession` actor: owns `HashSet<Topic>`, outbound
    `mpsc::Sender<WsMessage>` (bounded 512), one reader task per
    subscribed topic.
- Per-frame:
  - `subscribe`: dispatch on `topic.required_perm() -> AuthzCheck`
    and run the matching gate. Three classes exist —
    resource-scoped (typically `Read`, sometimes `Share` /
    `Comment`), identity-scoped (`caller_id == subject_uuid`, no
    admin bypass), role-scoped (`caller.role == Admin`); plus the
    bespoke job-originator-or-admin check for `job:{id}`. Full
    matrix in **§ AuthZ model**. Deny → `denied` frame + audit
    `event = "message_bus.subscribe_denied"`. Allow → subscribe on bus,
    ack.
  - `unsubscribe`: drop the reader task for that topic.
  - `ping/pong` for keepalive.
  - Binary CRDT frame: route to `CollabSessionService` (see
    `docs/plan/markdown-collab.md`), not the generic path.
- On `user:{caller}:authz` event: walk session's subs, re-check each,
  evict any that lost access (`revoked` frame with reason
  `grant_revoked`).
- On resource-delete: bus publishes `AuthzChanged` for affected → same
  eviction path.
- Outbound queue full → close WS with 1013 "try again later"; client
  reconnects, refetches, resubs.

### 5. Service integration — publish AFTER commit

Rule: **`bus.publish` is called after the DB transaction commits,
never before, never inside**. If publish were inside the tx, a
rollback would still fan out to clients. If publish were async and
awaited, a slow subscriber could hold the tx open.

Pattern: services return `(result, Vec<MessageBusEvent>)` from the tx
boundary; the calling layer publishes after commit. Or a
`TxCommitHook` queues events and flushes on commit. Pick one, apply
everywhere.

## Frontend components

### 1. Singleton client (`lib/message-bus/client.svelte.ts`)

Location is `lib/message-bus/` (subsystem dir, mirrors `lib/auth/` and
`lib/upload/` — see `frontend/AGENTS.md`), NOT `lib/stores/` — the
client is subsystem-scoped plumbing, not global reactive state that
routes read from.

- **Fetches a ticket** via `POST /api/rt/ticket` (through `apiFetch`,
  so DPoP is applied; `getCsrfHeaders()` merged in for the state-
  changing POST). Ticket then passes on the WS upgrade via
  `Sec-WebSocket-Protocol: oxi.ticket.<uuid>` (NOT a query param —
  keeps the token off access logs and out of Referer / URL bar).
- Opens `wss://<same-origin>/api/rt/ws` with the subprotocol.
- **Refcounted subscriptions**:
  `#subs: Map<TopicKey, { count, handlers, revokedHandlers, acked }>`.
- On first refcount of a topic: send `rt.subscribe`; on last drop:
  send `rt.unsubscribe`.
- **On reconnect**: replay every already-known topic (client-side
  state survives the disconnect); fire `onReconnect` handlers so
  consumers refetch and catch up on events dropped during the
  outage window.
- **Backoff**: exponential (250 ms → 30 s), full-jitter. Circuit
  breaker at 20 consecutive failures (~5 minutes of retry —
  comfortably covers a cargo-release restart); trip logs one `error`
  line and stops until `messageBus.reconnect()` is called or the
  page reloads.
- **Reactive-safety rule**: every mutation entry point
  (`subscribe`, `onReconnect`, `reconnect`, `close`, `#call`) wraps
  its `$state` reads in `untrack(() => …)`. Without this a caller's
  `$effect` inherits a hidden dep on `state`, and each transition
  (idle → connecting → connected → disconnected → …) re-fires the
  effect — an observed 1000+/s loop on server-down. See the
  `feedback-svelte5-untrack-mutation-methods` memory for the
  general rule and the docstring on `subscribe` for the concrete
  case.
- **Health**: `state = $state<ConnectionState>` +
  `latencyMs = $state<number | null>` exposed for a future debug
  indicator (no UI consumes them yet — silent MVP).

### 2. Composables

```ts
// Generic — subscribe to any topic.
useTopic(topic, onEvent, onRevoked?)

// Folder-view sugar — per-verb handlers + reconnect hook.
useFolderTopic(() => folderId, {
    onFileCreated, onFileRenamed, onFileMoved, onFileDeleted,
    onFolderCreated, onFolderRenamed, onFolderMoved, onFolderDeleted,
    onRevoked,      // grant revoked, subscription evicted server-side
    onReconnect,    // WS came back; consumers refetch to catch up
})

// Session-level reconnect (fires on 2nd+ open, never initial).
useReconnect(cb)
```

Handles `$effect` lifecycle (subscribe on mount, unsubscribe on
destroy). Zero connection awareness in components.

## Wire protocol

Two wire formats share the same WS connection:

- **Control + notifications: JSON-RPC 2.0** — universally recognized,
  no library needed on either side, standard `id`-correlated
  responses, standard `error` object shape, id-less notifications for
  server-pushed events. Adopts the same well-known framing as
  Ethereum node WS APIs, LSP-over-WS, and countless other services;
  costs ~30 bytes/message over a bespoke shape and buys instant "oh,
  it's JSON-RPC" recognition + off-the-shelf client compat.
- **CRDT binary frames: Yjs sync protocol** — de-facto standard in the
  Yjs ecosystem, kept as-is because it's the reason we picked Yjs.

Method namespace for our JSON-RPC methods: `rt.*` — a short opaque
prefix reserved for message-bus methods. Prevents collisions if we
ever expose additional RPCs on the same WS (not planned, but the
namespace costs nothing).

### JSON-RPC frames (control + events)

```jsonc
// c → s (requests — id-correlated)
{ "jsonrpc": "2.0", "id": 42, "method": "rt.subscribe",
  "params": { "topic": "folder:abc" } }
{ "jsonrpc": "2.0", "id": 43, "method": "rt.unsubscribe",
  "params": { "topic": "folder:abc" } }
{ "jsonrpc": "2.0", "id": 44, "method": "rt.ping" }

// s → c (responses to requests — same id)
{ "jsonrpc": "2.0", "id": 42,
  "result": { "subscribed": "folder:abc" } }
{ "jsonrpc": "2.0", "id": 43,
  "result": { "unsubscribed": "folder:abc" } }
{ "jsonrpc": "2.0", "id": 44,
  "result": { "pong": true } }

// s → c (denials — same id, standard JSON-RPC error object)
{ "jsonrpc": "2.0", "id": 42,
  "error": { "code": -32001, "message": "no_read",
             "data": { "topic": "drive:xyz" } } }

// s → c (events — id-less = JSON-RPC notification)
{ "jsonrpc": "2.0", "method": "rt.event",
  "params": {
    "topic": "folder:abc",
    "event": "file_created",
    "data":  { "file_id": "…", "name": "notes.md",
               "parent_id": "abc" },
    "actor": { "user_id": "…" },
    "ts":    "2026-09-08T20:12:00Z"
  }}

// s → c (server-initiated eviction — also a notification)
{ "jsonrpc": "2.0", "method": "rt.revoked",
  "params": { "topic": "folder:abc", "reason": "grant_revoked" } }
```

### Yjs binary frames (CRDT — Phase A collab consumer)

```
[1 byte kind][16 bytes doc_id][payload…]
  0x01 = Yjs update      → collab:{doc_id}
  0x02 = Yjs awareness   → collab:{doc_id}:awareness
  0x03 = Yjs sync-step   → collab:{doc_id}
```

The WS handler classifies incoming frames by the `MessageType`
(text/binary). Text frames are JSON-RPC; binary frames are Yjs sync
protocol routed to `CollabSessionService` (see
`docs/plan/markdown-collab.md`).

### JSON-RPC error codes (stable — never repurpose)

Uses the JSON-RPC 2.0 "server-defined" range `-32000` to `-32099`,
per spec (`-32700..=-32000` is the reserved-by-spec block; `-32000`
downward is application-defined).

| `code` | `message` | Meaning | Audit `reason` variants |
|---|---|---|---|
| `-32001` | `"no_read"` | Resource-scoped topic, caller lacks Read (or resource doesn't exist — indistinguishable to caller by design). Anti-enum invariant. | `no_read`, `no_such_resource` |
| `-32002` | `"no_share"` | Resource-scoped topic requiring `Share`, caller has Read but not Share. Applies to `file:{id}:shares` (Phase B). | `no_share` |
| `-32003` | `"no_comment"` | Resource-scoped topic requiring `Comment` (`file:{id}:comments` Phase B). | `no_comment` |
| `-32004` | `"topic_forbidden"` | Identity-scoped mismatch OR unknown/malformed topic. Same wire code regardless of whether the target user exists — anti-enum. | `identity_mismatch`, `unknown_topic`, `not_admin` |
| `-32005` | `"sub_limit"` | Per-connection sub cap hit. | `sub_limit` |
| `-32006` | `"rate_limited"` | Subscribe-frame token bucket exhausted. | `rate_limited` |
| `-32007` | `"no_edit"` | CRDT edit frame from a caller without `Edit`. Emitted as a `rt.write_denied` notification (not tied to a request `id`). | `no_edit` |
| `-32603` | `"internal_error"` | Standard JSON-RPC internal error — server-side failure the client should retry. | — (server log) |
| `-32600` | `"invalid_request"` | Malformed JSON-RPC envelope (missing `method`, wrong `jsonrpc` version). Standard JSON-RPC. | `bad_envelope` |
| `-32601` | `"method_not_found"` | Method outside the `rt.*` allowlist. Standard JSON-RPC. | `unknown_method` |
| `-32602` | `"invalid_params"` | Method known but `params` shape wrong (missing `topic`, unparseable). Standard JSON-RPC. | `bad_params` |

Codes `-32001..=-32007` are our application-defined vocabulary; the
`-326xx` range is JSON-RPC's own standard set and we honour it for
envelope-level problems. Both are stable — a new denial cause gets a
new code, we never repurpose an existing one, per project convention.

### Sec-WebSocket-Protocol subprotocol advertisement

Client's WS handshake sends:
`Sec-WebSocket-Protocol: oxi.rt.v1, authorization.bearer.<jwt>`

Server accepts the handshake with `Sec-WebSocket-Protocol: oxi.rt.v1`
(the bearer half is consumed for auth, not echoed). The `v1` gives
us a bump-when-we-break contract handle; adding new methods stays
backward-compatible under `oxi.rt.v1`.

### Payload discipline

Event payloads are **thin facts** (IDs + actor + verb). Never full
DTOs — client refetches details via REST if it needs them. Keeps the
AuthZ surface small (thin payloads can't leak fields the caller
couldn't already read via REST for that resource) and makes the pg
NOTIFY 8 KB cap a non-issue.

## AsyncAPI generation

Mirror OpenAPI's role for the REST surface. The WS surface gets a
machine-readable AsyncAPI 3.0 document generated from the same Rust
enums the server uses, so the wire contract stays in sync with
implementation by construction — no hand-written spec that drifts.

### What it documents

- **Server info + subprotocol** — `oxi.rt.v1` under
  `Sec-WebSocket-Protocol`, connect URL, auth mechanisms.
- **Channels** — one per topic-kind (`folder`, `file`, `job`,
  `user-notifications`, `collab`, …), parameterized by their id:
  `folder/{folderId}`, `job/{jobId}`, etc.
- **Operations per channel:**
  - `send` — client subscribe / unsubscribe via `rt.subscribe` /
    `rt.unsubscribe` (JSON-RPC request messages).
  - `receive` — server events via `rt.event` notifications.
- **Message schemas** — the JSON-RPC envelope and one schema per
  `event` variant (`file_created`, `folder_created`,
  `share_granted`, `notification`, …). Generated via `schemars` from
  the same Rust `MessageBusEvent` enum the server publishes, so the
  schema is authoritative, not aspirational.
- **Error object shape + `code`/`message` catalog** — the JSON-RPC
  error table above becomes an AsyncAPI-declared `errors` block on
  the subscribe operation.
- **Binary frame schema** — a `application/octet-stream` message
  binding for the Yjs sync protocol frames, with a text description
  of the `[kind][doc_id][payload]` layout. AsyncAPI schemas can't
  fully describe the Yjs framing (it's out-of-band from the JSON
  envelope), so we document the structure in prose alongside a
  placeholder schema — same tradeoff every WS spec makes with binary
  bodies.

### Generator — `cargo run --bin generate-asyncapi`

Follows the same shape as `generate-openapi`:

- New binary `src/bin/generate_asyncapi.rs` that constructs the
  spec from `Topic`, `MessageBusEvent`, `AuthzCheck`, and the JSON-RPC
  method/error tables — all live in `application/ports/message_bus_ports.rs`
  as the single source of truth.
- Uses `schemars` for JSON Schema of each event variant (already
  compatible with `serde` derives; no re-annotation needed).
- Emits `resources/gen/asyncapi.yaml` (YAML for human-diffability,
  same choice AsyncAPI tooling defaults to).
- Add `just asyncapi` recipe alongside `just openapi`.
- CI check: same as the OpenAPI check — regenerate on every build,
  fail if the working tree is dirty after regeneration. Keeps spec
  and code from drifting.

### Consumers

- **Docs site** — AsyncAPI has a first-class HTML renderer
  (`@asyncapi/html-template` or the Studio playground). Point the
  docs at `resources/gen/asyncapi.yaml` and the WS surface has the
  same discoverability as `openapi.json`.
- **Client SDK generation (later)** — `@asyncapi/generator` produces
  typed clients (TS, Go, Python, Java). Not needed for v1, but the
  door is open when a third-party integration asks for one.
- **Contract testing (later)** — the spec doubles as a contract the
  smoke tests can assert against; `rt-hurl-helper` could validate
  incoming events against the schema before asserting on values.
  Cheap follow-up.

### Scope for the first PR

- Generator produces spec covering the Phase-A-MVP surface only
  (`rt.subscribe` / `rt.unsubscribe` / `rt.ping` methods,
  `rt.event` notification, `Folder(id)` and `UserAuthz(u)` topics,
  `FileCreated` / `FolderCreated` events, the error-code table,
  `defaultContentType`, `securitySchemes.bearerAuth`, and a `ping`
  operation with the `rt.pong` reply shape).
- Adding a new topic/event/method later is an enum variant + serde
  derive → regenerate → commit. Same discipline as OpenAPI.

### AsyncAPI follow-ups (deferred)

Land with their producer PRs; each is a small addition to
`generate-asyncapi.rs` alongside the code that emits it.

- **`rt.revoked` notification** on the Folder + File channels — the
  server-initiated eviction frame fired when a grant is revoked
  mid-session ([[project-message-bus]] AuthZ eviction section). Ships
  with the `AuthzChanged` publish hook in `ShareService::revoke`.
  Wire shape is already fixed by the plan; the AsyncAPI additions are
  a `RtRevokedNotification` message + a `receive`-action operation on
  every resource-scoped channel that supports eviction.
- **Yjs binary frames — prose section** at the doc level: AsyncAPI
  schemas can't fully describe the `[kind][doc_id][payload]` framing
  (it's out-of-band from the JSON envelope), so a plain-English
  section on the `Collab` channel description referring to
  `docs/plan/markdown-collab.md § Wire protocol` is the pragmatic
  documentation. Ships with the Collab channel definition when the
  editor PR lands.
- **Server variable expansion** — add a `port` variable so local dev
  URLs (`ws://localhost:8086/api/rt/ws`) can be expressed in tooling
  that reads the AsyncAPI URL template. Trivial addition; not
  blocking.
- **`defaultMessages` per channel** — AsyncAPI convention for
  reducing per-operation `$ref` boilerplate as the message count
  grows. Worth introducing once we hit ~10 messages per channel; MVP
  has 6 on Folder, still legible.
- **Bindings on messages** — declare `bindings.ws.headers` on the
  subscribe messages so tools can render the auth header shape (the
  spec knows about it via `securitySchemes`, but per-message bindings
  make it explicit at the point of use).
- **Reply message discrimination** — the `receiveFolderEvent`
  operation could split into per-event-kind messages
  (`RtFileCreatedEvent`, `RtFolderCreatedEvent`) instead of one
  polymorphic `RtFolderEventNotification` with `oneOf`. Better
  codegen for typed clients. Refactor when we generate an FE SDK.

### TypeScript client codegen via `@asyncapi/modelina` (Phase-A polish)

AsyncAPI has the same "spec → typed FE SDK" story OpenAPI has. Wire
it once, avoid hand-maintaining a growing catalog of message types.

- **Tool:** `@asyncapi/modelina` — the AsyncAPI-native model
  generator. Reads `resources/gen/asyncapi.json`, emits TypeScript
  interfaces + tagged unions for every message and schema. Actively
  maintained, produces idiomatic TS.
- **Not** `@asyncapi/generator`'s WebSocket TEMPLATE — that generates
  a full client SDK on assumptions (fetch shape, subscription model)
  that don't match our `useTopic` singleton store. Custom composable
  stays; only the message DTOs come from codegen.
- **Wiring:**
  - `frontend/package.json` dev-dep: `@asyncapi/modelina`.
  - Script `frontend/scripts/gen-message-bus-types.mjs` invokes Modelina,
    writes to `frontend/src/lib/generated/message-bus/`.
  - `just asyncapi-ts` recipe alongside `just asyncapi`.
  - CI dirty-tree check — regenerate on every build, fail if `git
    diff` on the generated folder is non-empty. Same discipline as
    OpenAPI's check.
  - Generated files carry a `// AUTO-GENERATED — do not edit; run
    `just asyncapi-ts` to regenerate` header.
- **What the FE gets:**
  - `type RtEvent = FileCreatedData | FileRenamedData | ...` — a
    tagged union keyed on the `event` discriminator, so the folder
    view's `switch (evt.event)` is exhaustive at compile time.
  - `RtSubscribeRequestBody`, `RtErrorResponseBody`, error-code enum,
    `RtPongResponseBody.result.pong === true` narrowed by type.
  - No divergence between wire spec and FE types — the CI check
    catches drift.
- **Also worth:** if we ever want a typed WS client for other
  languages (Rust sync client, Python integration), the AsyncAPI
  spec is the source; Modelina supports 8+ target languages.
- **Timing:** the current spec covers 8 event variants + 6 message
  envelopes. Marginal savings today; substantial as Phase B adds
  ~15 more event variants (comments, mentions, presence, share
  events). Set up now so the discipline is in place BEFORE the
  surface grows.

## AuthZ model (audit rules per AGENTS.md)

### The subscribe gate

"At least Read on the resource" is the **default** for resource-scoped
topics, but not the whole story. Every topic variant declares its own
gate via `Topic::required_perm() -> AuthzCheck`. Three classes exist —
the WS handler dispatches on the returned enum, it does not assume a
single check applies everywhere.

#### Class 1 — Resource-scoped (majority)

Default gate: `AuthorizationEngine::require(caller, resource, Read)`.

| Topic | Resource | Permission |
|---|---|---|
| `folder:{id}` | folder | `Read` |
| `folder:{id}:presence` | folder | `Read` |
| `file:{id}` | file | `Read` |
| `file:{id}:presence` | file | `Read` |
| `collab:{file_id}` | file | `Read` (Reader = view + own cursor; edits gate separately, see below) |
| `collab:{file_id}:awareness` | file | `Read` |
| `drive:{id}` | drive | `Read` (drive membership) |
| `calendar:{id}` | calendar | `Read` |
| `addressbook:{id}` | address book | `Read` |

Two Phase-B resource topics use a **stricter** permission because the
topic itself would leak enumeration metadata a Reader can't otherwise
see today:

| Topic | Actual permission | Why not Read |
|---|---|---|
| `file:{id}:shares` | `Share` (Owner-tier) | Reader sees the file's content, not who else has access. The share list is management metadata; the REST share endpoints already gate this way. |
| `file:{id}:comments` | Whatever the REST comments API decides — `Read` if comments are public to Readers; `Comment` if commenter-tier only | Consistency with REST. The bus does not invent a new policy. |

#### Class 2 — Identity-scoped

Gate: `caller_id == subject_uuid`. Plain equality. **No admin bypass**
— an admin cannot subscribe to `user:{other}:notifications`. Privacy is
a hard rule; cross-user monitoring uses admin topics, never a user's
private stream.

| Topic | Gate |
|---|---|
| `user:{u}:notifications` | caller == u |
| `user:{u}:authz` | caller == u |
| `user:{u}:sessions` | caller == u |
| `user:{u}:uploads` | caller == u |
| `user:{u}:trash` | caller == u |

Auto-subscribed topics (`user:{caller}:*`) at connect go through the
same check for consistency — the caller identity is derived from the
validated ticket, so this is by construction, but the code path must
not short-circuit.

#### Class 3 — Role-scoped

Gate: `caller.role == Admin` (or specific admin sub-role once we
introduce them).

| Topic | Gate |
|---|---|
| `admin:sessions` | admin role |
| `admin:audit` | admin role |

#### One non-resource topic — bespoke check

| Topic | Gate |
|---|---|
| `job:{id}` | `jobs.created_by == caller` **OR** admin role. Jobs are not in the AuthZ engine's resource set; the check lives in `Topic::required_perm()` and queries the job registry. |

### Enforcement rules

1. **AuthZ at subscribe time, not per event.** Fan-out is hot;
   subscribe is the choke point. Checking every event against every
   subscriber's grants would burn CPU on busy topics.
2. **Evict on grant loss** — do NOT keep re-checking to preserve a
   sub. The write path publishes `AuthzChanged { affected }` to
   `user:{u}:authz`; the WS handler walks that session's
   `HashSet<Topic>` and drops any sub whose resource intersects
   `affected`. Same eviction path for resource-delete, group-member
   removal, and admin kicks.
3. **Anti-enumeration on denials.** Per the graduated-denial
   convention (see `authz_require_graduated_denial`), the wire reason
   collapses cases the caller cannot distinguish; the audit line
   records the truth.
4. **CRDT edit frames re-check on the write side.** A Reader can hold
   a `collab:{file}` sub (view + cursor); their `0x01` update frames
   are dropped by the WS handler with `collab.write_denied` audit
   (`reason = no_edit`). Verified once per session and re-verified on
   `user:{caller}:authz` events.

### Wire-reason vocabulary (stable — never repurpose)

The wire uses JSON-RPC 2.0 `error` objects — see **§ Wire protocol →
JSON-RPC error codes** for the full `code`/`message`/audit-`reason`
mapping. That table is the authoritative one; this section
cross-references its audit-reason column for the AuthZ dispatch and
confirms the anti-enumeration collapse rules the wire honours.

### Audit-line convention

- **Connect reject** — `event = "auth.rt_ticket_rejected"`,
  `reason ∈ {expired, unknown, ip_mismatch, replay}`.
- **Subscribe deny** — `event = "message_bus.subscribe_denied"`, `reason`
  from the audit column above, plus `caller_id`, `topic`. Emitted
  BEFORE the wire `denied` frame.
- **Evict** — `event = "message_bus.subscription_evicted"`,
  `reason ∈ {grant_revoked, resource_deleted, admin_kick, group_membership_lost}`,
  plus `caller_id`, `topic`.
- **Collab edit rejected** — `event = "collab.write_denied"`,
  `reason ∈ {no_edit, session_evicted, external_write_conflict}`.
- **Notification fanout truncated** — `event =
  "notification.fanout_truncated"`, `reason = "over_max_fanout"`,
  `resource_id`, `principal`, `member_count`.

Every audit line uses `target: "audit"` per project convention. Wire
reasons are the compressed public vocabulary; audit reasons are the
uncompressed private truth.

## Limits & backpressure

| Limit | Default | Rationale |
|---|---|---|
| Subs per connection | 128 | Prevents runaway/malicious pinning of server memory |
| Subscribe frames/sec/conn | 50 | Token bucket, prevents storm-subscribing |
| Outbound mpsc slots/conn | 512 | Full → close WS 1013 |
| Broadcast ring slots/topic | 256 | Slow subscriber → lag → close WS + audit |
| Max event size | 8 KB | Fail-fast dev assertion; keeps pg NOTIFY cap a non-issue |
| Ticket TTL | 30 s | Short window, one-shot |
| `max_notification_fanout` | 1000 recipients / event | Beyond this, drop per-user notifications + audit `notification.fanout_truncated`; the `file:{id}:shares` event still fires. Covers the "all-employees" group case where individual bell pings would be noise. |

## Failure modes

- WS drop mid-session → client reconnects, ticket flow again,
  re-subscribes. Server discards session state.
- Publish under load → `broadcast::Sender::send` never blocks; slow
  subs lag out. Never let the publish path stall.
- Ticket replay → ticket is one-shot in-memory; second use is
  `denied` + audit.
- Post-commit publish failing → log a warning and move on. Do NOT
  retry into a queue; ephemeral events are best-effort by design.
- Replicator down (v2+) → local bus keeps working for same-instance
  subs; log the outage; alert.

## First PR — MVP scope and hurl smoke test

The smallest slice that proves fan-out works, topics are isolated,
and the AuthZ gate rejects unauthorized subscribes. Everything
larger (notifications table, presence, collab) rides on top later.

### Scope in

- `MessageBus` port + `InProcessMessageBus`.
- WS handler at `GET /api/rt/ws` with `subscribe` / `unsubscribe` /
  `ping` frames only (no CRDT binary frames yet).
- Auth: reuse existing `auth_middleware` — session cookie for
  browsers OR bearer JWT via `Sec-WebSocket-Protocol:
  oxi.rt.v1, authorization.bearer.<jwt>` for programmatic clients.
  Ticket flow deferred.
- Topics: `Folder(id)` (Class 1 — Resource-scoped, `Read`) and
  `UserAuthz(u)` (Class 2 — Identity-scoped, auto-subscribed at
  connect). No other topics accepted in MVP; parser returns
  `Unknown` → `denied` with `reason = topic_forbidden`.
- Events: `FileCreated`, `FolderCreated`. Publish hooks added in
  `FolderService::create_folder_with_perms` and
  `FileManagementService`'s file-create path (upload / chunked
  upload commit — publish AFTER commit only).

### Scope out (later PRs, not this one)

- Delete / rename / move publishes (same pattern, verified after
  create works).
- `user:{u}:notifications` topic, notifications table, bell UI.
- `job:{id}` topic, `collab:{id}` binary frames.
- Grant-revocation eviction (still enforced structurally via
  `Topic::required_perm` at subscribe, but no live evict-on-change
  wiring — that comes with the `AuthzChanged` publish hook in a
  follow-up).
- Ticket flow, rate limiting on subscribe frames, slow-subscriber
  metrics.
- Frontend integration (`useTopic`, folder-view autorefresh).
- `PgListenReplicator` — v2 multi-instance.

### Test surface — `rt-hurl-helper` (follows existing convention)

The api-test suite is entirely HTTP via hurl and cannot drive
WebSocket. Precedent for auxiliary Rust binaries exists in
`opaque-hurl-helper` and `dpop-hurl-helper` (both built with
`--features test_utils`, both invoked from `tests/api/run.sh`
outside the main hurl block). The bus test follows the same
pattern.

**New binary:** `src/bin/rt_hurl_helper.rs`, gated on
`test_utils`. Sole new crate dependency:
`tokio-tungstenite` — added under `[dependencies.tokio-tungstenite]
optional = true` and pulled in by the `test_utils` feature so the
release binary is unaffected. Never ships in production.

**CLI shape:**

```
oxi-rt-hurl-helper <mode> [flags]

  subscribe-and-collect         # runs in background alongside hurl
    --url ws://.../api/rt/ws
    --token JWT                 # bearer, passed via Sec-WebSocket-Protocol
    --subscribe TOPIC           # may repeat
    --expect-events N           # exit 0 when N events arrive
    --timeout DURATION          # overall cap, default 3s
    --output PATH               # write JSON summary on exit

  expect-denied                 # runs synchronously
    --url ws://.../api/rt/ws
    --token JWT
    --subscribe TOPIC
    --reason KEY                # expected denial reason, default: any
    --timeout DURATION          # default 2s
```

Exit codes: `0` = expectation met, `1` = expectation failed
(wrong event, unexpected event, timeout without hitting the target,
denied when expecting event, or vice versa), `2` = protocol error
/ connect failure.

Output JSON schema (for post-mortem assertions in shell):

```jsonc
{
  "subscribed":   ["folder:<uuid-a>"],
  "denied":       [],
  "events":       [ { "topic": "folder:<uuid-a>", "event": "file_created",
                      "data": { "file_id": "…", "name": "…",
                                "parent_id": "<uuid-a>", "actor": "…" },
                      "ts": "2026-…" } ],
  "timed_out":    false,
  "protocol_err": null
}
```

### Coverage — eleven scenarios, all green

The four MVP scenarios sketched below expanded to **S1–S11** as
Slice C, D, and F shipped. All orchestrated by
`tests/api/rt_bus_check.sh` invoked from `tests/api/run.sh` after the
main hurl block. Follows the `refcount_cascade` /
`thumb_import_check` patterns already in place.

Scenarios live today:

- **S1** — Positive delivery: subscribe A, upload into A, one
  `file_created`.
- **S2** — Topic isolation: subscribe A, upload into B then A;
  observe A's event only.
- **S3** — AuthZ denial: user2 subscribes to A without a grant →
  `no_read`.
- **S4** — Anti-enumeration parity: subscribe to a nonexistent
  folder returns the SAME `no_read` as S3.
- **S5** — Server keepalive: 3 s idle surfaces multiple RFC 6455
  Pings; session still delivers afterwards.
- **S6** — `file_deleted`: DELETE fires the publish hook.
- **S7** — Move fan-out: subscribe A+B, MOVE A→B, observe two
  `file_moved` (one per topic).
- **S8** — Grant-revoke eviction (Slice C): user2 subscribes to
  A+B (both granted); user1 revokes only A → `rt.revoked` for A,
  upload to B still delivers. Session survives.
- **S9** — Cross-user identity gate: user1 subscribes to
  `user:{user2_id}:authz` → `topic_forbidden` (identity mismatch;
  audit reason `identity_mismatch`; wire response indistinguishable
  from unknown topic per anti-enum).
- **S10** — Ticket happy path (Slice F): `POST /api/rt/ticket`,
  open WS with `oxi.ticket.<uuid>` subprotocol, subscribe +
  deliver.
- **S11** — Ticket single-use (Slice F): reusing a redeemed
  ticket fails the upgrade with 401 + audit
  `message_bus.upgrade_rejected reason=ticket_invalid`.

**Ready-file race fix**: the shell script uses a `wait_ready`
function that blocks on the helper's `--ready-file` (touched the
instant every requested subscribe is ack'd) instead of a
`sleep 0.4` heuristic that flaked on cold-cache runs. See
`rt-hurl-helper::Args::ready_file` and the wait_ready doc in the
shell script.

**Always rebuild the helper** — the guard `[[ ! -x $HELPER_BIN ]]`
was removed 2026-09-11 because it silently reused stale binaries
whenever the helper's source changed without touching the caller
shell. Cargo incremental short-circuits in ~50 ms; the cost is
negligible, the trap-free experience is worth it.

**Scenario 1 — Positive delivery** (fan-out works)

```
setup.hurl:
  - user1 logs in → capture $USER1_TOKEN
  - user1 creates folder A → capture $FOLDER_A

shell:
  rt-hurl-helper subscribe-and-collect \
      --token $USER1_TOKEN --subscribe folder:$FOLDER_A \
      --expect-events 1 --timeout 3s --output /tmp/rt_s1.json &
  sleep 0.3   # give the subscribe frame time to ack

actions.hurl:
  - user1 creates a file in $FOLDER_A

wait rt-hurl-helper
```

Assertion (jq on `/tmp/rt_s1.json`):
- `.timed_out == false`
- `.events | length == 1`
- `.events[0].event == "file_created"`
- `.events[0].data.parent_id == $FOLDER_A`

**Scenario 2 — Topic isolation** (no event on unsubscribed folder)

Verifies: a user subscribed only to folder A does NOT receive
events for actions in folder B, even when the user has full access
to both.

```
setup.hurl:
  - user1 creates folder B → capture $FOLDER_B  (folder A from S1 reused)

shell:
  rt-hurl-helper subscribe-and-collect \
      --token $USER1_TOKEN --subscribe folder:$FOLDER_A \
      --expect-events 1 --timeout 3s --output /tmp/rt_s2.json &
  sleep 0.3

actions.hurl:
  # First: create a file in B — user1 has full access, but we're
  # not subscribed to B, so nothing should arrive on the helper.
  - user1 creates a file in $FOLDER_B
  # Second: create a file in A — this triggers the helper's exit.
  - user1 creates a file in $FOLDER_A

wait
```

Assertion:
- `.events | length == 1`
- `.events[0].data.parent_id == $FOLDER_A`  ← NOT B
- no event with `parent_id == $FOLDER_B` present

The key invariant this locks in: **the server fans out per topic,
not per user or per drive**. A subscriber to `folder:A` sees only
`folder:A` events, even for topics they'd have permission to
subscribe to but didn't.

**Scenario 3 — AuthZ denial** (subscribe rejected on missing Read)

Verifies: a user without `Read` on a folder cannot subscribe to
its topic. Denial wire reason is `no_read`; audit line records
`message_bus.subscribe_denied` with `reason ∈ {no_read,
no_such_resource}`.

```
setup.hurl:
  - user2 registers and logs in → capture $USER2_TOKEN
  - (user2 has no grant on $FOLDER_A, which is user1's private folder)

shell:
  rt-hurl-helper expect-denied \
      --token $USER2_TOKEN --subscribe folder:$FOLDER_A \
      --reason no_read --timeout 2s
  # exit 0 = denied frame received with reason=no_read
```

Assertion is the helper's exit code (`0` pass, `1` fail). No
`/tmp` output file needed for a binary pass/fail.

**Scenario 4 — Anti-enumeration parity** (nonexistent folder ≡ no
access, from the caller's POV)

Verifies: subscribing to a folder that does not exist returns the
**same** wire reason as subscribing to a folder the caller can't
Read. Protects against a folder-enumeration oracle.

```
shell:
  rt-hurl-helper expect-denied \
      --token $USER2_TOKEN --subscribe folder:00000000-0000-0000-0000-000000000000 \
      --reason no_read --timeout 2s
  # exit 0 = same wire reason as scenario 3
```

Assertion: exit code 0. The audit line (checked out-of-band if we
wire log capture) records `reason = "no_such_resource"` — but the
wire reason is `no_read`, matching scenario 3. This is the
graduated-denial invariant from `authz_require_graduated_denial`.

### How the scenarios chain

All four run in one shell script, one WS connection is opened per
scenario for isolation (a helper invocation = a fresh WS). No
state carries between scenarios except the folder ids and tokens
captured in `setup.hurl`. Total wall-clock ≤ 10 s including
sleeps.

### Cleanup

Follows the existing api-test convention (per project memory
`api_tests`):

- Shared DB is dropped between full test-suite runs by
  `tests/common/stop-db.sh`.
- Storage is wiped at run start.
- No per-scenario teardown; folders A and B persist for the rest
  of the run — no test that runs after this cares about them.

### justfile / CI hook

Add to the existing `test-api` recipe list of Rust helper builds
(there's already a compile step for `opaque-hurl-helper` /
`dpop-hurl-helper`); the new binary joins the same
`--features test_utils` build. `tests/api/run.sh` gets one line —
`./rt_bus_check.sh || die "rt bus smoke failed"` — inserted after
the main hurl block, before the existing storage-cleanup / thumb
checks.

### What this coverage locks in

- Subscribe path AuthZ gate is real (S3, S4).
- Anti-enumeration parity between "no perm" and "no resource" (S4)
  — the invariant the plan promises.
- Fan-out is topic-scoped, not user-scoped (S2).
- Publish-after-commit produces exactly one event per action (S1),
  not zero (rollback lost the publish) and not multiple (retry /
  double-hook).
- End-to-end wire format is stable (S1 asserts on
  `event = "file_created"` string).

Everything else in the plan — evict-on-revoke, slow-subscriber
kick, rate limiting, ticket flow, PgListen replicator — is
follow-up test work with its own scenarios, layered on top of
this baseline once the baseline is green.

---

## Roadmap

### Phase A — Foundation (bus + notifications + MD collab)

Ships the infrastructure and the two most visible consumers together.

- **✅ Bus port** + `InProcessMessageBus` + `NoopReplicator` + WS
  handler + **ticket endpoint (F)**.
- **✅ Frontend singleton** + `useTopic` + `useFolderTopic` +
  `useReconnect` composables. `oxi:message-bus` logger namespace.
- **Topics live today**: `folder:{id}`, `user:{u}:authz`.
- **Topics reserved but not producing**: `user:{u}:notifications`,
  `job:{id}`, `collab:{file_id}`, `collab:{file_id}:awareness` —
  land with their consumers below.
- **✅ Folder-live updates**: `FolderService` / `FileUploadService` /
  `FileManagementService` / `TrashService` publish `file_created /
  renamed / moved / deleted` and `folder_created / renamed / moved /
  deleted` after commit; FE folder view refetches on receipt (100 ms
  coalesce, idempotent). Multi-user + multi-tab verified.
- **✅ Grant-revocation eviction** (Slice C): `AuthzChanged` →
  per-topic `rt.revoked`; folder view toasts + navigates to
  `/files`. Session survives; unrelated subs unaffected.
- **✅ Refetch-on-reconnect**: `messageBus.onReconnect(cb)` →
  `useReconnect` composable → folder view refetches after WS comes
  back. Bridges the in-memory-bus "events lost during outage" gap
  (see `project_message_bus_reconnect_gap` memory).
- **Job dashboard live** — TODO (next slice). `JobRegistry`
  publishes step progress and terminal state on `job:{id}`; FE
  job dashboard subscribes and replaces polling. Operator value:
  once a long-running job is triggered (backend migration, thumb
  import, blobs consistency…), the admin can navigate to another
  page and come back without losing progress visibility — the WS
  push keeps whatever component is subscribed up-to-date.
- **Notifications table + bell** — TODO (Slice E). New
  `notifications` table + `NotificationService` port; initial
  ingesters for `share-granted`, `new-login-from-new-device`,
  `job-completed-for-you`, `storage-quota-threshold`. FE bell with
  unread count, slide-out panel, toast pop on receive. Auto-subscribe
  to `user:{u}:notifications` server-side, same pattern as
  `user:{u}:authz` today.
- **MD collab editor** — TODO. See companion plan
  `docs/plan/markdown-collab.md`. Depends on binary-frame routing
  which this plan sketches but doesn't ship (`rt_ws.rs` today drops
  binary frames with a debug log).

Deliverables sized ~4 weeks end-to-end. Slice D (folder-live) and
Slice F (ticket flow) landed 2026-09-11. Slices E + collab are the
open work in Phase A.

### Deferred — everything below is on the shelf

None of these ship on a fixed date; each is triggered by a concrete
consumer need. Grouped by theme (workspace UX, infrastructure
payoff, replicator) so the reader still sees the connective tissue,
but there is no commitment to sequencing.

#### Workspace UX (formerly "Phase B")

Turns OxiCloud from a file store into a shared workspace. Ship when
a specific feature here graduates from "would be nice" to "the
product needs it".

- **Presence topics** — `folder:{id}:presence`, `file:{id}:presence`.
  Awareness-style: joined/left/cursor. Ephemeral, not persisted.
- **FE presence UI** — "N people viewing" badge in folder header;
  avatar rail; hover to highlight; "someone is previewing this photo
  right now" in the lightbox.
- **Comments on any file** — new `comments` table (threaded, per
  file, supports reactions), `CommentService` port,
  `file:{id}:comments` topic for live delivery.
- **@mentions** — mention autocomplete in the comment editor;
  mention → notification into the mentioned user's
  `user:{u}:notifications` topic + `notifications` row + optional
  email (reuses existing `MagicLinkMailer`-style templating).
- **Reactions** — 👍❤️🎉 on comments and files. Live fan-out on the
  same `file:{id}:comments` topic.
- **Comment resolutions** — Google-Docs-style thread markers.
- **`NotificationService` as a bus subscriber** (architectural
  pivot). Up to Phase A the bus's publish calls sit inline in each
  mutation site (`FolderService::create_folder_with_perms`,
  `FileUploadService::upload_file_streaming`, and the delete /
  rename / move sites for both files and folders). That is the
  right shape and stays: the bus is location-keyed
  (`Topic::Folder(id)`, subscriber-scoped) and belongs at the
  mutation site. When notifications ship, they sit on the **same
  axis** (also location + actor + subscriber-driven) — not the
  `FileLifecycleHook` axis (which is server-internal, content-keyed,
  fan-out-to-all). So `NotificationService` becomes an in-process
  subscriber to the bus itself: it registers a `bus.subscribe(...)`
  on the topics it cares about (`folder:{id}`, `file:{id}`,
  share-grant events), translates relevant events into
  `notif.notifications` rows, and re-publishes on
  `user:{u}:notifications`. No new dispatcher, no new hook trait,
  no changes to existing mutation sites — the bus IS the
  mutation-event pipeline for anything subscriber-driven. Contrast
  with `FileLifecycleHook` (`src/application/ports/file_lifecycle.rs`):
  that stays focused on content transitions (blob_hash,
  content_type) and fires unconditionally to server-side workers
  (thumbnails, audio metadata, plugins). Bus and lifecycle-hook are
  complementary — same triggering moment, orthogonal fan-out shape
  and payload discipline. Do NOT try to unify them; the two axes
  are genuinely different (all-vs-subscribed × content-vs-location).

#### Infrastructure payoff (formerly "Phase C")

Where the bus starts paying for itself on operator cost. Ship when
sync-client PROPFIND traffic or the album-viewing experience
becomes a real bottleneck.

- **Sync-client push invalidation** — WebDAV / NextCloud DAV
  handlers publish `file:{id}` and `folder:{id}` deltas after
  commit. Sync clients get a lightweight `Sync-Invalidate`
  mechanism (or a dedicated WS endpoint for headless clients) so
  they refetch only changed paths instead of polling PROPFIND.
  Cuts a large chunk of Nextcloud-style client chatter.
- **Album live updates** — `folder:{album_id}` reused; as photos
  are added to an album, everyone viewing sees them appear.
- **Slideshow sync** — one presenter picks "Present"; other viewers
  of the album can opt-in to follow the presenter's current frame.
  Uses `folder:{album_id}` with a `presenter_frame` event kind.

#### Multi-instance & broker

Only invoked when the deployment actually needs it. Also the
mitigation for the "events lost during outage window" gap (see
`project_message_bus_reconnect_gap` memory) if durable replay
becomes important for collab or sync-push.

- **`PgListenReplicator`** — ship when we run more than one server
  instance. Same `BusReplicator` port, no consumer changes.
- **`BrokerReplicator`** for RabbitMQ or NATS — ship when either
  cross-datacenter fan-out or a shared broker with other services
  matters. Same port, no consumer changes.

#### Session-resume tokens (wire-protocol extension)

Replaces today's "full refetch on reconnect" workaround with a
delta-replay protocol: the client remembers the sequence number of
the last event it processed per topic; on reconnect, it says
"resume from N" and the server replays every event since N. The
canonical shape across the industry — Discord's `OP 6 Resume`,
Slack's sync API, Firestore's `resume_token`, Notion's sync-token
pattern. Cheaper than a REST refetch for high-fan-out topics (Yjs
CRDT deltas, notification streams) where the "catch-up" would
otherwise pull megabytes of state the client mostly already has.

Requires:

- **Server-side**: per-topic bounded ring buffer with monotonic
  sequence numbers. Bounded because we're not building a durable
  log — a hold-back of the last N events per topic is enough for
  the common "closed laptop for 10 min" case. A resume request
  older than the retention window falls through to a client-side
  full refetch (same code path today's `onReconnect` uses), so
  the client never fails hard — just degrades.
- **Wire**: `rt.subscribe` gains an optional `since: number` param
  and the ack carries the current sequence number. `rt.event`
  gains a `seq` field the client stores as `last_seq[topic]`.
- **Client**: `MessageBusClient` persists `last_seq[topic]` and
  replays it on `#onOpen`'s subscribe-replay. `onReconnect`
  handlers keep their fallback-to-refetch role for the
  older-than-retention case.

Meaningful for the collab editor slice (Yjs) and for future
sync-client push. Not worth doing before either of those lands —
folder-view refetch is a folder-page fetch (small); Yjs
"refetch" would be the whole doc snapshot (potentially large).
See `project_message_bus_reconnect_gap` memory (option 2).

#### Client-side connection efficiency

Optimizations to how the SPA holds its WebSocket. Independent of
server changes; ship when the per-user connection count actually
becomes a load concern. Today's grace-period tab-hidden close
(closes the WS after 60 s hidden, reconnects on visibility return)
covers the low-hanging fruit; both items below layer on top.

- **SharedWorker for multi-tab dedup** — one WebSocket per user per
  browser profile, shared across every same-origin tab via a
  `SharedWorker`. All tabs `postMessage` through the worker
  instead of holding independent `WebSocket` instances. Slack /
  Gmail / Google Docs all do this. Turns "user has 5 folder tabs
  open" from 5 WS into 1. Refactor cost: `MessageBusClient` moves
  behind the worker boundary; every `useTopic` call becomes an
  RPC to the worker instead of a direct method call. Payoff
  scales with per-user tab count — worth doing if operators see
  the "Live WS sessions" admin card sitting persistently at 5×
  the user count. Not worth it otherwise; grace-close already
  handles the common "background tab" case at ~30% of this
  refactor's complexity.
- **Web Push for truly-offline delivery** — service-worker-backed
  push notifications delivered by the browser vendor (FCM for
  Chrome, Mozilla autopush for Firefox, Apple Push for Safari)
  even when the user has no OxiCloud tab open. Complements the
  WS-based notification stream: WS delivers to foreground tabs;
  Web Push delivers to closed browsers. Requires server-side
  push-subscription store, per-vendor endpoint delivery (usually
  `web-push` crate), and a service worker in the FE. Meaningful
  UX win alongside Slice E (notifications bell) — a share
  arriving while the user is away actually reaches them.
  Deferred until Slice E ships; the two form a natural pair.

## What this bus does NOT replace

- Message queue / job queue — jobs stay in `job_registry`; bus just
  carries their progress live.
- Audit log — stays `tracing target: "audit"`.
- Email — `NotificationService`'s deliverer for offline users.
- Durable per-user "inbox" — the `notifications` table is the source
  of truth; bus is the live-delivery optimization.
