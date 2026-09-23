<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	// Collaborative markdown editor.
	//
	// Mounts CodeMirror 6 with markdown syntax + the `y-codemirror.next`
	// binding to a `Y.Text` + `Awareness` sourced from a `CollabDoc`.
	// One editor per file id; wraps its own lifecycle so callers just
	// render `<CollabEditor fileId={id} />` and unmount to teardown.
	// Peer cursors + selections render natively via the awareness
	// registry — each user's caret gets their `username` label and a
	// deterministic colour derived from their id.
	//
	// Read-only viewers: the subscribe ack for `collab:{fileId}` carries
	// the caller's `Permission::Update` as `capabilities.can_write`.
	// When false, CodeMirror is mounted with `EditorState.readOnly.of(true)`
	// via a live-reconfigurable `Compartment` (so a hypothetical mid-
	// session capability upgrade could flip it without a full remount).
	// The compartment defaults to read-only until the ack lands —
	// fail-closed, matching the server's own gate.
	//
	// Explicit-flush hooks: `visibilitychange → hidden`, `pagehide`,
	// and $effect cleanup all fire `rt.collab_flush` to tighten
	// worst-case staleness on the file's blob from ≤60 s (the
	// debouncer's max) down to sub-second latency. See the effect
	// body below for the wiring.

	// CodeMirror runtime lives entirely inside the mount effect —
	// dynamic-imported below alongside the language grammar. Only
	// TYPES are pulled in at module scope: type imports are erased
	// by the compiler, so the SSR bundle stays free of runtime
	// references (which its Rollup pass would otherwise flag as
	// unused since `$effect` bodies are stripped from SSR output).
	// Keeping the types top-level lets the module-scope `let view`
	// / `let readOnlyCompartment` declarations name their instance
	// types without a widening `unknown`.
	import type { Compartment, Extension } from '@codemirror/state';
	import type { EditorView } from '@codemirror/view';
	// `HighlightStyle` + `tags` are used at MODULE TOP LEVEL below
	// (see `collabHighlight`) and therefore must stay static. They're
	// also tiny compared to `@codemirror/view` + `y-codemirror.next`;
	// pulling them synchronously costs a handful of KB and keeps the
	// syntax palette declaration inline where the theme rules live.
	import { HighlightStyle } from '@codemirror/language';
	import { tags as t } from '@lezer/highlight';

	/** Theme-aware syntax highlight style.
	 *
	 *  Every `color` reads from OxiCloud's design-system tokens
	 *  defined in `lib/styles/base/variables.css` (`--syntax-*`).
	 *  Those tokens are `light-dark(...)` values, so the editor's
	 *  colours flip automatically with the surrounding
	 *  `color-scheme` — no compartment / reconfigure plumbing. The
	 *  central palette also means theme designers change syntax
	 *  colours in one place, not inside a Svelte component. */
	const collabHighlight = HighlightStyle.define([
		// Keywords: `if`, `return`, `let`, `fn`, `import`, etc.
		{ tag: t.keyword, color: 'var(--syntax-keyword)' },
		{ tag: t.controlKeyword, color: 'var(--syntax-keyword)' },
		{ tag: t.moduleKeyword, color: 'var(--syntax-keyword)' },
		// Strings + character literals.
		{ tag: t.string, color: 'var(--syntax-string)' },
		{ tag: t.special(t.string), color: 'var(--syntax-string)' },
		{ tag: t.character, color: 'var(--syntax-string)' },
		{ tag: t.regexp, color: 'var(--syntax-regexp)' },
		// Comments — italic + muted.
		{ tag: t.comment, color: 'var(--syntax-comment)', fontStyle: 'italic' },
		{ tag: t.lineComment, color: 'var(--syntax-comment)', fontStyle: 'italic' },
		{ tag: t.blockComment, color: 'var(--syntax-comment)', fontStyle: 'italic' },
		{ tag: t.docComment, color: 'var(--syntax-comment)', fontStyle: 'italic' },
		// Numbers, booleans, null.
		{ tag: t.number, color: 'var(--syntax-number)' },
		{ tag: t.bool, color: 'var(--syntax-number)' },
		{ tag: t.null, color: 'var(--syntax-number)' },
		// Function definitions + calls.
		{ tag: t.function(t.variableName), color: 'var(--syntax-function)' },
		{ tag: t.function(t.propertyName), color: 'var(--syntax-function)' },
		{ tag: t.definition(t.function(t.variableName)), color: 'var(--syntax-function)' },
		// Types, classes, tags.
		{ tag: t.typeName, color: 'var(--syntax-type)' },
		{ tag: t.className, color: 'var(--syntax-type)' },
		{ tag: t.tagName, color: 'var(--syntax-type)' },
		// Property names + attributes.
		{ tag: t.propertyName, color: 'var(--syntax-property)' },
		{ tag: t.attributeName, color: 'var(--syntax-property)' },
		// Markdown headings + emphasis + link text.
		{ tag: t.heading, color: 'var(--syntax-heading)', fontWeight: '600' },
		{ tag: t.strong, fontWeight: '600' },
		{ tag: t.emphasis, fontStyle: 'italic' },
		{ tag: t.link, color: 'var(--syntax-link)', textDecoration: 'underline' },
		{ tag: t.url, color: 'var(--syntax-link)' },
		// Muted structural bits.
		{ tag: t.meta, color: 'var(--syntax-meta)' },
		{ tag: t.punctuation, color: 'var(--syntax-meta)' },
		{ tag: t.operator, color: 'var(--syntax-operator)' },
		{ tag: t.escape, color: 'var(--syntax-escape)' },
		// Constants (SCREAMING_SNAKE style, or true/false/null in some grammars).
		{ tag: t.constant(t.variableName), color: 'var(--syntax-number)' },
		{ tag: t.standard(t.variableName), color: 'var(--syntax-keyword)' }
	]);
	import { onDestroy, untrack } from 'svelte';

	import { CollabDoc, type SyncState } from '$lib/collab/collabDoc';
	import { messageBus } from '$lib/message-bus/client.svelte';
	import { session } from '$lib/stores/session.svelte';

	/** Dynamically resolve a CodeMirror language extension from the
	 *  file's extension. Each `import()` call is code-split by Vite
	 *  into its own chunk — opening a `.md` fetches only the markdown
	 *  grammar; opening a `.rs` fetches only the rust grammar. The
	 *  editor mount `await`s this before creating the EditorState.
	 *
	 *  Anchor rule from feedback: "at least markdown". Files without
	 *  a known language extension fall through to `markdown()` — a
	 *  reasonable superset for prose, does no harm on unknown text
	 *  (the alternative is zero highlighting, which reads as "editor
	 *  is broken").
	 *
	 *  Coverage tracks the FE's `TEXTY_EXT_RE` gate in FileViewer.
	 *  Adding an extension to the gate should add a case here too. */
	async function languageFor(filename: string): Promise<Extension> {
		const m = filename.toLowerCase().match(/\.([a-z0-9]+)$/);
		const ext = m?.[1] ?? '';
		switch (ext) {
			case 'md':
			case 'markdown':
				return (await import('@codemirror/lang-markdown')).markdown();
			case 'js':
			case 'jsx':
			case 'mjs':
			case 'cjs':
			case 'ts':
			case 'tsx':
				return (await import('@codemirror/lang-javascript')).javascript({
					jsx: ext.includes('x'),
					typescript: ext.startsWith('t')
				});
			case 'py':
			case 'pyw':
				return (await import('@codemirror/lang-python')).python();
			case 'rs':
				return (await import('@codemirror/lang-rust')).rust();
			case 'html':
			case 'htm':
			case 'svelte':
			case 'vue':
				return (await import('@codemirror/lang-html')).html();
			case 'css':
			case 'scss':
			case 'sass':
			case 'less':
				return (await import('@codemirror/lang-css')).css();
			case 'json':
				return (await import('@codemirror/lang-json')).json();
			case 'yaml':
			case 'yml':
				return (await import('@codemirror/lang-yaml')).yaml();
			case 'xml':
				return (await import('@codemirror/lang-xml')).xml();
			case 'sql':
				return (await import('@codemirror/lang-sql')).sql();
			// Legacy CodeMirror modes for languages without a
			// first-party `@codemirror/lang-*` package. Each is a
			// StreamLanguage tokenizer — coarser than a Lezer grammar
			// but plenty for keyword / string / comment coloring. All
			// live in one npm package (`@codemirror/legacy-modes`),
			// so Rollup should ideally split each mode into its own
			// chunk; dynamic-import per-language keeps that a
			// possibility even if it hasn't materialised today.
			case 'sh':
			case 'bash':
			case 'zsh':
			case 'fish': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { shell } = await import('@codemirror/legacy-modes/mode/shell');
				return StreamLanguage.define(shell);
			}
			case 'go': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { go } = await import('@codemirror/legacy-modes/mode/go');
				return StreamLanguage.define(go);
			}
			case 'java': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { java } = await import('@codemirror/legacy-modes/mode/clike');
				return StreamLanguage.define(java);
			}
			case 'kt':
			case 'kts': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { kotlin } = await import('@codemirror/legacy-modes/mode/clike');
				return StreamLanguage.define(kotlin);
			}
			case 'scala': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { scala } = await import('@codemirror/legacy-modes/mode/clike');
				return StreamLanguage.define(scala);
			}
			case 'c':
			case 'h': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { c } = await import('@codemirror/legacy-modes/mode/clike');
				return StreamLanguage.define(c);
			}
			case 'cpp':
			case 'hpp':
			case 'cc':
			case 'cxx': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { cpp } = await import('@codemirror/legacy-modes/mode/clike');
				return StreamLanguage.define(cpp);
			}
			case 'cs': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { csharp } = await import('@codemirror/legacy-modes/mode/clike');
				return StreamLanguage.define(csharp);
			}
			case 'rb': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { ruby } = await import('@codemirror/legacy-modes/mode/ruby');
				return StreamLanguage.define(ruby);
			}
			case 'swift': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { swift } = await import('@codemirror/legacy-modes/mode/swift');
				return StreamLanguage.define(swift);
			}
			case 'r': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { r } = await import('@codemirror/legacy-modes/mode/r');
				return StreamLanguage.define(r);
			}
			case 'lua': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { lua } = await import('@codemirror/legacy-modes/mode/lua');
				return StreamLanguage.define(lua);
			}
			case 'pl':
			case 'pm': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { perl } = await import('@codemirror/legacy-modes/mode/perl');
				return StreamLanguage.define(perl);
			}
			case 'toml': {
				const { StreamLanguage } = await import('@codemirror/language');
				const { toml } = await import('@codemirror/legacy-modes/mode/toml');
				return StreamLanguage.define(toml);
			}
			case 'ini':
			case 'cfg':
			case 'conf': {
				// No dedicated INI grammar; reuse `properties` — java
				// properties is a `key = value` grammar close enough
				// for INI / most config files.
				const { StreamLanguage } = await import('@codemirror/language');
				const { properties } = await import('@codemirror/legacy-modes/mode/properties');
				return StreamLanguage.define(properties);
			}
			default:
				return (await import('@codemirror/lang-markdown')).markdown();
		}
	}

	/** 12-slot hex palette for peer cursors. Chosen to stay readable on
	 *  both light and dark backgrounds — saturation and lightness are
	 *  balanced across the wheel. Hex strings are required by
	 *  `y-codemirror.next`, which appends `33` (20% alpha) to build the
	 *  selection background — HSL strings break that concatenation and
	 *  the selection paints nothing. */
	const CURSOR_PALETTE = [
		'#e11d48', // rose
		'#f97316', // orange
		'#eab308', // amber
		'#22c55e', // green
		'#14b8a6', // teal
		'#06b6d4', // cyan
		'#3b82f6', // blue
		'#6366f1', // indigo
		'#8b5cf6', // violet
		'#a855f7', // purple
		'#ec4899', // pink
		'#84cc16' //  lime
	] as const;

	/** Deterministic peer-cursor colour derived from a user id.
	 *  Returns `{ color, colorLight }` — the two fields
	 *  `y-codemirror.next` reads from `awareness.user.*`. Same user id
	 *  → same colour across sessions and machines, so peers recognise
	 *  each other visually. */
	function userColor(userId: string): { color: string; colorLight: string } {
		let sum = 0;
		for (let i = 0; i < userId.length; i++) sum = (sum + userId.charCodeAt(i)) & 0xffff;
		const color = CURSOR_PALETTE[sum % CURSOR_PALETTE.length];
		// Selection background: same hex + `33` = 20% alpha. Matches
		// what y-codemirror.next's own fallback would compute; we set
		// it explicitly so a library update that changes the fallback
		// doesn't silently drift.
		return { color, colorLight: `${color}33` };
	}

	interface Props {
		/** Dashed UUID of the file to edit. */
		fileId: string;
		/** Filename with extension — drives the CodeMirror language
		 *  binding (`languageFor` matches on the trailing extension).
		 *  Optional so the standalone `/collab/[fileId]` test route,
		 *  which doesn't have the filename in hand, can mount too; a
		 *  missing filename falls back to markdown highlighting (a
		 *  safe superset for prose, does no harm on code). */
		filename?: string;
		/** Actions rendered on the left of the status row — the host
		 *  decides what belongs there (download, open in a window,
		 *  hand off to the office editor). Keeping them in this row
		 *  rather than in a bar of their own means the editor chrome
		 *  stays two bars deep, like the office editor's. */
		toolbar?: Snippet;
	}
	let { fileId, filename, toolbar }: Props = $props();

	/** Who else has this file open, from the awareness registry. */
	interface Peer {
		id: number;
		name: string;
		color: string;
	}
	let peers = $state<Peer[]>([]);

	/** Up to two letters for the avatar: initials of a two-word name,
	 *  otherwise the first two characters. */
	function initials(name: string): string {
		const parts = name.trim().split(/\s+/).filter(Boolean);
		if (parts.length > 1) return (parts[0][0] + parts[1][0]).toUpperCase();
		return (parts[0] ?? '?').slice(0, 2).toUpperCase();
	}

	let syncState = $state<SyncState>('idle');
	let container: HTMLDivElement | undefined = $state();
	/** Does the server say this caller may write? Fail-closed default:
	 *  flipped only by a subscribe ack carrying `can_write: true`.
	 *  Authorization only — see `readOnly` for what the editor
	 *  actually enforces. */
	let canWrite = $state(false);

	/** What the editor enforces. Permission is necessary but NOT
	 *  sufficient: the doc must also have finished syncing.
	 *
	 *  Until sync-step-2 lands, the local `Y.Doc` is legitimately
	 *  empty — indistinguishable, from the editor's point of view,
	 *  from a genuinely empty file. Typing into it would produce
	 *  CRDT inserts positioned against that empty doc, which the
	 *  server then merges into the real content: the user's text
	 *  lands at the top of a file they could not see, and the flush
	 *  writes the result to the blob. Nothing is deleted (an empty
	 *  local doc has no text to author a delete for, so the file
	 *  cannot be wiped this way) but the file is still corrupted.
	 *
	 *  Gating on `synced` closes that window by construction rather
	 *  than relying on the sync handshake never failing. See
	 *  `readOnlyCompartment` for the live wire-up. */
	const readOnly = $derived(!canWrite || syncState !== 'synced');

	/** Effective state shown to the user. The bus's circuit-tripped
	 *  `unavailable` outranks any per-doc state — no point telling
	 *  the user "syncing…" when the underlying transport has given
	 *  up. Denied stays terminal (that's already a permanent state). */
	const displayState = $derived<SyncState>(
		messageBus.state === 'unavailable' && syncState !== 'denied' ? 'unavailable' : syncState
	);

	/** Tooltip for the state badge — the visible cue is the icon. */
	const statusLabel = $derived(
		displayState === 'idle'
			? 'Ready'
			: displayState === 'syncing'
				? 'Syncing…'
				: displayState === 'synced'
					? 'Synced'
					: 'Disconnected — changes stay local until the connection returns'
	);
	const statusIcon = $derived(
		displayState === 'synced'
			? 'check-circle'
			: displayState === 'syncing'
				? 'circle-notch'
				: displayState === 'disconnected'
					? 'exclamation-triangle'
					: 'clock'
	);

	let collab: CollabDoc | undefined;
	let view: EditorView | undefined;
	/** CodeMirror `Compartment` that wraps the `EditorState.readOnly`
	 *  extension so the editor can flip between edit and read-only
	 *  modes without a full state rebuild — `view.dispatch({effects:
	 *  compartment.reconfigure(...)})`. Rebuilt per mount because a
	 *  `Compartment` is bound to one `EditorState`. */
	let readOnlyCompartment: Compartment | undefined;
	/** Live reconfigure hook set by the mount effect's async IIFE once
	 *  the CodeMirror runtime has been dynamic-imported. Flips the
	 *  readOnly compartment without needing `EditorState` in scope
	 *  outside the IIFE (keeping the SSR bundle free of CodeMirror
	 *  runtime references). `undefined` before the runtime loads:
	 *  `readOnly`'s current value is picked up by construction when
	 *  the IIFE finally builds the EditorState. */
	let reconfigureReadOnly: ((v: boolean) => void) | undefined;

	// Push `readOnly` into the live CodeMirror compartment whenever it
	// moves — on a capability change (subscribe ack, mid-session
	// demotion) AND on a sync-state change. The latter is what makes
	// the doc un-typeable until sync-step-2 has landed; see `readOnly`.
	//
	// A no-op until the runtime IIFE sets the hook, which is correct:
	// the EditorState it then builds reads `readOnly` directly, and it
	// calls the hook once itself to settle any value that moved while
	// the chunks were in flight.
	$effect(() => {
		const ro = readOnly;
		untrack(() => reconfigureReadOnly?.(ro));
	});

	// Mount effect: attach CodeMirror + CollabDoc when `container`
	// becomes available. Runs once per `fileId` change; the cleanup
	// tears down and $effect re-runs for a new file. This is the
	// canonical Svelte 5 pattern for imperative library integration.
	//
	// The mount work is async because the language grammar is fetched
	// via `import()` per file type — the wrapper IIFE runs it while
	// the effect's cleanup stays synchronous. A `cancelled` flag
	// guards against the rare case where the effect re-runs (fileId
	// prop changed) before the awaited import resolves — the stale
	// mount aborts without touching a container that now belongs to
	// a new fileId's mount.
	$effect(() => {
		if (!container) return;
		const currentFileId = fileId;
		const currentFilename = filename;
		const currentContainer = container;
		let cancelled = false;

		// Kick the CollabDoc synchronously — it doesn't need the
		// language grammar to start syncing, and the WS subscribe
		// benefits from firing as early as possible so the sync-step-2
		// diff arrives while we're still loading the grammar chunk.
		// Reset readonly to the fail-closed default on every mount —
		// a leftover `false` from a previous file would let a viewer
		// type into a new file for the sub-second window between
		// mount and the fresh subscribe ack.
		canWrite = false;

		collab = new CollabDoc({
			fileId: currentFileId,
			onSyncStateChange: (s) => {
				syncState = s;
			},
			onCapabilities: (caps) => {
				// Record the permission only. Pushing it into the live
				// CodeMirror compartment is the effect below's job —
				// `readOnly` now depends on `syncState` as well, and
				// that moves without any capability change, so a
				// reconfigure driven from this callback alone would
				// leave the editor writable on a doc that never synced.
				canWrite = caps.canWrite;
			}
		});
		collab.connect();

		// Publish local user info into the awareness registry so peers
		// can render this caret with a name + colour. Read the session
		// snapshot at mount time — if it's not loaded yet (edge case
		// during boot) we still publish a placeholder rather than
		// leaving the peer view unlabelled.
		const localUser = session.user;
		const palette = userColor(localUser?.id ?? currentFileId);
		collab.awareness.setLocalStateField('user', {
			name: localUser?.username ?? 'Anonymous',
			color: palette.color,
			colorLight: palette.colorLight
		});

		// Everyone else in the registry, for the avatars in the status
		// row. `change` fires on join, leave and every cursor move, so
		// the list stays current without polling.
		const awareness = collab.awareness;
		const refreshPeers = () => {
			peers = [...awareness.getStates().entries()]
				.filter(([id]) => id !== awareness.clientID)
				.map(([id, state]) => {
					const user = (state as { user?: { name?: string; color?: string } }).user;
					return {
						id,
						name: user?.name ?? 'Anonymous',
						color: user?.color ?? 'var(--color-text-muted)'
					};
				});
		};
		awareness.on('change', refreshPeers);
		refreshPeers();

		// Dynamic-import the CodeMirror runtime alongside the language
		// grammar. Everything the editor needs is a code-split chunk,
		// so a user who never opens a collab-editable file ships zero
		// CodeMirror bytes. Requested in parallel — the network fans
		// out but nothing awaits any one chunk before firing the next.
		// The status pill shows "Syncing…" during the load (fine UX
		// on cold cache; invisible on warm).
		const collabRef = collab;
		void (async () => {
			const [stateMod, viewMod, cmdsMod, langMod, yCollabMod, langExt] = await Promise.all([
				import('@codemirror/state'),
				import('@codemirror/view'),
				import('@codemirror/commands'),
				import('@codemirror/language'),
				import('y-codemirror.next'),
				// `languageFor` matches on the trailing extension in the
				// FILENAME — the UUID has none, which used to make every
				// file fall through to markdown. Prefer the passed
				// `filename`; the fileId is only a fallback for the
				// standalone test route.
				languageFor(currentFilename ?? currentFileId)
			]);
			if (cancelled) return;

			const { Compartment: CompartmentCtor, EditorState } = stateMod;
			const { EditorView: EditorViewCtor, keymap, lineNumbers } = viewMod;
			const { defaultKeymap, history, historyKeymap } = cmdsMod;
			const { syntaxHighlighting } = langMod;
			const { yCollab } = yCollabMod;

			// Fresh compartment per mount — it's bound to this
			// EditorState. Initial value tracks `readOnly` at this
			// instant; the `onCapabilities` callback reconfigures the
			// same compartment later when the ack arrives (if the mount
			// won the race with the ack) or immediately (if the ack
			// came first).
			readOnlyCompartment = new CompartmentCtor();

			const state = EditorState.create({
				// Seeded from the CRDT, not empty: sync-step-2 can land while
				// the chunks above load, and `yCollab` only applies updates
				// that arrive after it binds.
				doc: collabRef.yText().toString(),
				extensions: [
					lineNumbers(),
					history(),
					keymap.of([...defaultKeymap, ...historyKeymap]),
					langExt,
					readOnlyCompartment.of(EditorState.readOnly.of(readOnly)),
					// The language extension only produces a syntax
					// tree; `syntaxHighlighting` paints colours from
					// it. `collabHighlight` (see the definition above)
					// uses `light-dark(...)` for every colour so both
					// `data-color-scheme` states read cleanly from one
					// extension — no compartment / reconfigure
					// plumbing needed. `fallback: true` widens coverage
					// to language nodes the palette doesn't name.
					syntaxHighlighting(collabHighlight, { fallback: true }),
					// Pass the awareness registry so `y-codemirror.next`
					// renders peer cursors + selections with the `user`
					// field we just published (name + colour).
					yCollab(collabRef.yText(), collabRef.awareness)
				]
			});

			view = new EditorViewCtor({
				state,
				parent: currentContainer
			});

			// Now that the runtime + `view` + `readOnlyCompartment`
			// are all in scope, wire the reconfigure hook that
			// `onCapabilities` calls on late-arriving acks. Closes
			// over `EditorState` from the just-loaded `stateMod`; a
			// no-op after cleanup thanks to the `view` null-check.
			reconfigureReadOnly = (v: boolean) => {
				if (!view || !readOnlyCompartment) return;
				view.dispatch({
					effects: readOnlyCompartment.reconfigure(EditorState.readOnly.of(v))
				});
			};
			// The ack MAY have landed while we were awaiting the
			// runtime chunks. Fire once now with the current
			// `readOnly` value so the freshly-built state doesn't
			// visibly disagree with what the ack said.
			reconfigureReadOnly(readOnly);
		})();

		// Explicit-flush hooks. The debouncer's `debounce_max` (default
		// 60 s server-side) bounds worst-case staleness; these three
		// events tighten the common case to sub-second latency by asking
		// the server to flush NOW when the user is visibly done with the
		// tab. All three fire the same idempotent `rt.collab_flush` —
		// a clean actor short-circuits with `{ flushed: false }`, so
		// firing on every visibility toggle costs a round-trip at
		// worst.
		//
		//   * `visibilitychange → hidden` — the primary hook. Fires
		//     reliably across browsers on tab switch / minimise /
		//     screen lock. Also fires on tab close in most cases.
		//   * `pagehide` — belt for the tab-close path, especially on
		//     Safari where `visibilitychange` sometimes misses the
		//     final close.
		//   * `$effect` cleanup below — braces for programmatic
		//     unmount (route change, logout, feature toggle off).
		const flushIfLive = () => {
			// `collab` may already be undefined mid-cleanup — guard.
			if (!cancelled && collab) {
				void collab.flush();
			}
		};
		const onVisibility = () => {
			if (typeof document !== 'undefined' && document.visibilityState === 'hidden') {
				flushIfLive();
			}
		};
		const onPageHide = () => flushIfLive();
		if (typeof document !== 'undefined') {
			document.addEventListener('visibilitychange', onVisibility);
		}
		if (typeof window !== 'undefined') {
			window.addEventListener('pagehide', onPageHide);
		}

		return () => {
			cancelled = true;
			// Unmount flush BEFORE destroy — otherwise the WS is gone
			// by the time the flush fires and we lose the window.
			// Best-effort: `flush()` swallows errors internally so a
			// dead socket collapses to a debug log, not an unhandled
			// promise rejection.
			void collab?.flush();
			if (typeof document !== 'undefined') {
				document.removeEventListener('visibilitychange', onVisibility);
			}
			if (typeof window !== 'undefined') {
				window.removeEventListener('pagehide', onPageHide);
			}
			awareness.off('change', refreshPeers);
			peers = [];
			view?.destroy();
			view = undefined;
			collab?.destroy();
			collab = undefined;
			readOnlyCompartment = undefined;
			reconfigureReadOnly = undefined;
		};
	});

	// Extra defence for hot-module-reload / rare unmount paths where
	// $effect cleanup somehow doesn't fire — Svelte's onDestroy is a
	// no-op if $effect already tore down.
	onDestroy(() => {
		view?.destroy();
		collab?.destroy();
	});
