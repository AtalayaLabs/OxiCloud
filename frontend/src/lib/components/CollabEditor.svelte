<script lang="ts">
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
	// Still ahead:
	//   * Explicit rt.collab_flush on unmount — the debouncer already
	//     bounds staleness to 60s; wiring `beforeunload` is a polish
	//     slice.
	//   * Read-only mode when Update permission is missing — the
	//     server's per-frame gate refuses the 0x01 UPDATE anyway;
	//     surfacing this in the UI is a follow-up.

	import { EditorState } from '@codemirror/state';
	import { EditorView, keymap, lineNumbers } from '@codemirror/view';
	import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
	import { markdown } from '@codemirror/lang-markdown';
	import { yCollab } from 'y-codemirror.next';
	import { onDestroy } from 'svelte';

	import { CollabDoc, type SyncState } from '$lib/collab/collabDoc';
	import { messageBus } from '$lib/message-bus/client.svelte';
	import { session } from '$lib/stores/session.svelte';

	/** Deterministic peer-cursor colour derived from a user id.
	 *
	 *  Rule: same user id → same colour across sessions and machines,
	 *  so peers recognise each other visually. Using a small hue-only
	 *  palette (12 slots, evenly-spaced across the wheel) at fixed
	 *  saturation / lightness keeps every colour readable on both
	 *  light and dark backgrounds without needing per-scheme
	 *  overrides. Hash → hue index is `sum-of-char-codes mod 12` —
	 *  crude but stable, no external hash lib needed. */
	function userColor(userId: string): string {
		let sum = 0;
		for (let i = 0; i < userId.length; i++) sum = (sum + userId.charCodeAt(i)) & 0xffff;
		const hueSlot = sum % 12;
		return `hsl(${hueSlot * 30} 70% 50%)`;
	}

	interface Props {
		/** Dashed UUID of the file to edit. */
		fileId: string;
	}
	let { fileId }: Props = $props();

	let syncState = $state<SyncState>('idle');
	let container: HTMLDivElement | undefined = $state();

	/** Effective state shown to the user. The bus's circuit-tripped
	 *  `unavailable` outranks any per-doc state — no point telling
	 *  the user "syncing…" when the underlying transport has given
	 *  up. Denied stays terminal (that's already a permanent state). */
	const displayState = $derived<SyncState>(
		messageBus.state === 'unavailable' && syncState !== 'denied' ? 'unavailable' : syncState
	);

	let collab: CollabDoc | undefined;
	let view: EditorView | undefined;

	// Mount effect: attach CodeMirror + CollabDoc when `container`
	// becomes available. Runs once per `fileId` change; the cleanup
	// tears down and $effect re-runs for a new file. This is the
	// canonical Svelte 5 pattern for imperative library integration.
	$effect(() => {
		if (!container) return;
		const currentFileId = fileId;

		collab = new CollabDoc({
			fileId: currentFileId,
			onSyncStateChange: (s) => {
				syncState = s;
			}
		});
		collab.connect();

		// Publish local user info into the awareness registry so peers
		// can render this caret with a name + colour. Read the session
		// snapshot at mount time — if it's not loaded yet (edge case
		// during boot) we still publish a placeholder rather than
		// leaving the peer view unlabelled.
		const localUser = session.user;
		collab.awareness.setLocalStateField('user', {
			name: localUser?.username ?? 'Anonymous',
			color: userColor(localUser?.id ?? currentFileId)
		});

		const state = EditorState.create({
			doc: '', // initial content comes from the CRDT after sync-step-2
			extensions: [
				lineNumbers(),
				history(),
				keymap.of([...defaultKeymap, ...historyKeymap]),
				markdown(),
				// Pass the awareness registry so `y-codemirror.next`
				// renders peer cursors + selections with the `user`
				// field we just published (name + colour).
				yCollab(collab.yText(), collab.awareness)
			]
		});

		view = new EditorView({
			state,
			parent: container
		});

		return () => {
			view?.destroy();
			view = undefined;
			collab?.destroy();
			collab = undefined;
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
			<span class="collab-editor__status-pill collab-editor__status-pill--{displayState}">
				{#if displayState === 'idle'}
					Ready
				{:else if displayState === 'syncing'}
					Syncing…
				{:else if displayState === 'synced'}
					Synced
				{:else if displayState === 'disconnected'}
					Disconnected
				{/if}
			</span>
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
		padding: 0.4rem 0.75rem;
		border-bottom: 1px solid var(--border-subtle);
		background: var(--surface-2);
	}

	.collab-editor__status-pill {
		display: inline-flex;
		align-items: center;
		padding: 0.15rem 0.5rem;
		border-radius: 999px;
		font-size: 0.75rem;
		background: var(--surface-3);
		color: var(--text-muted);
	}

	.collab-editor__status-pill--synced {
		background: var(--status-success-bg);
		color: var(--status-success-fg);
	}

	.collab-editor__status-pill--syncing {
		background: var(--status-info-bg);
		color: var(--status-info-fg);
	}

	.collab-editor__status-pill--disconnected {
		background: var(--status-error-bg);
		color: var(--status-error-fg);
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
