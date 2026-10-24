# Message bus via SharedWorker — one WebSocket per browser

**Status**: **DRAFT (2026-09-21).** Design record. No code yet. Owner:
TBD. Companions: `docs/architecture/message-bus-and-notifications.md`
(wire protocol reference), `docs/plan/dpop.md` (Gate C describes the
Service Worker that co-exists with the SharedWorker introduced here),
`docs/plan/markdown-collab.md` (largest current consumer of the WS
data-plane).

## Motivation

OxiCloud's message-bus client (`frontend/src/lib/message-bus/client.svelte.ts`)
opens **one WebSocket per tab**. Modern browser usage doesn't match
that assumption: users routinely keep 5–30 tabs of the same site
open, and every tab drags a full `/api/rt/ws` connection along
with it — plus its own `rt.subscribe` fan-out, its own DPoP-ticket
exchange, its own keepalive Ping traffic, and (for collab) its own
Yjs sync-step-1 catch-up per open file.

At the 10k-user scale target, that's noise piling on top of noise:

- **Server-side memory**: N users × M tabs × per-connection state
  (session snapshot, DPoP thumbprint check per frame, `subs`
  HashMap, `Sub` reader tasks). Every extra tab adds a socket, an
  event stream, a keepalive timer, and the CPU cost of applying
  every fan-out to that extra tab.
- **Server-side connection accounting**: axum tracks each WS in
  `active_ws_sessions`. TCP accept backpressure, file-descriptor
  budgets on the OS, and reverse-proxy connection limits (nginx
  `worker_connections`, Cloudflare tunnel caps) all count tabs
  as connections.
- **Client-side**: every tab pays for its own reconnect backoff
  when the server restarts, its own DPoP nonce refresh dance, its
  own subscribe replay. A user opening a folder in 8 tabs pays 8×
  the boot cost.
- **Bus event fan-out is redundant**: if user X has 8 tabs of the
  files listing open, the server sends the SAME `file_created`
  event 8 times over 8 different TCP streams. All 8 tabs then
  render the same delta.

None of this hurts a single dev instance. All of it compounds at
scale.

The primitive that lets a browser share one connection across all
same-origin tabs is `SharedWorker`. This document plans the
migration.

## Design decisions locked upfront

**Right primitive: SharedWorker, not Service Worker.**
- Service Worker: aggressively terminated when idle (~30 s Chrome,
  ~15 s Safari). Holding a long-lived WebSocket in one fights the
  platform. There is no `beforesuspend` hook to flush; on wake, the
  WS is dead. The SW's actual purpose (network `fetch` interception,
  push notifications, offline cache) doesn't fit "keep a socket
  hot" at all.
- SharedWorker: purpose-built for cross-tab shared state. Alive as
  long as ≥1 tab holds a `MessagePort` to it, dies when the last
  port closes. WebSockets and IndexedDB and `crypto.subtle` all
  work normally. No throttling of long-lived TCP.

**Coexistence with the DPoP Service Worker (already shipped).** The
existing DPoP-signing SW (`frontend/src/service-worker.ts`)
intercepts `/api/*` fetches from any origin-scoped client — page,
dedicated worker, or **shared worker**. So the SharedWorker's ONE
HTTP call it needs to make (`POST /api/rt/ticket` to redeem a
short-lived ticket for the WS upgrade) transits the DPoP SW
transparently. The SharedWorker never touches DPoP directly. Zero
code change to the SW; keypair stays in origin-scoped IndexedDB.

The WS upgrade itself doesn't need DPoP — the ticket flow was
designed exactly for the "browser can't set custom headers on
`new WebSocket()`" case (`Sec-WebSocket-Protocol: oxi.ticket.<uuid>`
carries the credential).

**Must support graceful fallback.** Enterprise CSPs disable
`SharedWorker` via `SharedWorkerAllowed=false`; some sandboxed
iframes can't reach it; iOS PWAs installed to home screen have
historically been spotty; Firefox private-browsing has occasionally
disabled it. The migration MUST detect and fall back to today's
per-tab shape — silently, without regression.