</script>

<div class="collab-editor">
	{#if displayState === 'denied'}
		<!-- Terminal state — no editor. Anti-enum: message covers
		     "no Read grant" AND "unknown file" without leaking which. -->
		<div class="collab-editor__denied">
			<div class="collab-editor__denied-icon" aria-hidden="true">🔒</div>
			<h2>Can't open this file</h2>
			<p>You don't have access to it, or it doesn't exist.</p>
		</div>
	{:else if displayState === 'unavailable'}
		<!-- Circuit breaker tripped — server is unreachable. The bus
		     client stopped auto-retrying; only a user action can
		     re-arm it. Refreshing the page is the simplest way. -->
		<div class="collab-editor__denied">
			<div class="collab-editor__denied-icon" aria-hidden="true">🌩️</div>
			<h2>Server unreachable</h2>
			<p>The live-updates connection can't reach the server. Refresh the page to try again.</p>
			<button type="button" class="collab-editor__retry" onclick={() => location.reload()}>
				Refresh
			</button>
		</div>
	{:else}
		<div class="collab-editor__status">
			{#if toolbar}
				<div class="collab-editor__tools">{@render toolbar()}</div>
			{/if}
			<div class="collab-editor__meta">
				{#if peers.length > 0}
					<!-- Who else is in this file. Same colour as their caret,
					     so an avatar and a cursor are recognisably one person. -->
					<div
						class="collab-editor__peers"
						data-testid="collab-editor-peers"
						title={peers.map((p) => p.name).join(', ')}
					>
						{#each peers as peer (peer.id)}
							<span class="collab-editor__peer" style:background={peer.color} aria-label={peer.name}
								>{initials(peer.name)}</span
							>
						{/each}
					</div>
				{/if}
				<!-- Icon + colour rather than a word: the row is chrome, and the
					     states read faster as a green tick, a spinner and a red
					     warning than as text. The label lives in the tooltip. -->
				<span
					class="collab-editor__state collab-editor__state--{displayState}"
					data-testid="collab-editor-state"
					data-state={displayState}
					title={statusLabel}
					aria-label={statusLabel}
					role="status"
				>
					<Icon name={statusIcon} />
				</span>
				{#if readOnly && displayState !== 'idle' && displayState !== 'syncing'}
					<!-- Only surface read-only AFTER the server's capabilities
				     ack has landed (syncState transitions past `syncing`).
				     Otherwise the fail-closed default would flash a
				     "Read only" pill during every mount, even for
				     Editors — surprising and wrong. -->
					<span
						class="collab-editor__state collab-editor__state--readonly"
						data-testid="collab-editor-readonly"
						title="Read only — you don't have permission to edit this file"
						aria-label="Read only"
					>
						<Icon name="lock" />
					</span>
				{/if}
			</div>
		</div>
		<div
			bind:this={container}
			class="collab-editor__pane"
			role="textbox"
			aria-label="Collaborative markdown editor"
		></div>
	{/if}
</div>

<style>
	.collab-editor {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 20rem;
	}

	.collab-editor__status {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		flex-wrap: wrap;
		padding: 0.4rem 0.75rem;
		border-bottom: 1px solid var(--border-subtle);
		background: var(--surface-2);
	}

	.collab-editor__tools,
	.collab-editor__meta {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	/* Pushed right when there are no tools, so the status pill keeps its
	   place whether the host passes a toolbar or not. */
	.collab-editor__meta {
		margin-left: auto;
	}

	.collab-editor__peers {
		display: flex;
		align-items: center;
	}

	.collab-editor__peer {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.6rem;
		height: 1.6rem;
		margin-right: -0.4rem;
		border: 2px solid var(--surface-2);
		border-radius: 999px;
		font-size: 0.65rem;
		font-weight: 600;
		color: var(--color-text-light);
	}

	.collab-editor__peer:last-child {
		margin-right: 0;
	}

	/* One badge shape for every state; colour and icon carry the meaning. */
	.collab-editor__state {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.6rem;
		height: 1.6rem;
		border-radius: 999px;
		background: var(--surface-3);
		color: var(--text-muted);
	}

	.collab-editor__state--synced {
		background: var(--status-success-bg);
		color: var(--status-success-fg);
	}

	.collab-editor__state--syncing {
		color: var(--color-primary, var(--text-muted));
	}

	/* The one state a user must notice: their edits are not reaching anyone. */
	.collab-editor__state--disconnected {
		background: var(--status-danger-bg, var(--surface-3));
		color: var(--status-danger-fg, var(--color-danger));
	}

	.collab-editor__state--readonly {
		background: var(--status-warning-bg, var(--surface-3));
		color: var(--status-warning-fg, var(--text-muted));
	}

	.collab-editor__pane {
		flex: 1;
		overflow: auto;
	}

	/* Theme-aware CodeMirror surface. CodeMirror 6's default styling
	 * hardcodes light-mode colours (black caret, light gutter background),
	 * which vanish on `<html data-color-scheme="dark">`. Bind every visible
	 * surface to the app's `--color-*` tokens so both schemes look native
	 * without importing a whole CM theme. */
	.collab-editor__pane :global(.cm-editor) {
		height: 100%;
		background: var(--color-bg-surface);
		color: var(--color-text);
	}

	.collab-editor__pane :global(.cm-scroller) {
		font-family: var(--font-mono);
		font-size: 0.9rem;
	}

	.collab-editor__pane :global(.cm-content) {
		caret-color: var(--color-text);
	}

	/* Custom cursor element CM renders when `drawSelection()` is on
	 * (default for `defaultKeymap`). Native `caret-color` above covers
	 * the plain textarea path; this rule covers the drawn one. */
	.collab-editor__pane :global(.cm-cursor),
	.collab-editor__pane :global(.cm-cursor-primary) {
		border-left-color: var(--color-text);
	}

	.collab-editor__pane :global(.cm-gutters) {
		background: var(--color-bg-page, var(--color-bg-surface));
		color: var(--color-text-muted);
		border-right: 1px solid var(--color-border, transparent);
	}

	.collab-editor__pane :global(.cm-activeLine),
	.collab-editor__pane :global(.cm-activeLineGutter) {
		background: color-mix(in srgb, var(--color-accent) 10%, transparent);
	}

	.collab-editor__pane :global(.cm-selectionBackground),
	.collab-editor__pane :global(.cm-content ::selection) {
		background: color-mix(in srgb, var(--color-accent) 30%, transparent);
	}

	/* Peer cursor labels rendered by `y-codemirror.next`. The library's
	 * default styling gives the floating name a ~10 px font that's hard
	 * to read at normal viewing distance; bump it, add breathing room,
	 * and make sure it sits above the editor's own overlays.
	 *
	 * `.cm-ySelectionInfo` is the name pill above the caret.
	 * `.cm-ySelectionCaret` is the vertical caret line.
	 * `.cm-ySelectionCaretDot` is the small triangle at the top —
	 * the anchor for the pill. */
	.collab-editor__pane :global(.cm-ySelectionInfo) {
		font-size: 0.75rem;
		font-family: var(--font-sans, system-ui);
		font-weight: 500;
		padding: 0.15rem 0.4rem;
		border-radius: 0.25rem;
		/* Sit high enough to clear the top of the line and stay
		 * visible when the caret is on the first line. */
		top: -1.4em;
		line-height: 1.2;
		white-space: nowrap;
		/* Above line-decorations but below CodeMirror tooltips. */
		z-index: 20;
		/* Deliberately no `opacity` rule — `y-codemirror.next`'s
		 * default is hover-only reveal (opacity 0 → 1 on caret
		 * hover). Overriding it here would keep every peer name
		 * pinned to the screen and cover the text. Sizing only. */
	}

	.collab-editor__pane :global(.cm-ySelectionCaret) {
		/* Slightly wider than the default 1 px so peer carets are
		 * findable at a glance without being confused with the local
		 * caret (which the browser draws natively). */
		border-left-width: 2px;
		margin-left: -1px;
	}

	.collab-editor__denied {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 0.5rem;
		padding: 2rem;
		text-align: center;
		color: var(--text-muted);
	}

	.collab-editor__denied-icon {
		font-size: 3rem;
		line-height: 1;
	}

	.collab-editor__denied h2 {
		margin: 0;
		font-size: 1.15rem;
		color: var(--text-primary);
	}

	.collab-editor__denied p {
		margin: 0;
		max-width: 28rem;
	}

	.collab-editor__retry {
		margin-top: 0.5rem;
		padding: 0.45rem 1rem;
		border: 1px solid var(--border-subtle);
		border-radius: 0.35rem;
		background: var(--surface-2);
		color: var(--text-primary);
		cursor: pointer;
		font: inherit;
	}

	.collab-editor__retry:hover {
		background: var(--surface-3);
	}
</style>
