<script lang="ts">
	import { page } from '$app/state';
	import type { Pathname } from '$app/types';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { appPath, resolveDynamic } from '$lib/utils/appPath';
	import { notifications } from '$lib/composables/useNotifications.svelte';
	import { player } from '$lib/stores/player.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	/** Which badge, if any, an entry's icon carries. */
	type Badge = 'unread' | 'playback';

	interface NavItem {
		/**
		 * A route of this app. Typed as `Pathname` only at the `resolveDynamic`
		 * call: the list is assembled with spreads, which widen a literal to
		 * `string` and lose the route union on the way through.
		 */
		href: string;
		label: string;
		icon: string;
		/** Test id suffix; the href's own slug for everything but `/player`. */
		testId: string;
		badge?: Badge;
	}

	/**
	 * Files · Media · Player · Notifications · Account, with the two optional
	 * ones gated on the SAME flags as the sidebar: a deployment with photos or
	 * music off must not advertise them here either.
	 *
	 * Derived rather than a const, and Player is a member rather than something
	 * rendered beside Media. It used to be injected inside the loop with
	 * `{#if item.href === '/photos'}` for its position, which quietly tied it
	 * to Media's existence — gating Media out would have removed Player too.
	 * The order lives in one array now, where it can be read off.
	 */
	const ITEMS = $derived.by<NavItem[]>(() => [
		{ href: '/files', label: t('nav.files', 'Files'), icon: 'folder', testId: 'files' },
		...(serverConfig.features.photos
			? [{ href: '/photos', label: t('nav.photos', 'Media'), icon: 'images', testId: 'photos' }]
			: []),
		...(serverConfig.features.music
			? [
					{
						href: '/player',
						label: t('music.player', 'Player'),
						icon: 'music',
						testId: 'player',
						badge: 'playback' as const
					}
				]
			: []),
		{
			href: '/notifications',
			label: t('notifications.title', 'Notifications'),
			icon: 'bell',
			testId: 'notifications',
			badge: 'unread'
		},
		{ href: '/account', label: t('nav.account', 'Account'), icon: 'user-circle', testId: 'account' }
	]);

	const path = $derived(appPath(page.url.pathname));
	const unread = $derived(ui.unread + notifications.unread);

	function active(href: string): boolean {
		return path === href || path.startsWith(`${href}/`);
	}
</script>

<nav class="mobile-nav" aria-label={t('nav.primary', 'Primary')}>
	<!--
		`resolveDynamic` IS `resolve`, with the one cast the route union needs
		when the path comes from a list rather than a literal — the lint rule
		cannot see through the wrapper. Disabled for the loop, as `AppShell`
		does for its sidebar, rather than per line.
	-->
	<!-- eslint-disable svelte/no-navigation-without-resolve -->
	{#each ITEMS as item (item.href)}
		{@const isActive = active(item.href)}
		<a
			class="mobile-nav__item"
			class:mobile-nav__item--active={isActive}
			href={resolveDynamic(item.href as Pathname)}
			aria-current={isActive ? 'page' : undefined}
			data-testid={`mobile-nav-${item.testId}-link`}
		>
			<span class="mobile-nav__icon">
				<Icon name={item.icon} />
				{#if item.badge === 'unread' && unread > 0}
					<span class="mobile-nav__badge">{unread > 99 ? '99+' : unread}</span>
				{:else if item.badge === 'playback' && player.active}
					<span class="mobile-nav__badge mobile-nav__badge--glyph" aria-hidden="true">
						<Icon name={player.playing ? 'play' : 'pause'} />
					</span>
				{/if}
			</span>
			<span class="mobile-nav__label">{item.label}</span>
		</a>
	{/each}
	<!-- eslint-enable svelte/no-navigation-without-resolve -->
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
