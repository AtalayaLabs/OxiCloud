# UI Diagnostics

Runtime-tunable knobs and namespaced logging exposed to the browser
DevTools console so upload and session issues can be diagnosed without
a rebuild, a config flag, or a page reload.

All entry points live under a single global — `window.oxi` — attached
during the client init hook (`frontend/src/hooks.client.ts`). Type
`oxi.` in DevTools with autocomplete to discover what's available; the
sections below name what each surface does.

## The `oxi.*` helper

```js
oxi.log                            // raw `loglevel` module
oxi.setLogLevel(ns, level)         // toggle a namespace's level
oxi.listLogLevels()                // enumerate persisted overrides
oxi.UPLOAD_BATCH_BYTES             // get/set per-PUT byte cap for delta upload
await oxi.checkBusChain()          // round-trip the message bus end-to-end
```

`oxi.setLogLevel` returns a confirmation string (`"oxi:upload → debug"`)
so the DevTools echo is a positive signal instead of the confusing
`undefined` a naked `void`-returning setter would produce.

## Log levels

The frontend uses [`loglevel`](https://github.com/pimterry/loglevel)
with per-namespace levels. Each subsystem gets its own logger; toggle
them independently.

### Namespaces

| Namespace   | Emitted by                                                         |
| ----------- | ------------------------------------------------------------------ |
| `oxi:upload` | Delta + direct upload pipeline (`lib/api/endpoints/deltaUpload.ts`, `static/workers/deltaWorker.js`) |
| `oxi:message-bus` | Tab-side bus client — the `MessageBusClient` interface (per-tab OR the SharedWorker bridge). Logs subscribe lifecycle, reconnect handlers, dispatch errors. Emitted by `lib/message-bus/client.svelte.ts` + `lib/message-bus/shared-worker-bridge.svelte.ts`. |
| `oxi:message-bus:worker` | **SharedWorker-internal state** — the connection state machine, ticket flow, backoff cycles, binary dispatch. The worker fans these out as port envelopes; each tab replays them into tab-local `loglevel`. In shared mode they appear in every tab that enabled this namespace, from a single bus running once per browser. Emitted by `lib/message-bus/worker-core.ts` + `lib/message-bus/ws-bus-transport.ts`. |
| `oxi:collab` | Collab (Yjs) document lifecycle — sync-step state, awareness peers, write-denied falls, debouncer flush. Emitted by `lib/collab/collabDoc.ts`. |

New namespaces should follow the `oxi:<subsystem>` shape so a wildcard
filter across the whole app remains meaningful.

### Levels

`trace` &lt; `debug` &lt; `info` &lt; `warn` &lt; `error` &lt; `silent`.

Default per namespace is `info` — phase transitions and error paths
surface without any opt-in. Bump to `debug` for per-chunk /
per-batch verbose trace during a failure hunt.

### Runtime toggle

```js
// Deep dive into upload internals
oxi.setLogLevel('oxi:upload', 'debug')
// → 'oxi:upload → debug'

// Quiet mode — only warnings and errors
oxi.setLogLevel('oxi:upload', 'warn')

// Full silence
oxi.setLogLevel('oxi:upload', 'silent')

// See every namespace's current override
oxi.listLogLevels()
// → { 'oxi:upload': 'DEBUG' }

// Everything (including future namespaces) to debug
oxi.log.setLevel('debug')
```

Changes persist to `localStorage` under the key
`loglevel:<namespace>` — the choice survives page reloads and browser
restarts until you explicitly change it back or clear localStorage.

Worker context: `deltaWorker.js` runs in a Web Worker and can't
`import 'loglevel'` (the worker is served from `/static` without
bundler resolution). Instead it emits log events via `postMessage` and
the main-thread orchestrator relays them through the shared logger, so
`oxi.setLogLevel('oxi:upload', 'debug')` filters worker output too.

## Upload batch tuning

`oxi.UPLOAD_BATCH_BYTES` controls the target size of each `PUT
/api/files/delta/chunks` body — the delta worker groups missing chunks
into that size before sending. Default is 8 MiB; can go up (fewer,
larger requests) or down (more, smaller requests).

```js
oxi.UPLOAD_BATCH_BYTES              // read current value
// → 8388608

oxi.UPLOAD_BATCH_BYTES = 1024 * 1024   // 1 MiB per PUT
// → 1048576

oxi.UPLOAD_BATCH_BYTES = 8 * 1024 * 1024   // back to default (removes the override)
```

Persisted to `localStorage['oxi:upload:batchBytes']`. Setting the value
back to the default clears the entry so the storage stays clean.

### When to lower it

Behind reverse proxies with tight per-request timeouts. The classic
case is **Cloudflare Tunnel**: 100-second absolute per-request
timeout on the Free/Pro plans. A user on a slow uplink (say, hotel
Wi-Fi at 512 Kbps) can't complete an 8 MiB PUT in that window and gets
their request cut mid-flight. Lower to 1 MiB (~16 seconds at 512 Kbps)
and it fits comfortably.

Cost: about 8× more HTTP requests per file. TCP keep-alive amortises
most of the connection setup; the extra CPU is negligible.

### Read timing

Read once per upload at worker spawn time. Change from the console →
the NEXT upload picks up the new value; the currently-running upload
finishes with the old value. No reload required.

## Delta upload trace

A healthy fresh upload at `info` level looks like:

```
[3f7a2b] delta start                                     {file: "vacation.mp4", size: 524288000}
[3f7a2b] worker: worker start                            {file: "vacation.mp4", size: 524288000}
[3f7a2b] worker: wasm loaded
[3f7a2b] worker: hashed — blake3=<64-hex> (512 chunks)
[3f7a2b] worker: negotiate: 256 hashes → 240 missing, 16 dedup'd
[3f7a2b] worker: negotiate: 256 hashes → 256 missing, 0 dedup'd
[3f7a2b] worker: ✅ committed — uploaded 501346304 B, reused 22941696 B (4% dedup, blake3=<hash>)
[3f7a2b] worker: commit HTTP 201                         {blake3, uploadedBytes, reusedBytes, totalBytes, attempt}
[3f7a2b] delta done                                      {file, blake3, savedBytes, uploadedBytes}
```

At `debug` level the worker additionally emits one line per chunk PUT
(`chunk PUT: 28 chunks, 8825338 bytes`) — expect roughly one line per
`UPLOAD_BATCH_BYTES` worth of body sent.

Every line prefixes a short 6-hex upload id (`3f7a2b`) so concurrent
uploads stay distinguishable in the console. The `blake3` field is the
whole-file BLAKE3 hash the commit call carried — same value as
`storage.file_blobs.hash` on the server, so log lines correlate
directly to server-side blob rows.

### Common failure signatures

| Log line                                                              | Meaning                                                                                                   | What to try                                                                                                                    |
| --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `worker fallback: wasm unavailable: <detail>`                         | Browser blocked WebAssembly (CSP, extension, disabled in prefs)                                           | Check CSP `script-src 'wasm-unsafe-eval'` allows it; disable content-blocking extensions on the tab                            |
| `delta disabled for this tab: Worker constructor threw`               | `new Worker(...)` failed — usually CSP `worker-src`                                                       | Check CSP `worker-src` allows `blob:` if the worker uses one, or the origin                                                    |
| `worker: chunk PUT failed (HTTP 413)` → `worker requested fallback`   | Server (or a proxy in front of it) rejected the PUT body as too large                                     | Lower `oxi.UPLOAD_BATCH_BYTES`; check proxy body caps                                                                          |
| `worker: chunk PUT failed: <NetworkError / timeout>`                  | Cloudflare (or another proxy) cut the request mid-flight                                                  | Lower `oxi.UPLOAD_BATCH_BYTES` so each PUT fits inside the proxy's per-request timeout                                         |
| `worker: negotiate failed (HTTP 5xx)`                                 | Server-side error during the negotiate stage                                                              | Server logs; grep for the emitted `request_id`                                                                                 |
| `delta worker went silent for 20s — disabling delta for this tab`     | Worker stopped emitting progress — WASM hang, DoS, or extreme main-thread contention. Poisons this tab.   | Reload to reset the poison flag; check `console` for errors emitted by the WASM module or the worker itself                    |
| `delta timeout after Xs — falling back to direct upload`              | Wall-clock timeout (`120s + 90s per GB`) — usually means chunk PUTs are stalling                          | Look for the last `chunk PUT` line; if none appeared for many seconds, network is stalled at the tunnel                        |
| `delta done with non-2xx (HTTP 500) — falling back to direct upload`  | Commit rejected server-side — chunk verification, quota, name conflict, etc.                              | Server logs; look for `delta_upload.rejected` audit line with a `reason` field                                                 |
| `worker: commit HTTP 507`                                             | Storage quota exceeded                                                                                    | User needs to free space or admin needs to increase quota                                                                      |

The generic pattern for user reports: ask them to open DevTools →
Console tab (filter: `oxi:upload`), run `oxi.setLogLevel('oxi:upload',
'debug')`, retry the failing upload, and share the output. The last
line before the "Upload failed" toast names the actual failure.

## Message bus — probe + state

The real-time bus multiplexes topic subscriptions, binary collab
frames, and JSON-RPC calls over a single `/api/rt/ws`. Depending on
browser support and server config, that WebSocket is either one per
tab (`PerTabMessageBusClient`) or one per BROWSER
(`SharedWorkerBridge`), with the shared variant backed by a
SharedWorker that outlives any single tab's lifecycle. The
`checkBusChain()` helper + namespaced logs are the first stop when
anything "live" stops updating.

### `oxi.checkBusChain()` — one-shot end-to-end probe

Round-trips a `rt.ping` through the full chain and returns a
structured snapshot:

```js
await oxi.checkBusChain()
// {
//   ok: true,
//   impl: 'SharedWorkerBridge',
//   state: 'connected',
//   workerVersion: 'dev-1792800537',
//   latencyMs: 7
// }
```

Three fields discriminate the shape of a failure:

| Field | What it tells you |
| --- | --- |
| `impl` | `SharedWorkerBridge` → shared mode is live. `PerTabMessageBusClient` → fallback engaged (Safari < 16.4, enterprise CSP `SharedWorkerAllowed=false`, iOS PWA quirks, handshake timeout, `?shared_worker=0` URL override, or `OXICLOUD_ENABLE_SHARED_WORKER_BUS=false` on the server). |
| `state` | `connected` (healthy) \| `connecting` (handshake / ticket in flight) \| `disconnected` (reconnect scheduled) \| `unavailable` (circuit breaker tripped) \| `idle` (no subscribe yet) |
| `latencyMs` | Full-chain round-trip in ms. Numbers in the single digits are normal on localhost; typical production is 10–50 ms. |

On a broken chain the helper **doesn't throw** — easier to read in
DevTools:

```js
await oxi.checkBusChain()
// {
//   ok: false,
//   impl: 'SharedWorkerBridge',
//   state: 'unavailable',
//   workerVersion: 'dev-...',
//   latencyMs: null,
//   error: { code: -32603, message: 'not_connected' }
// }
```

`error.message` is a stable key:
- `'not_connected'` — worker's WS is down (circuit tripped OR server unreachable).
- `'ws_closed'` — the WS dropped mid-ping (transient; retry).
- Anything else — the server returned an error with its own `code`.

### Connection states

| State | Meaning | Recovery path |
| --- | --- | --- |
| `idle` | Nothing has subscribed yet; WS not opened. | A `.subscribe(...)` kicks the connect flow. |
| `connecting` | Ticket fetch + WS upgrade in flight. | Expected transient. |
| `connected` | WS open and acked. | — |
| `disconnected` | WS dropped; backoff timer running. | Automatic — jittered exponential backoff (250 ms doubling to a 30 s cap). |
| `unavailable` | Circuit breaker tripped (10 consecutive failures). | **User action required**: refresh the page, OR `window.messageBus.reconnect()`. In shared mode, the worker auto-recovers on new port join (page refresh in any tab works). |

### Shared-mode-specific wrinkles

**The SharedWorker outlives page refresh.** State is held in the
worker, not the tab. A tab refresh does NOT reset the circuit
breaker on its own — the fix landed as auto-recovery on new port
join: when a port attaches to a worker in `unavailable` state, the
worker calls `transport.reconnect()` to re-arm.

**The worker script is cached by the browser.** Updating
`frontend/src/lib/message-bus/worker.ts` + rebuilding does NOT
reload the worker in a running browser. The script only refreshes
when every port disconnects. Three ways to force a restart during
development:

1. `chrome://inspect/#shared-workers` → find `oxi-bus` →
   **Terminate**. Next tab load spawns a fresh worker.
2. Close every tab of the site; reopen one.
3. DevTools → Application → Storage → **Clear site data** (nuclear).

**`workerVersion` as a staleness signal.** `oxi.checkBusChain()`
reports the worker's build identity. If an operator deployed
updates but a user's tab still reports an older `workerVersion`,
the user is still talking to a stale worker from before the
deploy; a tab close is required.

### Fallback detection reasons

When shared mode isn't available, the factory emits an
`oxi:message-bus` info line naming the specific reason:

| Reason | Trigger |
| --- | --- |
| `typeof_undefined` | Browser has no `SharedWorker` API (Safari < 16.4, some sandboxed contexts). |
| `construct_threw` | `new SharedWorker(...)` threw at construction — enterprise CSP `SharedWorkerAllowed=false`, sandboxed iframe without `allow-same-origin`. |
| `handshake_timeout` | Worker script loaded but didn't ack `hello` within 1 s — 404, MIME mismatch, iOS PWA storage sandboxing. Possible network block too (CSP `worker-src` or `connect-src` disallowed). |

Operators can grep these in client-side bug reports — `jsonpath
"$.reason"` style — to count how many users fell back vs ran shared
mode.

### Log signatures by scenario

Enable both namespaces when triaging:

```js
oxi.setLogLevel('oxi:message-bus', 'debug')
oxi.setLogLevel('oxi:message-bus:worker', 'debug')
```

Expected sequences:

**Healthy startup (shared mode)**:
```
[oxi:message-bus] info: SharedWorker handshake complete — shared bus is live
                       { workerVersion: 'dev-<ts>' }
[oxi:message-bus:worker] debug: connected
[oxi:message-bus:worker] debug: stable connection — reconnect counters reset
```

**Server stop + restart (within the 2-min circuit-breaker budget)**:
```
[oxi:message-bus:worker] debug: ws closed { code: 1006, reason: 'abnormal' }
[oxi:message-bus:worker] warn:  need-ticket timed out { nonce: 1 }    // after 5 s
[oxi:message-bus:worker] warn:  need-ticket timed out { nonce: 2 }
...                                                                     // on recovery:
[oxi:message-bus:worker] debug: connected
[oxi:message-bus:worker] debug: stable connection — reconnect counters reset
```

**Circuit tripped (server down past the 10-failure budget, ~2 min)**:
```
[oxi:message-bus:worker] warn:  need-ticket timed out { nonce: 10 }
                                                                        // state → 'unavailable'
                                                                        // UI banner: "Server unreachable"
                                                                        // After user refresh:
                                                                        // worker auto-reconnect:
[oxi:message-bus:worker] debug: connected
```

**Fallback engaged**:
```
[oxi:message-bus] info: SharedWorker not available — falling back to per-tab bus
                       { reason: 'typeof_undefined' }
```

### Common failure signatures — message bus

| Log line | Meaning | What to try |
| --- | --- | --- |
| `SharedWorker handshake timed out — falling back to per-tab bus` | Worker script failed to load or ack `hello` within 1 s. | Check Network tab for the worker URL (`_app/immutable/workers/worker-<hash>.js`); check CSP `worker-src`. If the worker loaded but didn't ack, inspect `chrome://inspect/#shared-workers` for errors. |
| `SharedWorker construction threw` | CSP block or sandboxed iframe. | `reason.err` names the throw; typically `SecurityError`. Operator-side CSP tweak if enterprise policy; cannot fix client-side. |
| `need-ticket timed out { nonce }` | Tab's ticket POST failed (server down OR 401-with-failed-refresh). | Check network tab for `POST /api/rt/ticket` — status 0 is network error, 401 is auth. |
| `ticket fetch failed — worker will time out on need-ticket` | Bridge's ticket POST threw. | Same as above; look at the `err` field for the cause. |
| `incoming parse failed` | A server frame wasn't valid JSON-RPC. | Server-side logs; grep for the WS session id. |
| `subscribe failed { topic, error }` | Server denied the subscribe. | `error.code` + `error.message` name the denial — `TOPIC_FORBIDDEN`, `SUB_LIMIT`, etc. |

### Admin-side counterpart

The server exposes `GET /api/admin/settings → active_ws_sessions`
— one WS per browser in shared mode, N per browser in per-tab
mode. Mismatch between expected shape and observed count is a
strong signal:

- `active_ws_sessions` = tabs → every user is in per-tab mode
  (shared mode isn't engaging anywhere → check
  `OXICLOUD_ENABLE_SHARED_WORKER_BUS` or the browser matrix).
- `active_ws_sessions` = browsers (lower than tabs) → shared mode
  working.
- `active_ws_sessions` = 0 while users are editing → every
  session's circuit tripped (server was down past 2 min OR
  endpoint misconfig).

