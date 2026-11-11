<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { onMount } from 'svelte';
	import AppSplash from '$lib/components/AppSplash.svelte';
	import { session } from '$lib/stores/session.svelte';

	// The app root redirects by user kind:
	//   - external users (magic-link / OIDC-only / OCM recipients) have no
	//     personal drive, so they land on Shared-with-me;
	//   - internal users go to the files browser, which resolves the default
	//     personal drive's root folder via `session.loadHomeFolder()` (post-D0
	//     this reads `GET /api/drives` and picks the row whose
	//     `default_for_user` matches the caller).
	onMount(() => {
		const target = session.isExternalUser ? '/shared-with-me' : '/files';
		void goto(resolve(target), { replaceState: true });
	});
</script>

<!--
	This page is never READ, only passed through: it exists to decide where
	`/` goes. So it must look like the boot it continues, not like a page.

	It used to be a bare `<p>Loading…</p>` — unstyled, untranslated, left
	behind when every other waiting screen moved to `AppSplash` (see the
	note at the foot of `+layout.svelte`). Signed out it was visible on
	every single visit to `/`: the layout's splash covers the session
	probe, but once that resolves the guard renders this page for the frame
	or two the `goto('/login')` takes, so the flash was the hand-off
	between two spinners showing a paragraph of text.

	`AppSplash` is the same component `app.html`'s inline splash and the
	OIDC callback overlay use, so the whole boot is now one unbroken
	screen. The label names what is being waited on rather than the widget.
-->
<AppSplash label="Loading OxiCloud" />
