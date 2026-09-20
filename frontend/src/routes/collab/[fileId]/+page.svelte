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
	// Wait for the boot-time `/api/config` fetch to resolve before
	// deciding on/off. Reading `features.markdown_collab` before
	// `loaded` returns the store's pre-load default (false), which
	// would flash "Markdown collab is off" for the ~ms window between
	// route mount and config load.
	const enabled = $derived(serverConfig.loaded && serverConfig.features.markdown_collab === true);
	const stillLoading = $derived(!serverConfig.loaded);
</script>

<svelte:head>
	<title>Collab · {fileId}</title>
</svelte:head>

<div class="collab-page">
	{#if stillLoading}
		<!-- Config not yet fetched — don't decide on/off yet. The
		     pre-load defaults have `markdown_collab: false`, and
		     rendering the "off" notice here would flash it for anyone
		     who refreshes while `/api/config` is in flight (or the
		     server is still starting up). -->
		<div class="collab-page__notice">
			<p>Loading…</p>
		</div>
	{:else if !enabled}
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