**One core, two harnesses.** The maintenance-cost trap is two
divergent implementations of the same `MessageBusClient` API. To
prevent it: keep exactly ONE implementation of the client logic
(backoff, subscribe replay, DPoP-ticket, revoked/write-denied
handling, capabilities cache). Both modes wrap it:

- Per-tab mode: consumers import the singleton directly (today's
  shape).
- Shared mode: the same client runs inside the SharedWorker; a
  thin **bridge** proxies calls over a `MessagePort`. Consumers see
  the same public API — the bridge implements
  `MessageBusClient`, verified at compile time by a TypeScript
  conformance check.

A new feature (new subscribe param, new event kind) requires one
line on each side of the port protocol — never a duplicate
implementation. Drift is caught at compile time by the conformance
type.

**No server-side changes.** The wire protocol (`rt.subscribe` /
`rt.event` / `rt.revoked` / `rt.write_denied` / `rt.collab_flush`
/ binary Yjs frames) is identical from the server's perspective.
The server sees one WS per browser instead of N per user — that's
a reduction in load, not a schema change.

## Non-goals

- **Cross-browser sharing.** Same-origin means same browser session.
  A user opening OxiCloud in Chrome AND Firefox still has two
  WebSockets; that's inherent to the origin sandbox.
- **Cross-origin sharing.** Embedding OxiCloud in a third-party page
  won't share the parent site's SharedWorker. Same-origin only.
- **Reducing the number of Yjs client IDs.** Two tabs on the same
  file each generate their own `Y.Doc` and therefore their own Yjs
  client ID — peers still see two named cursors from the same
  user. Cosmetic; can be papered over later if it becomes a
  complaint.
- **Anything that removes the per-tab bus client entirely.** The
  fallback path stays supported forever; SharedWorker is an
  optimisation, not a lock-in.

## Coverage / compatibility matrix

| Environment | SharedWorker | Falls back? |
|---|---|---|
| Chrome (all supported versions) | ✅ | — |
| Firefox (all supported versions) | ✅ | — |
| Safari ≥ 16.4 (Mar 2023) | ✅ | — |
| Safari < 16.4 | ❌ (no API) | Per-tab |
| iOS Safari 16.4+ browser tab | ✅ | — |
| iOS PWA (home-screen installed) | ⚠️ historically flaky | Per-tab (detected by handshake) |
| Enterprise Chromium with `SharedWorkerAllowed=false` | ❌ (throws) | Per-tab (detected by try/catch) |
| Sandboxed iframes without `allow-same-origin` | ❌ | Per-tab |
| Playwright / headless with default flags | ✅ | — |

The fallback is invisible to consumers of the `MessageBusClient`
API. Runtime handshake — not `typeof SharedWorker !== 'undefined'`
— decides which path is live.

## Architecture

```
                ┌─────────┐   ┌─────────┐   ┌─────────┐
                │  Tab A  │   │  Tab B  │   │  Tab C  │
                │ Svelte  │   │ Svelte  │   │ Svelte  │
                │ + Bridge│   │ + Bridge│   │ + Bridge│
                └────┬────┘   └────┬────┘   └────┬────┘
                     │              │              │
                     ▼              ▼              ▼          ports
                  ═════════════════════════════════════       ══════
                            SharedWorker                   ═══════════
                     ┌──────────────────────────┐        (one instance,
                     │ MessageBusClient (core)  │         same script,
                     │ + routing table          │         same "name")
                     │ + ports registry         │
                     └──────────────┬───────────┘
                                    │
                                    ▼   single WebSocket
                              /api/rt/ws
                                    │
                                    ▼
                              OxiCloud server
```

Each tab owns exactly one `MessagePort`; the worker owns exactly
one WebSocket. Tabs speak to the worker via port; the worker speaks
to the server via WS.

### Routing state inside the worker

Four maps, keyed by port for cleanup:

