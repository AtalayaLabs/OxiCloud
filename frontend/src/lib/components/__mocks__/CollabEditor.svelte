<script lang="ts">
	// Test double for `CollabEditor`. The real one mounts CodeMirror and
	// opens a message-bus session — neither belongs in a component test of
	// its host. It renders the host's toolbar snippet so tests can assert on
	// what the host puts in the editor's row.
	import type { Snippet } from 'svelte';

	interface Props {
		fileId: string;
		filename?: string;
		toolbar?: Snippet;
	}
	let { toolbar }: Props = $props();
</script>

<div data-testid="collab-editor-stub">
	{@render toolbar?.()}
</div>
