<script lang="ts">
	import PlayerPanel from '$lib/components/PlayerPanel.svelte';
	import { t } from '$lib/i18n/index.svelte';

	/**
	 * The player as a page, which is how the mobile footer reaches it —
	 * matching Notifications and Account, whose footer entries are also
	 * routes.
	 *
	 * A route rather than a full-screen overlay, because an overlay has no
	 * history entry: Android's back gesture would navigate the page behind it
	 * instead of dismissing it, and you would have to hand-roll a pushState
	 * shim, a focus trap and scroll locking to get back what a route gives
	 * for free. It also means the player survives a refresh with everything
	 * else — `player.restore()` has already run by the time this renders.
	 *
	 * Nothing about playback lives here. The `<audio>` element stays in
	 * `MusicPlayer`, mounted by `AppShell` above the router, so opening this
	 * page does not restart or duplicate anything — it is another view onto
	 * the one player.
	 */
</script>

<svelte:head><title>{t('music.player', 'Player')} · OxiCloud</title></svelte:head>

<div class="player-page">
	<header class="player-page__header">
		<h1 class="page-title">{t('music.player', 'Player')}</h1>
	</header>
	<PlayerPanel />
</div>

<style>
	/* Same measure as `/notifications`, so the two pages the footer leads to
	   sit on the same grid. A flex column because `PlayerPanel`'s queue takes
	   the remaining height and scrolls inside it. */
	.player-page {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		max-width: 52rem;
		height: 100%;
		min-height: 0;
		margin: 0 auto;
	}

	.player-page__header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
	}
</style>
