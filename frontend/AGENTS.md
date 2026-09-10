# AGENTS.md — Frontend

Complements the repo-root `/AGENTS.md`. Not shipped (adapter-static
copies only `frontend/static/`).

## localStorage keys

Prefix `oxi-`, kebab-case separators. Example: `oxi-view-mode`.
Enforced by `$lib/utils/localStoragePrefs::wipeAppKeys()` which sweeps
every `oxi-*` key on user-account switches — any other prefix leaks the
previous user's state into the new one.

## Logging — `loglevel` with `oxi:*` namespaces

**Never use bare `console.debug/info/warn/error` in `$lib` or route code.**
Route through the shared [`loglevel`](https://github.com/pimterry/loglevel)
logger so users and support can dial verbosity per subsystem from the
browser console without a redeploy.

```ts
import log from 'loglevel';

const bus = log.getLogger('oxi:message-bus');
bus.debug('subscribed', { topic });
bus.warn('reconnect scheduled', { attempt, backoffMs });
bus.error('unexpected frame', { raw });
```

Convention:

- **Namespace = `oxi:<subsystem>`** in kebab-case. One namespace per
  subsystem/module boundary — e.g. `oxi:upload` (delta + direct
  uploader), `oxi:message-bus` (WS client + `useTopic`). Do not create
  finer-grained per-file namespaces; users tune subsystems, not files.
- **Level is user-controlled** via the DevTools helper installed in
  `src/hooks.client.ts`:
  ```js
  oxi.setLogLevel('oxi:message-bus', 'debug');
  oxi.listLogLevels();
  ```
  Choices persist to `localStorage['loglevel:<namespace>']`. Default is
  loglevel's `warn` — production stays quiet unless the user opts in.
- **Add every new namespace to the DevTools comment block** in
  `hooks.client.ts` (the `Log levels — namespaces used today: …` line)
  so users have a discoverable list.
- **No `console.log` at all** — Stylelint/ESLint don't flag it, but the
  codebase convention does. `console.error` is only acceptable in
  boot-time paths (`hooks.client.ts`, generator scripts, worker
  bootstraps) where the shared logger isn't reachable yet.
- **Workers can't `import log` from a static path** — see
  `lib/api/endpoints/deltaUpload.ts`: the worker `postMessage`s a
  `{type: 'log', level, msg, extra}` envelope and the main thread relays
  it through the shared logger. Mirror this pattern for any new worker.

## Message bus naming

The realtime channel is the **message bus** everywhere — backend port
`MessageBus`, plan doc `docs/plan/message-bus.md`, generated DTOs under
`$lib/generated/message-bus/`, FE store/composables named accordingly.
Only two things keep the older `rt`/`Rt` shorthand, and both for wire-
protocol reasons:

- **JSON-RPC method prefix** — `rt.subscribe`, `rt.event`, `rt.revoked`,
  `rt.ping`, `rt.error`. The prefix is opaque wire vocabulary and does
  not have to expand to "realtime"; treat it as a short namespace tag
  reserved for message-bus methods.
- **Generated type names** — `RtSubscribeParams`, `RtEventBody`, etc.
  Modelina keys off the AsyncAPI schema names, which mirror the JSON-RPC
  method names.

When adding FE code around the bus, use `message-bus` in file names,
store names, and logger namespaces:

- Store: `$lib/stores/message-bus.svelte.ts`
- Composables: `$lib/composables/useTopic.svelte.ts` (topic-generic — no
  bus name in the file)
- Logger namespace: `oxi:message-bus`
- localStorage keys (if any): `oxi-message-bus-*`
