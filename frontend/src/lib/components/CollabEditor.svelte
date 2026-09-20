<script lang="ts">
	// Collaborative markdown editor — the simple version.
	//
	// Mounts CodeMirror 6 with markdown syntax + the `y-codemirror.next`
	// binding to a `Y.Text` sourced from a `CollabDoc`. One editor per
	// file id; wraps its own lifecycle so callers just render
	// `<CollabEditor fileId={id} />` and unmount to teardown.
	//
	// Out of scope for the simple version:
	//   * Awareness / presence cursors — C6.
	//   * Explicit rt.collab_flush on unmount — the debouncer already
	//     bounds staleness to 60s; we'll wire beforeunload in a polish
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

	interface Props {
		/** Dashed UUID of the file to edit. */
		fileId: string;
	}
	let { fileId }: Props = $props();

	let syncState = $state<SyncState>('idle');
	let container: HTMLDivElement | undefined = $state();

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

		const state = EditorState.create({
			doc: '', // initial content comes from the CRDT after sync-step-2
			extensions: [
				lineNumbers(),
				history(),
				keymap.of([...defaultKeymap, ...historyKeymap]),
				markdown(),
				// `undefined` = no awareness for the simple version.
				// The binding still consumes updates on the yText and
				// emits local edits back through Y.Doc.update, which our
				// `CollabDoc.#docUpdateHandler` translates to 0x01
				// UPDATE frames on the wire.
				yCollab(collab.yText(), undefined)
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
	<div class="collab-editor__status">
		<span class="collab-editor__status-pill collab-editor__status-pill--{syncState}">
			{#if syncState === 'idle'}
				Ready
			{:else if syncState === 'syncing'}
				Syncing…
			{:else if syncState === 'synced'}
				Synced
			{:else if syncState === 'disconnected'}
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

	.collab-editor__pane :global(.cm-editor) {
		height: 100%;
	}

	.collab-editor__pane :global(.cm-scroller) {
		font-family: var(--font-mono);
		font-size: 0.9rem;
	}
</style>