## Interrupted uploads

Two coordinated behaviours help users recover from an accidental page
reload during an upload (details in
`frontend/src/lib/upload/interruption.ts`):

### `beforeunload` guard

While any upload is in flight, the browser prompts *"Leave site?
Changes may not be saved"* on refresh / tab close. Deliberate leave
(user clicks Leave) proceeds; accidental Cmd-R is cancelled.

Installed lazily: the listener is added when the first batch acquires
the guard, removed when the last batch releases it. No cost outside
active uploads.

### sessionStorage register

Every `uploadBatch` writes a record to
`sessionStorage['oxi:upload:interrupted']` while it runs and removes
it on completion. If a reload survives them, the root layout's
`onMount` reads and clears the register, then toasts:

> Upload interrupted: `<name>`. Re-drop to resume — already-uploaded
> chunks are reused.

Chunks landed in `storage.file_blobs` before the reload persist on the
server. A re-drop lets the delta worker's `negotiate` stage discover
them as "already on server", so a resumed upload only transfers what
was in flight when the reload hit — not everything from scratch.

`sessionStorage` (not `localStorage`) on purpose: entries clear when
the tab closes entirely, so a user who closed the tab hours ago
doesn't get nagged on reopen.

## Code entry points

| Concern                             | File                                                            |
| ----------------------------------- | --------------------------------------------------------------- |
| `oxi.*` global attach + setters     | `frontend/src/hooks.client.ts`                                  |
| Delta orchestrator (main thread)    | `frontend/src/lib/api/endpoints/deltaUpload.ts`                 |
| Delta worker (CDC + BLAKE3 + PUTs)  | `frontend/static/workers/deltaWorker.js`                        |
| Direct upload (fallback path)       | `frontend/src/lib/api/endpoints/files.ts::uploadFileWithProgress` |
| Interrupted-upload registry         | `frontend/src/lib/upload/interruption.ts`                       |
| Reload-time toast wiring            | `frontend/src/routes/+layout.svelte` (`onMount`)                |
| Message-bus factory + bridge        | `frontend/src/lib/message-bus/client.svelte.ts` + `shared-worker-bridge.svelte.ts` |
| Message-bus WS transport            | `frontend/src/lib/message-bus/ws-bus-transport.ts`              |
| SharedWorker entry + routing        | `frontend/src/lib/message-bus/worker.ts` + `worker-core.ts`     |
| SharedWorker detection + handshake  | `frontend/src/lib/message-bus/detect.ts`                        |

Server-side counterpart for delta upload:
[Delta upload protocol](../delta-upload-protocol.md).

Message-bus design + rollout:
[SharedWorker-backed message bus](../plan/message-bus-via-shared-worker.md).
