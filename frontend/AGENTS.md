# AGENTS.md — Frontend

Complements the repo-root `/AGENTS.md`. Not shipped (adapter-static
copies only `frontend/static/`).

## Components — read the catalogue before writing UI

**`docs/architecture/components.md` lists every reusable component**, with
what it is and why it exists. Read it before building any UI, and add a row
when you build something reusable.

This is not bookkeeping. The recurring defect in this frontend is not a
missing component, it is a _second_ one: three right-click menus, two boot
splashes, three hand-rolled swipe-to-delete rows — each written because
nobody knew the first existed, and each then drifting until the same bug had
to be fixed in three places. A component absent from that list gets
rewritten by the next person.

- **Reuse beats writing.** Something 80% right takes a prop, not a sibling.
  One component with a flag beats two that drift.
- **Generalise on the SECOND caller.** When a second surface needs the same
  behaviour, move it into `lib/components/` then — not once a third copy has
  grown its own bugs.
- **Open every component with why it exists**, not just what it does: the
  duplication it prevents, the bug it was extracted after. That comment is
  what makes the next reader reuse instead of rewrite.

Known homes for shared behaviour, before you add a component at all:
`lib/composables/` for rune logic, `lib/stores/` for global state,
`lib/utils/` for DOM and data helpers.

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
`$lib/api/generated/asyncapi/`, FE store/composables named accordingly.
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

- Subsystem dir: `$lib/message-bus/` — reactive client (`client.svelte.ts`,
  a `MessageBusClient` singleton owning the WebSocket, refcounted topic
  subs, and reconnect), frame builders (`frames.ts`), error-code
  constants (`error-codes.ts`). Mirrors the `$lib/auth/` and
  `$lib/upload/` subsystem-dir pattern rather than living in
  `$lib/stores/` — the client is subsystem-scoped plumbing that only
  the message-bus composables reach for, not a global reactive store
  read from route decisions like `session`.
- Composables: `$lib/composables/useTopic.svelte.ts` (topic-generic — no
  bus name in the file), `$lib/composables/useFolderTopic.svelte.ts`
  (folder-view sugar with per-verb handlers).
- Logger namespace: `oxi:message-bus`.
- localStorage keys (if any): `oxi-message-bus-*`.

## Dialog error UX

**Mutating actions in a `promptDialog` MUST pass `action` instead of
awaiting the dialog's return value and running the mutation outside.**

Without `action`, the dialog resolves and closes before the mutation
runs; a backend error (409 AlreadyExists, 507 QuotaExceeded, …) then
surfaces as a corner toast and the user has lost their typed value. The
dialog store already supports the better flow — it runs the `action`
inside its own try/catch, keeps the modal open on reject with an
inline error, and resolves only on success.

Preferred shape:

```ts
await promptDialog({
	title: t('common.rename', 'Rename'),
	defaultValue: item.name,
	selectOnOpen: 'name', // file/folder: preserve extension on retype
	confirmText: t('common.rename', 'Rename'),
	action: async (name) => {
		if (!name || name === item.name) return; // normal close, no-op
		await renameFile(item.id, name); // throw → inline error
		await reload();
	}
});
```

Discouraged shape (loses typed value on error):

```ts
const name = await promptDialog({ title: '…', defaultValue: item.name });
if (!name) return;
try {
	await renameFile(item.id, name);
} catch (e) {
	errorToast(e); // toast appears, modal is already gone
}
```

Exceptions where the old pattern is correct:

- **Multi-step flows** (prompt → confirm → mutate). Dialogs are a
  single-at-a-time queue, so firing a second dialog inside `action`
  deadlocks: the queued dialog can't open while the first is still
  current. `files/[...path]/+page.svelte::onNewDocument` is the
  canonical example (prompt → non-text-ext confirm → upload). Keep
  the old pattern; errors in the final mutation step toast.
- **Confirm-only dialogs with no form state to preserve** — a plain
  `confirmDialog` of "Delete X?" can safely toast on failure. The user
  lost nothing by the dialog closing. Still OK to use `action` for
  consistency, but not required.

The DialogHost renders inline errors with `role="alert"` and an
`aria-invalid`/`aria-describedby` pairing on the input, clears the
error on `oninput`, and refocuses the input after a failed submit so
the keyboard flow stays continuous.

**Shared error-state classes** live in `lib/styles/base/forms.css`:

- `.form-error` — the inline message paragraph (red via
  `--color-error-text`, dark-mode aware).
- `.form-input--invalid` — the matching red border on the input
  while the error is set.

Use these in any new modal rather than coining a per-component
error class — one place to tune contrast, dark-mode, and spacing.
Note `--color-danger-text` (one letter apart) is `#ffffff` white —
meant for text ON a red button background; using it on a dialog
surface renders invisible text. The right token is
`--color-error-text`.

**Custom-content modals (not using `promptDialog`)** — e.g.
`ShareDialog`, `MoveDialog`, `DrivePoliciesModal`, `QuotaEditor` —
should follow the same shape: local `error`/`busy` state, clear on
input, disable submit while busy, render inline rather than toast. A
shared composable at `$lib/composables/useAsyncAction.svelte.ts`
codifies the pattern — reach for it when adding a new custom modal
instead of hand-rolling the state machine.

Rename flows also pass `selectOnOpen: 'name'` (file/folder — selects
the stem before the last dot, preserving the extension) or
`selectOnOpen: true` (names without extension semantics — people,
groups, playlists — where `'name'` would misfire on dots in the
middle of a value).
