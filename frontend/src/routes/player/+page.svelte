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
	   sit on the same grid.

	   NO `height: 100%`, deliberately. With it, the panel was pinned to the
	   viewport and `GestureList`'s `flex: 1` queue absorbed whatever was left
	   after the cover, transport and volume bars — which on a short screen was
	   two rows and a scrollbar. The page is the scroller instead: everything
	   keeps its natural size and the queue runs as long as it likes, because
	   `.content-area` above already scrolls and already pads for the fixed
	   mobile footer.

	   The popover placement is unaffected — its height cap lives in
	   `MusicPlayer`, so the queue still scrolls inside the dropdown. */
	.player-page {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		max-width: 52rem;
		margin: 0 auto;
	}

	.player-page__header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
	}

	/* Short viewport — a phone in landscape, or a small window. The heading
	   costs a line plus the column's gap, and it is the most expendable thing
	   on the page: the footer entry that leads here is labelled "Player", the
	   tab title says it, and the panel below shows cover, title and transport.
	   That space is worth more as queue rows. */
	@media (height <= 600px) {
		.player-page__header {
			display: none;
		}
	}
</style>
