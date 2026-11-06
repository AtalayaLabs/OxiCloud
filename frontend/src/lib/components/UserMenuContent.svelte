<script lang="ts">
	import { resolve } from '$app/paths';
	import Icon from '$lib/icons/Icon.svelte';
	import SelfAvatar from '$lib/components/SelfAvatar.svelte';
	import { isAtLeastAdmin } from '$lib/utils/roles';
	import { i18n, LANGUAGES, setLocale, t, type Locale } from '$lib/i18n/index.svelte';
	import { session } from '$lib/stores/session.svelte';
	import { theme, type Theme } from '$lib/stores/theme.svelte';
	import { performLogout } from '$lib/auth/logout';

	interface Props {
		/** Close the surrounding dropdown after a navigation row is clicked. */
		onNavigate?: () => void;
		onabout?: () => void;
	}
	let { onNavigate, onabout }: Props = $props();

	let langOpen = $state(false);
	let langToggle: HTMLButtonElement | undefined;

	const isAdmin = $derived(isAtLeastAdmin(session.user?.role));

	const THEMES: { mode: Theme; icon: string; label: string }[] = [
		{ mode: 'light', icon: 'sun', label: t('user_menu.theme.light', 'Light') },
		{ mode: 'auto', icon: 'desktop', label: t('user_menu.theme.auto', 'Like OS') },
		{ mode: 'dark', icon: 'moon', label: t('user_menu.theme.dark', 'Dark') }
	];

	const currentLang = $derived(LANGUAGES.find((l) => l.code === i18n.locale) ?? LANGUAGES[0]);

	async function chooseLocale(loc: Locale) {
		langOpen = false;
		await setLocale(loc);
	}

	function placePopover(node: HTMLElement) {
		if (!langToggle) return;
		const gap = 4;
		const pad = 8;
		const rect = langToggle.getBoundingClientRect();
		const width = node.offsetWidth;
		const height = node.offsetHeight;
		const belowRoom = window.innerHeight - rect.bottom - gap - pad;
		const aboveRoom = rect.top - gap - pad;
		const below = belowRoom >= height || belowRoom >= aboveRoom;
		const room = Math.max(96, below ? belowRoom : aboveRoom);
		if (room < height) node.style.maxHeight = `${room}px`;
		node.style.top = `${below ? rect.bottom + gap : Math.max(pad, rect.top - gap - Math.min(height, room))}px`;
		node.style.left = `${Math.max(pad, Math.min(rect.right - width, window.innerWidth - width - pad))}px`;
	}

	function topLayerMenu(node: HTMLElement) {
		node.showPopover();
		placePopover(node);
		const dismiss = (event: Event) => {
			if (
				event.target instanceof Node &&
				(node.contains(event.target) || langToggle?.contains(event.target))
			)
				return;
			langOpen = false;
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') langOpen = false;
		};
		window.addEventListener('pointerdown', dismiss, { capture: true });
		window.addEventListener('scroll', dismiss, { capture: true, passive: true });
		window.addEventListener('resize', dismiss);
		window.addEventListener('keydown', onKey);
		return {
			destroy: () => {
				window.removeEventListener('pointerdown', dismiss, { capture: true });
				window.removeEventListener('scroll', dismiss, { capture: true });
				window.removeEventListener('resize', dismiss);
				window.removeEventListener('keydown', onKey);
			}
		};
	}
</script>

<svelte:window onclick={() => (langOpen = false)} />