```typescript
// worker.ts

interface SubState {
  serverSubbed: boolean;     // did we send rt.subscribe upstream?
  cachedAck: RtSubscribeAck; // reply to serve to late-joining tabs
  ports: Set<MessagePort>;   // fan-out targets
}

// Topic subs (`folder:*`, `user:*:authz`, etc.)
const topicSubs = new Map<string /* topic */, SubState>();

// Binary + write-denied handlers — keyed by file_id, not topic.
// Collab needs BOTH a topic sub (for the ack + revoked) and a
// binary handler (for the actual Yjs frames).
const fileHandlers = new Map<string /* fileId */, Set<MessagePort>>();

// JSON-RPC id → originating port. Only the tab that asked cares.
const pending = new Map<number /* upstream id */, MessagePort>();

// Reverse index for cleanup on port close.
const perPort = new WeakMap<MessagePort, {
  topics: Set<string>;
  fileIds: Set<string>;
  pendingIds: Set<number>;
}>();
```

The `MessagePort` is the tab's identity — no `tabId` needed,
`WeakMap` GCs the reverse-index entry automatically when the port
dies.

### Port protocol

Everything on the port is a JSON envelope. Roughly 15 message
types, defined in `lib/message-bus/port-protocol.ts` with runtime
typeguards:

```typescript
// Tab → Worker
| { kind: 'hello'; reqId: number; wantedLogLevel: LogLevel }
| { kind: 'subscribe'; reqId: number; topic: string }
| { kind: 'unsubscribe'; topic: string }
| { kind: 'register-binary-handler'; fileId: string }
| { kind: 'unregister-binary-handler'; fileId: string }
| { kind: 'register-write-denied-handler'; fileId: string }
| { kind: 'unregister-write-denied-handler'; fileId: string }
| { kind: 'send-binary'; bytes: ArrayBuffer /* transferable */ }
| { kind: 'call'; reqId: number; method: string; params?: object }
| { kind: 'ping'; reqId: number }
| { kind: 'set-log-level'; namespace: string; level: LogLevel }

// Worker → Tab
| { kind: 'hello-ack'; reqId: number; state: ConnectionState }
| { kind: 'state'; state: ConnectionState }
| { kind: 'ack'; reqId: number; result: unknown }
| { kind: 'nack'; reqId: number; error: RtErrorObject }
| { kind: 'event'; topic: string; params: RtEventParams }
| { kind: 'revoked'; topic: string; params: RtRevokedParams }
| { kind: 'binary'; fileId: string; bytes: ArrayBuffer }
| { kind: 'write-denied'; fileId: string; params: RtWriteDeniedParams }
| { kind: 'ping-pong'; reqId: number; latencyMs: number }
| { kind: 'log'; level: LogLevel; namespace: string; args: unknown[]; ts: number }
```

Every envelope has a runtime typeguard. Envelopes with `reqId`
correlate a call to its response; envelopes without are broadcasts.

### Binary transferables

- **Tab → Worker → Server** (one-hop chains): use `postMessage(msg,
  [buffer])` — zero-copy transfer of ownership.
- **Server → Worker → N tabs** (fan-out): the `postMessage` API can't
  transfer to N ports; structured clone per port is unavoidable.
  Cheap for OxiCloud's frame sizes (20–200 B per Yjs UPDATE) but
  worth calling out.
- **Single-port fast path**: when the fan-out target set has size
  1, transfer instead of clone. Two-line branch that recaptures
  zero-copy for the very common case of one tab per file.
- **`SharedArrayBuffer` is deliberately out of scope.** True
  zero-copy fan-out is possible but requires site-wide COOP + COEP
  response headers, which break cross-origin iframes / images / PDFs.
  For the frame sizes we're moving, the trade is a bad one.

### Behavioral parity table

Only items where the two modes legitimately differ:

| Behavior | Per-tab | Shared |
|---|---|---|
| Server-side sub count | N tabs × M topics | 1 × M for the whole browser |
| Server-side WS count | N per user | 1 per user |
| Reconnect UI | Only affected tab flaps | ALL tabs flap together |
| `MAX_SUBSCRIPTIONS_PER_CONNECTION` | Per-tab budget | Shared browser budget |
| Rate-limit on `rt.subscribe` | Per-tab bucket | Shared browser bucket |
| Latency probe `ping()` | Per-tab RTT | Shared |
| Awareness cursor count | 2 tabs same file = 2 cursors labelled same user | Same (Y.Doc-scoped, not connection-scoped) |
| `beforeunload` flush | Frame goes out on this tab's WS | Frame goes out on the worker's WS |
| DPoP ticket exchange | Per-tab (once per session) | Per-worker (once per browser session) |

