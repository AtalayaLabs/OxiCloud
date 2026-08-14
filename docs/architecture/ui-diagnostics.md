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

Server-side counterpart for delta upload:
[Delta upload protocol](../delta-upload-protocol.md).