{#if session.user}
	<div class="user-menu-header">
		<SelfAvatar large />
		<div class="user-menu-id">
			<div class="user-menu-identity">
				<div class="user-menu-name">{session.user.username || session.user.email}</div>
				{#if isAdmin}
					<span class="role-badge role-badge-admin">
						<Icon name="shield-alt" />
						{t('user_menu.admin', 'Admin')}
					</span>
				{/if}
			</div>
			<div class="user-menu-email">{session.user.email}</div>
		</div>
	</div>
{/if}

<div class="user-menu-divider"></div>

{#if isAdmin}
	<a
		class="user-menu-item"
		href={resolve('/admin')}
		data-testid="appshell-user-menu-admin-item"
		onclick={onNavigate}
	>
		<Icon name="cogs" /> <span>{t('user_menu.admin_panel', 'Admin panel')}</span>
	</a>
	<a
		class="user-menu-item"
		href={resolve('/groups')}
		data-testid="appshell-user-menu-groups-item"
		onclick={onNavigate}
	>
		<Icon name="user-group" />
		<span>{t('user_menu.manage_groups', 'Manage groups')}</span>
	</a>
{/if}
<a
	class="user-menu-item"
	href={resolve('/profile')}
	data-testid="appshell-user-menu-profile-item"
	onclick={onNavigate}
>
	<Icon name="user-circle" /> <span>{t('user_menu.profile', 'My profile')}</span>
</a>

<div class="user-menu-divider"></div>

<div class="user-menu-item user-menu-item--lang">
	<Icon name="globe" />
	<span>{t('settings.language', 'Language')}</span>
	<div class="lang-selector" class:lang-selector--open={langOpen} data-testid="appshell-lang-menu">
		<button
			type="button"
			class="lang-selector__toggle"
			bind:this={langToggle}
			aria-haspopup="listbox"
			aria-expanded={langOpen}
			data-testid="appshell-lang-toggle-btn"
			onclick={(e) => {
				e.stopPropagation();
				langOpen = !langOpen;
			}}
		>
			<span class="lang-selector__label">{currentLang.flag} {currentLang.name}</span>
			<Icon name="chevron-down" class="lang-selector__arrow" />
		</button>
		{#if langOpen}
			<ul class="lang-selector__dropdown" role="listbox" popover="manual" use:topLayerMenu>
				{#each LANGUAGES as lang (lang.code)}
					<li>
						<button
							type="button"
							class="lang-option"
							class:lang-option--active={lang.code === i18n.locale}
							role="option"
							aria-selected={lang.code === i18n.locale}
							data-testid={`appshell-lang-${lang.code}-option`}
							onclick={(e) => {
								e.stopPropagation();
								chooseLocale(lang.code);
							}}
						>
							<span class="lang-option__flag">{lang.flag}</span>
							<span class="lang-option__name">{lang.name}</span>
							{#if lang.code === i18n.locale}
								<Icon name="check" class="lang-option__check" />
							{/if}
						</button>
					</li>
				{/each}
			</ul>
		{/if}
	</div>
</div>

<div class="user-menu-item user-menu-item--theme">
	<Icon name="adjust" />
	<span>{t('user_menu.appearance', 'Appearance')}</span>
	<div
		class="theme-segmented"
		role="radiogroup"
		aria-label={t('user_menu.appearance', 'Appearance')}
		data-testid="appshell-theme-toggle"
	>
		{#each THEMES as th (th.mode)}
			<button
				type="button"
				class="theme-segmented__opt"
				class:theme-segmented__opt--active={theme.current === th.mode}
				role="radio"
				aria-checked={theme.current === th.mode}
				title={th.label}
				aria-label={th.label}
				data-testid={`appshell-theme-${th.mode}-option`}
				onclick={(e) => {
					e.stopPropagation();
					theme.set(th.mode);
				}}
			>
				<Icon name={th.icon} />
			</button>
		{/each}
	</div>
</div>

<button class="user-menu-item" data-testid="appshell-user-menu-about-item" onclick={onabout}>
	<Icon name="info-circle" /> <span>{t('user_menu.about', 'About OxiCloud')}</span>
</button>

<div class="user-menu-divider"></div>

<button
	class="user-menu-item user-menu-logout"
	data-testid="appshell-user-menu-logout-btn"
	onclick={() => void performLogout()}
>
	<Icon name="sign-out-alt" /> <span>{t('actions.logout', 'Log out')}</span>
</button>

<style>
	.user-menu-header {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.user-menu-id {
		min-width: 0;
	}

	.user-menu-identity {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}

	.user-menu-name {
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}

	.user-menu-email {
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.user-menu-item--lang,
	.user-menu-item--theme {
		cursor: default;
	}

	/* Custom language selector — flag + native name + active checkmark. */
	.lang-selector {
		position: relative;
		margin-left: auto;
	}

	.lang-selector__toggle {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: var(--space-1) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		cursor: pointer;
		font-size: var(--text-sm);
	}

	.lang-selector__toggle:hover {
		background: var(--color-bg-hover);
	}

	.lang-selector__label {
		font-weight: var(--weight-semibold);
		white-space: nowrap;
	}

	:global(.lang-selector__arrow) {
		font-size: var(--text-xs, 0.7rem);
		transition: transform 0.15s ease;
	}

	.lang-selector--open :global(.lang-selector__arrow) {
		transform: rotate(180deg);
	}

	.lang-selector__dropdown {
		position: fixed;
		inset: auto;
		z-index: var(--z-dropdown);
		min-width: 12rem;
		max-height: 18rem;
		overflow: auto;
		list-style: none;
		margin: 0;
		padding: 0.25rem;
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg, var(--radius-md));
		box-shadow: var(--shadow-lg, 0 10px 30px var(--color-overlay-shadow));
	}

	.lang-option {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		width: 100%;
		padding: 0.45rem 0.55rem;
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		text-align: left;
		border-radius: var(--radius-sm);
		font-size: var(--text-sm);
	}

	.lang-option:hover {
		background: var(--color-bg-hover);
	}

	.lang-option--active {
		color: var(--color-primary);
		font-weight: var(--weight-semibold);
	}

	.lang-option__name {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	:global(.lang-option__check) {
		color: var(--color-primary);
		flex-shrink: 0;
	}
</style>