The "reconnect UI flaps together" is the only user-visible
difference worth noting in release notes. Everything else is
ledger-only.

## Cross-tab logging — parity with today's per-tab console tail

Today every tab's DevTools console shows the message-bus log stream
under the `oxi:message-bus` namespace, filtered by that tab's own
`oxi.setLogLevel('oxi:message-bus', 'debug' | 'info' | ...)` setting
(see `frontend/src/hooks.client.ts` for the `window.oxi.setLogLevel`
helper the log framework installs). Moving the bus into a
SharedWorker would strip that: the worker has its own log context
and its own console — invisible to every tab.

**Requirement**: keep the same per-tab log tail. A user with
DevTools open on Tab A must see the bus's `subscribed`, `ticket
issued`, `connect scheduled`, `subscription revoked`, etc. lines as
before, at the same namespace, honouring the same per-tab level.

**Design**:

- The `MessageBusClient` core (running inside the worker) keeps
  logging exactly as today — `busLog.debug('subscribed', { topic })`
  et al. It's a plain `loglevel` call; unchanged code.
- **The worker's `loglevel` handler is replaced by a fan-out
  emitter.** Instead of writing to the worker's own console, the
  handler serialises the level + namespace + args and
  `postMessage(...)`s a `{kind: 'log', level, namespace, args,
  timestamp}` envelope to every connected port. Structured-clone-
  safe args only (strings + plain JSON objects) — matches what the
  bus already logs.
- **Each tab's bridge replays into the tab-local `loglevel` on
  receipt.** `busLog[level](...args)` fires in tab context; the
  tab's own filter (its persisted `oxi.setLogLevel` choice) gates
  whether it hits the DevTools console. `window.oxi.setLogLevel`
  keeps working exactly as today — the level lives in the tab's
  `localStorage`, `loglevel` reads it locally on every call.

**Filter push-down (optimisation, not correctness)**: emitting
worker debug lines to every port at all times is a real cost when
no tab wants debug. Bridge tells worker its desired level on
handshake and on any `oxi.setLogLevel` call; worker keeps a
`maxLevelWantedByAnyTab` and only emits envelopes at ≤ that level.
Off by default in Gate 2 (worker just emits everything at info+
so debug hides unless someone flips it); wire the push-down in Gate
5 alongside reconnect testing.

**Diagnostic guardrail**: log every log-envelope drop when a port's
send fails (dead port, structured-clone incompatibility). Worker
console shows those; not user-visible but keeps the fan-out
observable.

**Non-goal**: the SharedWorker's DevTools panel showing the log
stream. That's still available (Chrome DevTools → "Shared workers"
→ inspect) but nobody should need it — the whole point is to
mirror the log stream INTO every tab.

**Envelope**:

```typescript
| { kind: 'log';
    level: 'trace' | 'debug' | 'info' | 'warn' | 'error';
    namespace: string;
    args: unknown[];   // must be structured-clone-safe
    ts: number;        // Date.now() at emit; helps ordering
  }
