<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { appPath } from '$lib/utils/appPath';
	import { notifications } from '$lib/composables/useNotifications.svelte';
	import { player } from '$lib/stores/player.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	/**
	 * The four navigation destinations. Player is NOT one of them — it opens
	 * a panel rather than going anywhere, so it is rendered separately below,
	 * after Media: Files · Media · Player · Notifications · Account.
	 */
	const ITEMS = [
		{ href: '/files', label: t('nav.files', 'Files'), icon: 'folder' },
		{ href: '/photos', label: t('nav.photos', 'Photos'), icon: 'images' },
		{ href: '/notifications', label: t('notifications.title', 'Notifications'), icon: 'bell' },
		{ href: '/account', label: t('nav.account', 'Account'), icon: 'user-circle' }
	] as const;

	const path = $derived(appPath(page.url.pathname));
	const unread = $derived(ui.unread + notifications.unread);

	function active(href: string): boolean {
		return path === href || path.startsWith(`${href}/`);
	}
</script>

<nav class="mobile-nav" aria-label={t('nav.primary', 'Primary')}>
	{#each ITEMS as item (item.href)}
		{@const isActive = active(item.href)}
		<a
			class="mobile-nav__item"
			class:mobile-nav__item--active={isActive}
			href={resolve(item.href)}
			aria-current={isActive ? 'page' : undefined}
			data-testid={`mobile-nav-${item.href.slice(1)}-link`}
		>
			<span class="mobile-nav__icon">
				<Icon name={item.icon} />
				{#if item.href === '/notifications' && unread > 0}
					<span class="mobile-nav__badge">{unread > 99 ? '99+' : unread}</span>
				{/if}
			</span>
			<span class="mobile-nav__label">{item.label}</span>
		</a>
		<!--
			Player sits between Media and Notifications, and only where music
			is enabled — the same `features.music` that hides `/music` from the
			sidebar and `MusicPlayer` itself from the top bar.

			A LINK to `/player`, like every other entry here. It was briefly a
			button opening a bottom sheet, which cost the back gesture: a
			full-screen overlay has no history entry, so Android's back would
			navigate the page behind it rather than dismiss it. Notifications
			and Account are pages; so is this.

			Rendered inside the loop rather than added to `ITEMS` because it is
			conditional and the four destinations are not.
		-->
		{#if item.href === '/photos' && serverConfig.features.music}
			{@const playerActive = active('/player')}
			<a
				class="mobile-nav__item"
				class:mobile-nav__item--active={playerActive}
				href={resolve('/player')}
				aria-current={playerActive ? 'page' : undefined}
				data-testid="mobile-nav-player-link"
			>
				<span class="mobile-nav__icon">
					<Icon name="music" />
					{#if player.active}
						<span class="mobile-nav__badge mobile-nav__badge--glyph" aria-hidden="true">
							<Icon name={player.playing ? 'play' : 'pause'} />
						</span>
					{/if}
				</span>
				<span class="mobile-nav__label">{t('music.player', 'Player')}</span>
			</a>
		{/if}
	{/each}
</nav>

<style>
	.mobile-nav {
		display: none;
		position: fixed;
		left: 0;
		right: 0;
		bottom: 0;
		z-index: var(--z-sticky);
		height: calc(var(--mobile-nav-height) + env(safe-area-inset-bottom, 0px));
		padding: 0 var(--space-2) env(safe-area-inset-bottom, 0);
		background: var(--color-bg-surface);
		border-top: 1px solid var(--color-border);
		box-sizing: border-box;
	}

	.mobile-nav__item {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 2px;
		color: var(--color-text-subtle);
		text-decoration: none;
		font-size: var(--text-2xs);
		transition: color var(--motion-fast, 0.15s) ease;
	}

	.mobile-nav__item--active {
		color: var(--color-accent);
		font-weight: var(--weight-semibold);
	}

	.mobile-nav__icon {
		position: relative;
		font-size: var(--text-lg);
		line-height: 1;
	}

	.mobile-nav__badge {
		position: absolute;
		top: -4px;
		right: -10px;
		min-width: 16px;
		height: 16px;
		padding: 0 var(--space-1);
		border-radius: var(--radius-lg);
		background: var(--color-notification-badge);
		color: var(--color-notification-bg);
		font-size: 10px;
		font-weight: var(--weight-bold);
		line-height: 16px;
		text-align: center;
	}

	/* The player's badge carries a glyph where the notification one carries a
	   count: same circle, but centred and smaller, since a play triangle at
	   10px fills the disc where a digit has whitespace built in. */
	.mobile-nav__badge--glyph {
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 0;
		font-size: 7px;
		line-height: 1;
	}

	@media (width <= 768px) {
		.mobile-nav {
			display: flex;
		}
	}
</style>
