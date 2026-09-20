<script lang="ts">
	// Standalone collab-editor test route.
	//
	// Navigate to `/collab/<file-uuid>` while logged in — the page
	// mounts a `CollabEditor` against that file id. Feature-gated on
	// `serverConfig.features.markdown_collab`; when off, shows a hint
	// pointing at the env var to enable it.
	//
	// This is intentionally NOT the eventual UX. The proper open-a-.md
	// flow will route through the file listing / preview surface; this
	// page just lets us test the wire and editor without file-view
	// integration blocking the demo.

	import { page } from '$app/state';

	import CollabEditor from '$lib/components/CollabEditor.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';

	const fileId = $derived(page.params.fileId ?? '');
	const enabled = $derived(serverConfig.features.markdown_collab === true);
</script>

<svelte:head>
	<title>Collab · {fileId}</title>
</svelte:head>

<div class="collab-page">
	{#if !enabled}
		<div class="collab-page__notice">
			<h1>Markdown collab is off</h1>
			<p>
				Set <code>OXICLOUD_ENABLE_MARKDOWN_COLLAB=true</code> on the server (requires
				<code>OXICLOUD_MESSAGEBUS_ENABLE=true</code>) and restart.
			</p>
		</div>
	{:else if !fileId}
		<div class="collab-page__notice">
			<h1>Missing file id</h1>
			<p>Open <code>/collab/&lt;file-uuid&gt;</code>.</p>
		</div>
	{:else}
		<CollabEditor {fileId} />
	{/if}
</div>

<style>
	.collab-page {
		display: flex;
		flex-direction: column;
		height: 100vh;
	}

	.collab-page__notice {
		padding: 2rem;
		max-width: 40rem;
		margin: 4rem auto;
		text-align: center;
	}

	.collab-page__notice code {
		background: var(--surface-2);
		padding: 0.1rem 0.35rem;
		border-radius: 0.2rem;
	}
</style>