```

**Namespace scope**: the worker forwards logs for its OWN
namespaces (`oxi:message-bus` today, `oxi:message-bus:worker` for
its own internal machinery if we split). Loggers that already run
tab-side (`oxi:collab` in `CollabDoc`, UI-scoped namespaces) stay
local — they don't cross the port.

## Gate 0 — Design record

- This document.
- No code.
- **Deliverable**: PR-reviewable design doc.

## Gate 1 — Port protocol module + conformance type

- New file `frontend/src/lib/message-bus/port-protocol.ts`:
  - Discriminated union of every envelope kind (as above).
  - Runtime typeguard `parsePortMessage(raw: unknown): PortMessage
    | null` — returns `null` on any shape mismatch so the sender's
    log line shows what was dropped.
  - Documented ordering + transferable contract.
- New file `frontend/src/lib/message-bus/client-interface.ts`:
  - Exports the `MessageBusClient` interface — the public API
    every consumer already depends on (subscribe, sendBinary,
    registerBinaryHandler, ping, collabFlush, reconnect, state,
    latencyMs). Extract from today's `client.svelte.ts` without
    changing behavior.
  - Conformance check: `type _AssertBridge = Assert<
    SharedWorkerBridge extends MessageBusClient>` — a TS trick
    that turns an unimplemented method into a compile error.
- No runtime behavior change; today's client keeps working.
- **Deliverable**: type-only refactor, `npm run check` clean, all
  existing tests pass.

## Gate 2 — SharedWorker script and bridge (dev-only)

- New file `frontend/src/lib/message-bus/worker.ts`:
  - `self.onconnect = (event) => { … }` — accepts a new port,
    stores it in the ports registry, wires message handlers.
  - Holds one `MessageBusClient` instance (imported from today's
    `client.svelte.ts` after Gate 1's refactor separated
    interface from implementation).
  - Implements the four routing maps described above.
  - Fan-out helpers `fanoutEvent`, `fanoutRevoked`, `fanoutBinary`,
    `fanoutWriteDenied` — each takes a topic-or-fileId + payload
    and iterates the matching port set. Single-port fast-path uses
    transferable; N-port path uses clone.
  - Cleanup on port `close`: walk `perPort.get(port)`, remove from
    each map, if a topic's `ports.size === 0` send upstream
    `rt.unsubscribe`.
- New file `frontend/src/lib/message-bus/shared-worker-bridge.svelte.ts`:
  - Implements `MessageBusClient`. Constructor takes a
    `SharedWorker`. Every method serializes a port envelope,
    tracks the `reqId → resolve/reject` in a local pending-map,
    and returns a Promise resolved by the matching `ack` /
    `nack` frame.
  - `subscribe()` allocates a client-local sub id + registers the
    callbacks; the incoming `event` / `revoked` port frames route
    to the right callback set.
  - `state` and `latencyMs` are `$state` runes updated from
    inbound `state` / `ping-pong` port frames.
- New file `frontend/src/lib/message-bus/detect.ts`:
  - `trySharedWorker(): Promise<SharedWorker | null>` — 3-step
    detection:
    1. `typeof SharedWorker === 'undefined'` → null.
    2. `try { new SharedWorker(url, { name: 'oxi-bus' }) }` →
       return null on throw (CSP block, sandboxed iframe).
    3. Live handshake — send `{kind: 'hello'}`, expect
       `{kind: 'hello-ack'}` within 1 s. Silence means the worker
       didn't load (404, MIME mismatch, iOS PWA quirks).
  - Handshake success is the only proof that shared mode works;
    every other signal is a suggestion.
- Modified `frontend/src/lib/message-bus/client.svelte.ts`:
  - New factory: `async makeMessageBus(): Promise<MessageBusClient>`.
  - Feature-flag gated: only tries SharedWorker if
    `serverConfig.features.shared_worker_bus === true` (added in
    Gate 3) or the URL bears `?shared_worker=1` (dev override).
  - Falls back to today's `PerTabMessageBusClient` on any failure.
- Vite config: SharedWorker entry needs `?sharedworker` import or a
  manual chunk config; verify build output has a stable URL for
  the script.
- **Test**: Vitest — mock `SharedWorker` to throw at construction,
  assert factory returns the per-tab client and every existing
  test still passes.

## Gate 3 — Server-side feature flag + opt-OUT rollout

- New field on `FeaturesConfig`: `shared_worker_bus: bool`
  (**default `true`**). Env `OXICLOUD_ENABLE_SHARED_WORKER_BUS`.
  Emitted on `/api/config.features.shared_worker_bus`.
- FE factory (Gate 2) reads it before `trySharedWorker`. When `true`
  + handshake passes, shared mode is live; when `false`, the factory
  returns `PerTabMessageBusClient` without touching detection. The
  mandatory fallback engages the same way for every browser that
  can't run shared mode even when the flag is on.
- Boot-time audit log on the non-default case:
  `tracing::info!(target: "audit", event = "config.feature_disabled",
   feature = "shared_worker_bus", ...)` — operators debugging a
  user report can grep boot logs to confirm an explicit opt-out
  without reading env or `/api/config`.

### Why opt-OUT, not opt-in

The feature exists specifically to reduce N tabs × WebSocket to one
per browser. Shipping off-by-default would mean the feature exists
but doesn't help anyone unless an operator proactively flips a flag
— which, for a self-hosted product where the operator is often also
the user, with poor env-var discovery, most won't. The feature
wasn't built to sit on the shelf.

The risk is bounded by the mandatory fallback (Gate 2a-2c). We spent
three gates building a handshake-tested, silently-engaged per-tab
path for every browser that can't run shared mode (Safari < 16.4,
enterprise CSP, iOS PWA, 1 s handshake timeout). That IS the
contain-the-blast-radius mechanism — we trust it. The fallback stays
supported forever; dropping per-tab mode is out of scope.

### Rollout plan

- Ship as `true` from the first release carrying the feature.
- Watch two signals over one release cycle:
  1. `admin.active_ws_sessions` gauge should drop proportionally
     to the tab-per-user ratio on every production instance.
  2. The FE's `oxi:message-bus` fallback-reason log counts
     (`typeof_undefined`, `construct_threw`, `handshake_timeout`)
     — the baseline should be dominated by `typeof_undefined`
     (actual old browsers). A spike of `construct_threw` or
     `handshake_timeout` is a real-world problem we didn't
     anticipate — operators roll back with
     `OXICLOUD_ENABLE_SHARED_WORKER_BUS=false` and restart.
- After one quiet release cycle, propose tombstoning the FLAG
  entirely in a future major (keeping the per-tab fallback code
  forever, just removing the ops-tunable knob). Not scoped here.

### Rollback semantics

Rollback is per-browser-reload, not per-flag. Flipping the flag
`true → false` and restarting the server does not immediately kill
every connected SharedWorker — the worker lives until its last tab
closes. During the transition window:

- Tabs with an alive-but-stale SharedWorker finish their session on
  shared mode.
- Newly-opened tabs read `/api/config.features.shared_worker_bus =
  false` and land directly on per-tab mode. If the old worker is
  still alive from another tab, both WS types coexist for that user
  until all shared-mode tabs close.

This is mildly inconsistent but not catastrophic — no data is lost,
all tabs keep working, the resource profile is wrong only during
transition. Operators who need a clean cutover can ask users to
close every browser tab before resuming.

## Gate 4 — Cross-tab correctness tests

- **Playwright: two tabs, one WS.** Open two tabs of the files
  listing, count `active_ws_sessions` on the server via the admin
  endpoint. Assert `1` under shared mode, `2` under per-tab mode.
  Assert both tabs receive folder-topic `file_created` events on
  an upload from a paired user.
- **Playwright: collab two tabs same file.** Two tabs open the
  same `.md`, both edit, both converge. Server sees 1 subscribe
  to `collab:<file>`, both tabs receive the fan-out.
- **Playwright: collab two tabs different files.** Two tabs each
  open a distinct `.md`, each gets its own subscribe, each sees
  only its own file's UPDATEs (no cross-talk on the port routing).
- **Playwright: force-fallback path.** Boot with the Chromium flag
  `--disable-features=SharedWorkerAPI` plus a throwaway
  `--user-data-dir`; assert the app comes up in per-tab mode
  (`window.messageBus.constructor.name === 'PerTabMessageBusClient'`,
  admin `active_ws_sessions === tabCount`) and every existing
  scenario still passes.

  **Flag verified in-the-wild 2026-10-24**: a manual run with
  `--disable-features=SharedWorkerAPI --user-data-dir=/tmp/<name>`
  produced 2 tabs → 2 active WS per the admin panel, exactly as
  shared-mode-disabled expects. Playwright's `launchOptions.args`
  reproduces this cleanly.

  Alternative: `addInitScript({ delete window.SharedWorker })`
  inside Playwright — same outcome, no OS-level flag needed,
  easier to toggle per-test.
- **Vitest: port protocol roundtrip.** Every envelope kind
  serialised → parsed → typeguard passes → structurally equal.
- **Vitest: conformance type.** `SharedWorkerBridge extends
  MessageBusClient` compiles. Removing a method from the bridge
  breaks compilation (guarded by a `// @ts-expect-error` test).

## Gate 5 — Reconnect + backoff behaviour under shared mode

- Verify that when the WS drops (server restart), the SharedWorker
  fires the `state` broadcast to every port, every tab shows the
  transient disconnected state, and reconnect resumes with the
  ambient backoff + half-jitter logic — one reconnect for the
  browser, not N.
- **Test**: Playwright with a scripted server restart mid-session,
  assert reconnect happens once (via server-side `active_ws_sessions`
  gauge and the audit line count), and that every open tab
  restores its state.

## Risks & call-outs

- **Enterprise CSP surface**: some organisations pre-disable
  SharedWorker fleet-wide. The fallback is what saves them, but
  we should log an audit `oxi:message-bus` info line when
  detection fails so operators can see why their instance is
  running in per-tab mode.
- **iOS PWA edge case**: home-screen-installed OxiCloud may run
  in an isolated storage container where SharedWorker doesn't
  reach the same instance. The 1-second handshake timeout will
  catch this and fall back.
- **Debugging is worse**: DevTools has a "Shared workers" panel
  but it's less polished than the "Service workers" panel.
  Consider an in-app `/dev/message-bus` view that surfaces the
  worker's routing tables at runtime — cheap Svelte component,
  ships with `?dev=1` gate only.
- **Update semantics**: SharedWorker script updates require every
  tab holding it to close before the new version loads. A
  post-deploy incident (e.g. a bad worker script that the fallback
  path fixes on reload) is harder to hot-fix without user
  intervention. The factory should log the worker script
  version at hello-ack time so operators know which mix is live.
- **Awareness cursor duplication**: 2 tabs of the same user editing
  the same file show as 2 named cursors to peers. Cosmetic but
  worth noting; a `Set<userId>`-based awareness filter could
  collapse them if it ever becomes a complaint.
- **State runes across tabs**: Svelte 5 `$state` in the bridge is
  per-tab; the worker's state broadcast projects into each tab's
  local rune. This is correct but means the FE code path is
  slightly more indirect (state change happens in worker →
  broadcast → bridge → rune update). Callers see identical
  behaviour but the reactive-graph shape is different — worth
  covering in the port protocol tests.

## Migration path from an operator's perspective

1. Upgrade OxiCloud. Feature ships **on by default** (opt-out). On
   browsers that support SharedWorker + CSP that allows it, users
   immediately drop from N WS per tab to 1 WS per browser. Browsers
   that can't run shared mode fall back silently to the per-tab
   client — no user-visible breakage.
2. `admin.active_ws_sessions` drops proportionally to the
   tab-per-user ratio across the fleet. The FE emits an
   `oxi:message-bus` `info` line on every fallback engagement with a
   stable `reason:` key so operators can count how many sessions
   went per-tab vs. shared.
3. On any incident, opt OUT by setting
   `OXICLOUD_ENABLE_SHARED_WORKER_BUS=false` and restarting. New
   tabs land on per-tab mode immediately; already-open tabs with a
   live SharedWorker finish their session on shared mode until
   closed (the worker outlives its last port close only).

**Rollback is boot-level**: no server-side schema change, no wire
change, no FE rebuild. Flip the env and restart.

## What NOT to build

- **A generic pub/sub abstraction on the worker.** The bus already
  is that. Don't add a second layer just because we now have a
  worker to put things in.
- **A cross-origin variant.** Same-origin is a security property,
  not a limitation to work around.
- **A "shared everything" worker.** Keep the SharedWorker's job
  narrow — WebSocket-driven bus routing. Anything else that
  wants a browser-scoped singleton (state store, cache) belongs
  in its own worker, if it belongs anywhere.
