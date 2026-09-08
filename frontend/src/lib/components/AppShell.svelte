<script lang="ts">
	import type { Snippet } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { logout } from '$lib/api/endpoints/auth';
	import { setLogoutInProgress } from '$lib/api/client';
	import { searchResources } from '$lib/api/endpoints/search';
	import { fileInlineUrl, deleteFile } from '$lib/api/endpoints/files';
	import { deleteFolder } from '$lib/api/endpoints/folders';
	import { addFavorite } from '$lib/api/endpoints/favorites';
	import type { FileItem, FolderItem, ItemType } from '$lib/api/types';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import DrivePicker from '$lib/components/DrivePicker.svelte';
	import ReadOnlyBanner from '$lib/components/ReadOnlyBanner.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { dateTimeFormatFor, iconNameFromClass } from '$lib/utils/display';
	import { userInitials, avatarColorIndex } from '$lib/utils/avatar';
	import { i18n, LANGUAGES, setLocale, t, type Locale } from '$lib/i18n/index.svelte';
	import { serverStatus } from '$lib/stores/serverStatus.svelte';
	import { apiFetch } from '$lib/api/client';
	import { dialogs } from '$lib/stores/dialogs.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { preferences } from '$lib/stores/preferences.svelte';
	import { session } from '$lib/stores/session.svelte';
	import { theme, type Theme } from '$lib/stores/theme.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { errorToast } from '$lib/utils/errors';
	import { formatBytes } from '$lib/utils/format';

	let { children }: { children: Snippet } = $props();

	// The command palette is loaded on its first Cmd/Ctrl+K and mounted open.
	// Until then its ~400-line module stays out of the initial bundle.
	const palette = lazyComponent(() => import('$lib/components/CommandPalette.svelte'));

	interface NavLink {
		/**
		 * String rather than a literal union so admin links (which
		 * include a dynamic path segment) can share the same shape.
		 * `resolve()` accepts any string, so no type-level cost.
		 */
		href: string;
		label: string;
		icon: string;
		/** Stable key driving the per-section icon colour (see sidebar.css). */
		section: string;
		admin?: boolean;
	}

	const LINKS: NavLink[] = [
		{ href: '/files', label: t('nav.files', 'Files'), icon: 'folder', section: 'files' },
		{ href: '/shared', label: t('nav.shared', 'Shared'), icon: 'oxiexport', section: 'shared' },
		{
			href: '/shared-with-me',
			label: t('nav.shared_with_me', 'Shared with me'),
			icon: 'oxiimport',
			section: 'shared-with-me'
		},
		{ href: '/recent', label: t('nav.recent', 'Recent'), icon: 'clock', section: 'recent' },
		{
			href: '/favorites',
			label: t('nav.favorites', 'Favorites'),
			icon: 'star',
			section: 'favorites'
		},
		{
			href: '/calendar',
			label: t('nav.calendar', 'Calendar'),
			icon: 'calendar',
			section: 'calendar'
		},
		{ href: '/photos', label: t('nav.photos', 'Photos'), icon: 'images', section: 'photos' },
		{ href: '/music', label: t('nav.music', 'Music'), icon: 'music', section: 'music' },
		{ href: '/trash', label: t('nav.trash', 'Trash'), icon: 'trash', section: 'trash' }
	];

	// Admin sidebar — populated when the URL is under /admin. The
	// admin +page.svelte used to render its own horizontal tab
	// strip; that was displaced here so the section navigation
	// scales past ~7 items and matches deep-link URLs from the
	// address bar.
	const ADMIN_LINKS: NavLink[] = [
		{
			href: '/admin',
			label: t('admin.dashboard', 'Dashboard'),
			icon: 'chart-pie',
			section: 'admin-dashboard'
		},
		{
			href: '/admin/users',
			label: t('admin.users', 'Users'),
			icon: 'users',
			section: 'admin-users'
		},
		{
			href: '/admin/sessions',
			label: t('admin.sessions', 'Sessions'),
			icon: 'key',
			section: 'admin-sessions'
		},
		{
			href: '/admin/drives',
			label: t('admin.drives', 'Drives'),
			icon: 'hdd',
			section: 'admin-drives'
		},
		{
			href: '/admin/mounts',
			label: t('admin.mounts', 'External Mounts'),
			icon: 'folder',
			section: 'admin-mounts'
		},
		{
			href: '/admin/oidc',
			label: t('admin.oidc', 'OIDC / SSO'),
			icon: 'building-shield',
			section: 'admin-oidc'
		},
		{
			href: '/admin/storage',
			label: t('admin.storage_tab', 'Storage'),
			icon: 'database',
			section: 'admin-storage'
		},
		{
			href: '/admin/smtp',
			label: t('admin.smtp', 'Email (SMTP)'),
			icon: 'envelope',
			section: 'admin-smtp'
		},
		{
			href: '/admin/plugins',
			label: t('admin.plugins', 'Plugins'),
			icon: 'layer-group',
			section: 'admin-plugins'
		},
		{
			href: '/admin/jobs',
			label: t('admin.jobs.tab', 'Background tasks'),
			icon: 'cogs',
			section: 'admin-jobs'
		}
	];

	const isAdmin = $derived(session.user?.role === 'admin');

	// Any URL under /admin swaps the sidebar to admin mode. Uses
	// startsWith so a trailing slash / query params / hash don't
	// desync. Root `/admin` counts too (dashboard).
	const isAdminSection = $derived(page.url.pathname.startsWith('/admin'));
	const currentLinks = $derived(isAdminSection ? ADMIN_LINKS : LINKS);

	function active(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(`${href}/`);
	}

	// Sidebar-item active check. Non-admin links use `active()`
	// (matches href + any subpath). Admin links need a stricter
	// rule for `/admin` itself — a plain `startsWith('/admin/')`
	// would light up the Dashboard item on `/admin/drives` too.
	// So `/admin` matches ONLY the exact path; every other admin
	// item uses the same startsWith rule as before.
	function activeLink(href: string): boolean {
		if (href === '/admin') return page.url.pathname === '/admin';
		return active(href);
	}

	/**
	 * Data-driven sidebar links (`LINKS`, `ADMIN_LINKS`) hold
	 * runtime strings, not compile-time route keys. SvelteKit's
	 * typed `resolve()` refuses them; we know they're valid
	 * routes at runtime. Cast at the callsite, one place, so
	 * the template stays clean.
	 */
	function navHref(href: string): string {
		// @ts-expect-error runtime-known route string, not a literal typed key
		return resolve(href);
	}

	// ── Sidebar drop targets ─────────────────────────────────────────────────
	// The row-drag on `/files` (and other resource surfaces) sets a
	// `application/x-oxi-item` MIME with a JSON array of `{ id, name, kind }`.
	// The Favorites and Trash sidebar links accept a drop of that shape:
	//   – Favorites: batch-add each item as a favorite (idempotent server-side).
	//   – Trash: yes/no confirm dialog, then batch-delete on approval.
	// Every other sidebar link stays inert (no dragover ⇒ no drop cursor).
	const SIDEBAR_DRAG_TYPE = 'application/x-oxi-item';
	interface DragItem {
		id: string;
		name: string;
		kind: ItemType;
	}
	let sidebarDropHref = $state<string | null>(null);

	function sidebarOnDragOver(e: DragEvent, href: string) {
		if (!e.dataTransfer?.types.includes(SIDEBAR_DRAG_TYPE)) return;
		e.preventDefault();
		// Favorites is a pure add — signal "copy". Trash is destructive — signal
		// "move" so the cursor doesn't imply a benign copy for something that
		// actually removes the row from its source view.
		e.dataTransfer.dropEffect = href === '/favorites' ? 'copy' : 'move';
		sidebarDropHref = href;
	}

	function sidebarOnDragLeave(href: string) {
		if (sidebarDropHref === href) sidebarDropHref = null;
	}

	function parseDragItems(e: DragEvent): DragItem[] {
		const raw = e.dataTransfer?.getData(SIDEBAR_DRAG_TYPE);
		if (!raw) return [];
		try {
			const parsed = JSON.parse(raw) as unknown;
			if (!Array.isArray(parsed)) return [];
			return parsed.filter(
				(it): it is DragItem =>
					!!it &&
					typeof it === 'object' &&
					typeof (it as DragItem).id === 'string' &&
					typeof (it as DragItem).name === 'string' &&
					((it as DragItem).kind === 'file' || (it as DragItem).kind === 'folder')
			);
		} catch {
			return [];
		}
	}

	// Small bounded fan-out — same shape as `/files`'s `mapLimit` but
	// inlined so AppShell doesn't grow a shared-util dependency for two
	// call sites. A drop is usually small (single row or a page of
	// selection), so 6 concurrent requests is plenty.
	async function fanout<T>(items: T[], limit: number, fn: (item: T) => Promise<void>) {
		let next = 0;
		const worker = async () => {
			while (next < items.length) {
				const i = next++;
				try {
					await fn(items[i]);
				} catch (e) {
					errorToast(e);
				}
			}
		};
		await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
	}

	async function sidebarOnDrop(e: DragEvent, href: string) {
		// Narrow the sidebar-link href to the two accepting targets. Called
		// from the row template, which can't type-narrow the loop variable
		// through the arrow-function closure without a local capture.
		if (href !== '/favorites' && href !== '/trash') return;
		sidebarDropHref = null;
		if (!e.dataTransfer?.types.includes(SIDEBAR_DRAG_TYPE)) return;
		e.preventDefault();
		const items = parseDragItems(e);
		if (items.length === 0) return;

		if (href === '/favorites') {
			await fanout(items, 6, async (it) => {
				await addFavorite(it.kind, it.id);
				// Broadcast so a mounted resource-list source page (e.g. /files,
				// /recent, /shared-with-me) can flip the row's `is_favorite` in
				// place without a re-fetch. ResourceList listens for the same
				// event and patches its `items` prop. Uses the existing
				// window-event pattern (see `oxicloud:upload-files`).
				window.dispatchEvent(
					new CustomEvent('oxicloud:favorite-changed', {
						detail: { id: it.id, is_favorite: true }
					})
				);
			});
			ui.notify(
				t('favorites.added_n', { n: items.length }, 'Added {{n}} item(s) to favorites'),
				'success'
			);
			return;
		}

		// Trash: destructive → gate on confirm.
		const ok = await dialogs.confirm({
			title: t('files.batch_delete', 'Delete selected'),
			message:
				items.length === 1
					? t('files.confirm_delete', { name: items[0].name }, 'Move "{{name}}" to trash?')
					: t('files.confirm_batch_delete', { n: items.length }, 'Move {{n}} items to trash?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		await fanout(items, 6, (it) => (it.kind === 'file' ? deleteFile(it.id) : deleteFolder(it.id)));
	}

	let sidebarOpen = $state(false);
	let notifOpen = $state(false);
	let menuOpen = $state(false);
	let searchQuery = $state('');
	/** Mobile collapsible-search overlay state (toggles .top-bar--search-active). */
	let searchActive = $state(false);
	let langOpen = $state(false);
	let aboutOpen = $state(false);
	let appVersion = $state('');
	let searchInputEl = $state<HTMLInputElement | null>(null);

	// Bell ring/auto-open: react to the store's bellPing token (bumped on upload
	// start etc.). Open the panel and replay the ring animation.
	let bellRinging = $state(false);
	let lastPing = 0;
	$effect(() => {
		const p = ui.bellPing;
		if (p === lastPing) return;
		lastPing = p;
		if (p === 0) return;
		notifOpen = true;
		menuOpen = false;
		bellRinging = false;
		// Force a reflow gap before re-adding so the CSS animation restarts.
		requestAnimationFrame(() => (bellRinging = true));
		setTimeout(() => (bellRinging = false), 900);
	});

	function openMobileSearch() {
		searchActive = true;
		requestAnimationFrame(() => searchInputEl?.focus());
	}

	function closeMobileSearch() {
		searchActive = false;
		clearSearch();
	}

	async function openAbout() {
		menuOpen = false;
		aboutOpen = true;
		if (!appVersion) {
			try {
				const r = await apiFetch('/api/version', { credentials: 'same-origin' });
				if (r.ok) {
					const data = (await r.json()) as { version?: string };
					if (data.version) appVersion = `v${data.version}`;
				}
			} catch {
				/* version stays blank — non-critical */
			}
		}
	}

	// Top-bar autocomplete
	type Suggestion = { kind: 'folder'; item: FolderItem } | { kind: 'file'; item: FileItem };
	let suggestions = $state<Suggestion[]>([]);
	let suggestOpen = $state(false);
	let suggestBusy = $state(false);
	let suggestTimer: ReturnType<typeof setTimeout> | null = null;
	// Stale-response guard (same family as the search page): the debounce
	// spaces requests out but doesn't stop a SLOW earlier response from
	// resolving after a newer one and overwriting its suggestions.
	let suggestSeq = 0;
	let suggestInflight: AbortController | null = null;

	function goToResults() {
		const q = searchQuery.trim();
		if (!q) return;
		suggestOpen = false;
		searchActive = false;
		// Carry the currently-open folder into the search URL as `?in=<uuid>`
		// so a hard refresh, a shared link, or a bookmark all restore the
		// "This folder" scope. Trash section is always global — skip. See
		// `/search/+page.svelte` for the receiver side.
		//
		// Built by hand instead of via `URLSearchParams` because the Svelte
		// lint (svelte/prefer-svelte-reactivity) flags the mutable stdlib
		// variant; the two params here don't need reactivity anyway.
		const parts = [`q=${encodeURIComponent(q)}`];
		if (filesStore.currentFolder && filesStore.section !== 'trash') {
			parts.push(`in=${encodeURIComponent(filesStore.currentFolder)}`);
		}
		goto(resolve(`/search?${parts.join('&')}`));
	}

	function onSearch(e: SubmitEvent) {
		e.preventDefault();
		goToResults();
	}

	function onSearchInput() {
		if (suggestTimer) clearTimeout(suggestTimer);
		const q = searchQuery.trim();
		if (q.length < 2) {
			suggestSeq++;
			suggestInflight?.abort();
			suggestInflight = null;
			suggestions = [];
			suggestOpen = false;
			return;
		}
		suggestTimer = setTimeout(async () => {
			const seq = ++suggestSeq;
			suggestInflight?.abort();
			const ctl = new AbortController();
			suggestInflight = ctl;
			suggestBusy = true;
			try {
				const r = await searchResources(q, { recursive: true, limit: 9, signal: ctl.signal });
				if (seq !== suggestSeq) return; // superseded while awaiting
				// The wire is ordered — folders first, then files — but slice
				// per kind explicitly so the header preview stays a folder-heavy
				// list even when files dominate the result set.
				const folders = r.items
					.filter((it) => it.resource_type === 'folder')
					.slice(0, 3)
					.map((it) => ({ kind: 'folder' as const, item: it.resource as FolderItem }));
				const files = r.items
					.filter((it) => it.resource_type === 'file')
					.slice(0, 6)
					.map((it) => ({ kind: 'file' as const, item: it.resource as FileItem }));
				suggestions = [...folders, ...files];
				suggestOpen = suggestions.length > 0;
			} catch {
				if (seq !== suggestSeq || ctl.signal.aborted) return;
				suggestions = [];
				suggestOpen = false;
			} finally {
				if (seq === suggestSeq) suggestBusy = false;
			}
		}, 250);
	}

	function clearSearch() {
		searchQuery = '';
		suggestions = [];
		suggestOpen = false;
	}

	function pickSuggestion(s: Suggestion) {
		suggestOpen = false;
		if (s.kind === 'folder') goto(resolve(`/files/${s.item.id}`));
		else window.open(fileInlineUrl(s.item.id), '_blank', 'noopener');
	}

	const THEMES: { mode: Theme; icon: string; label: string }[] = [
		{ mode: 'light', icon: 'sun', label: t('user_menu.theme.light', 'Light') },
		{ mode: 'auto', icon: 'desktop', label: t('user_menu.theme.auto', 'Like OS') },
		{ mode: 'dark', icon: 'moon', label: t('user_menu.theme.dark', 'Dark') }
	];

	const storagePct = $derived.by(() => {
		const full = session.me?.full;
		if (!full || full.storage_quota_bytes <= 0) return 0;
		return Math.min(100, (full.storage_used_bytes / full.storage_quota_bytes) * 100);
	});

	const initials = $derived(userInitials(session.user?.username || session.user?.email));

	/** Uploaded avatar photo URL, if any. */
	const avatarPhoto = $derived(session.user?.image ?? null);

	/** Deterministic colour bucket 0–4 from the user id (shared with UserVignette). */
	const avatarColor = $derived(avatarColorIndex(session.user?.id));

	function closeMenus() {
		notifOpen = false;
		menuOpen = false;
		langOpen = false;
	}

	/**
	 * True when the shortcut target is a text-input surface — <input>,
	 * <textarea>, or any `contenteditable` element. Used by the
	 * Cmd/Ctrl+Shift+. shortcut to defer to normal typing when the
	 * user is composing text (otherwise typing `.` while holding Shift
	 * in a filename dialog would fight the shortcut).
	 */
	function isTextFieldFocused(target: EventTarget | null): boolean {
		if (!(target instanceof HTMLElement)) return false;
		const tag = target.tagName;
		return tag === 'INPUT' || tag === 'TEXTAREA' || target.isContentEditable;
	}

	async function chooseLocale(loc: Locale) {
		langOpen = false;
		await setLocale(loc);
	}

	const currentLang = $derived(LANGUAGES.find((l) => l.code === i18n.locale) ?? LANGUAGES[0]);

	function formatTime(ms: number): string {
		return dateTimeFormatFor(undefined, { hour: '2-digit', minute: '2-digit' }).format(ms);
	}

	function notifIcon(kind: string): string {
		switch (kind) {
			case 'success':
				return 'check';
			case 'error':
				return 'exclamation-circle';
			case 'warning':
				return 'exclamation-triangle';
			default:
				return 'info-circle';
		}
	}

	async function onLogout() {
		// Flip the session-teardown gate BEFORE the logout POST so every
		// ambient/subscriber-fired fetch that fires between here and the
		// /login mount short-circuits with AbortError instead of hitting
		// the server (see `client.ts::logoutInProgress`). Left ON across
		// the goto — module state persists over soft nav, so a stale
		// reactive re-fetch during the transition still no-ops. A hard
		// reload later (or the IdP round-trip below) wipes it naturally.
		setLogoutInProgress(true);
		let postLogoutUrl: string | undefined;
		try {
			({ postLogoutUrl } = await logout());
		} catch {
			/* clear locally regardless */
		}
		if (postLogoutUrl) {
			// Full-page navigation to the IdP end-session endpoint. Do NOT
			// touch local session state first: `session.reset()` fires the
			// layout $effect guard which races us with a competing
			// `goto('/login?redirect=...')`. The IdP round-trip lands us
			// back on `/login` where the SPA reboots fresh from scratch —
			// no local cleanup needed here.
			window.location.replace(postLogoutUrl);
			return;
		}
		session.reset();
		// `?source=logged_out` distinguishes the friendly explicit-logout
		// landing from `?source=session_expired` (auto-divert on 401 →
		// refresh 401). The login page reads the flag, shows the success
		// notice, and skips its existing-session probe.
		await goto(resolve('/login?source=logged_out'));
	}
</script>

<svelte:window
	onclick={closeMenus}
	onkeydown={(e) => {
		// First Cmd/Ctrl+K loads the palette and mounts it open; once mounted,
		// the palette's own handler takes over toggling/closing.
		if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k' && !palette.component) {
			e.preventDefault();
			void palette.load();
			return;
		}
		// Cmd/Ctrl+Shift+. toggles dotfile visibility — matches macOS
		// Finder's convention. `e.code === 'Period'` targets the
		// physical key regardless of keyboard layout (Cmd+Shift+.
		// yields `.key === '>'` on some layouts). Skip when focus is
		// inside a text field so users can still type `.` in inputs.
		if (
			(e.metaKey || e.ctrlKey) &&
			e.shiftKey &&
			e.code === 'Period' &&
			!isTextFieldFocused(e.target)
		) {
			e.preventDefault();
			preferences.toggleHideDotfiles();
			ui.notify(
				preferences.hideDotfiles
					? t('files.dotfiles_hidden_toast', 'Dotfiles hidden')
					: t('files.dotfiles_shown_toast', 'Dotfiles shown'),
				'info',
				2000,
				false
			);
			return;
		}
		if (e.key !== 'Escape') return;
		if (aboutOpen) aboutOpen = false;
		else if (searchActive) closeMobileSearch();
		else closeMenus();
	}}
/>

<div
	class="sidebar-overlay"
	class:active={sidebarOpen}
	onclick={() => (sidebarOpen = false)}
	role="presentation"
></div>

<div class="sidebar" class:open={sidebarOpen}>
	<a href={resolve('/files')} class="logo-container" data-testid="appshell-logo-link">
		<div class="logo">
			<svg viewBox="95 67 320 320" aria-hidden="true">
				<path
					d="M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z"
				/>
			</svg>
		</div>
		<div class="app-name">OxiCloud</div>
	</a>

	<nav
		class="nav-menu"
		class:nav-menu--admin={isAdminSection}
		aria-label={isAdminSection
			? t('nav.admin_sections', 'Admin sections')
			: t('nav.primary', 'Primary')}
	>
		{#if isAdminSection}
			<!--
			  "Back to app" escape hatch — the sidebar swaps to
			  admin mode when the URL is under /admin/*, so the
			  usual Files/Shared links disappear. A dedicated back
			  entry gives the operator a one-click way out without
			  having to hunt through the user menu.
			-->
			<a
				class="nav-item nav-item--back"
				href={resolve('/files')}
				data-testid="appshell-nav-admin-back-link"
				onclick={() => (sidebarOpen = false)}
			>
				<Icon name="arrow-left" />
				<span>{t('nav.back_to_app', 'Back to OxiCloud')}</span>
			</a>
		{/if}
		<!--
		  Sidebar links come from data-driven arrays (LINKS,
		  ADMIN_LINKS), so hrefs are runtime strings rather than
		  compile-time-known route keys. `navHref()` internally
		  routes them through `resolve()`, but ESLint's static
		  check can't see through the wrapper. Disabling the
		  rule here rather than sprinkling per-line directives.
		-->
		<!-- eslint-disable svelte/no-navigation-without-resolve -->
		{#each currentLinks as link (link.href)}
			{@const isDropTarget =
				!isAdminSection && (link.href === '/favorites' || link.href === '/trash')}
			<a
				class="nav-item"
				class:active={activeLink(link.href)}
				class:nav-item--drop-target={isDropTarget && sidebarDropHref === link.href}
				href={navHref(link.href)}
				data-section={link.section}
				data-testid={`appshell-nav-${link.section}-link`}
				onclick={() => (sidebarOpen = false)}
				ondragover={isDropTarget ? (e) => sidebarOnDragOver(e, link.href) : undefined}
				ondragleave={isDropTarget ? () => sidebarOnDragLeave(link.href) : undefined}
				ondrop={isDropTarget ? (e) => void sidebarOnDrop(e, link.href) : undefined}
			>
				<Icon name={link.icon} />
				<span>{link.label}</span>
			</a>
			{#if !isAdminSection && link.href === '/files' && !session.isExternalUser}
				<DrivePicker onnavigate={() => (sidebarOpen = false)} />
			{/if}
		{/each}
		<!-- eslint-enable svelte/no-navigation-without-resolve -->
	</nav>

	{#if session.user}
		<div class="storage-container">
			<div class="storage-title">
				<Icon name="database" /> <span>{t('storage.title', 'Storage')}</span>
			</div>
			<div class="storage-bar">
				<div class="storage-fill" style:width="{storagePct}%"></div>
			</div>
			<div class="storage-info">
				{#if (session.me?.full.storage_quota_bytes ?? 0) > 0}
					{Math.round(storagePct)}% · {formatBytes(session.me?.full.storage_used_bytes ?? 0)} / {formatBytes(
						session.me?.full.storage_quota_bytes ?? 0
					)}
				{:else}
					{formatBytes(session.me?.full.storage_used_bytes ?? 0)}
				{/if}
			</div>
		</div>
	{/if}
</div>

<div class="main-content">
	<div class="top-bar" class:top-bar--search-active={searchActive}>
		<button
			class="sidebar-toggle"
			aria-label={t('nav.toggle', 'Toggle navigation menu')}
			aria-expanded={sidebarOpen}
			data-testid="appshell-sidebar-toggle-btn"
			onclick={() => (sidebarOpen = !sidebarOpen)}
		>
			<Icon name="bars" />
		</button>

		<!-- Mobile: icon-only button that expands the full-width search overlay. -->
		<button
			class="search-toggle-btn"
			id="search-toggle-btn"
			aria-label={t('actions.search_btn', 'Search')}
			data-testid="appshell-search-toggle-btn"
			onclick={openMobileSearch}
		>
			<Icon name="search" />
		</button>

		<!-- Mobile: back arrow shown while the search overlay is expanded. -->
		<button
			class="search-back-btn"
			aria-label={t('common.close', 'Close')}
			data-testid="appshell-search-back-btn"
			onclick={closeMobileSearch}
		>
			<Icon name="arrow-left" />
		</button>

		<div class="search-slot">
			<form class="search-container" onsubmit={onSearch}>
				<Icon name="search" class="search-icon" />
				<input
					type="text"
					bind:this={searchInputEl}
					bind:value={searchQuery}
					data-testid="appshell-search-input"
					oninput={onSearchInput}
					onfocus={() => (suggestOpen = suggestions.length > 0)}
					onblur={() => setTimeout(() => (suggestOpen = false), 150)}
					placeholder={t('actions.search', 'Search files, folders...')}
					autocomplete="off"
				/>
				{#if searchQuery}
					<button
						class="search-clear"
						type="button"
						title={t('common.clear', 'Clear')}
						aria-label={t('common.clear', 'Clear')}
						data-testid="appshell-search-clear-btn"
						onclick={clearSearch}
					>
						<Icon name="times" />
					</button>
				{/if}
				<button
					class="search-button"
					type="submit"
					title={t('actions.search_btn', 'Search')}
					aria-label={t('actions.search_btn', 'Search')}
					data-testid="appshell-search-submit-btn"
				>
					<Icon name="search" />
				</button>

				{#if suggestOpen}
					<ul class="suggest">
						{#each suggestions as s (s.kind + s.item.id)}
							<li>
								<button
									class="suggest__item"
									type="button"
									data-testid={`appshell-search-suggestion-${s.kind}-${s.item.id}-item`}
									onmousedown={() => pickSuggestion(s)}
								>
									<span class="suggest__icon">
										{#if s.kind === 'folder'}
											<Icon name="folder" />
										{:else}
											<Icon name={iconNameFromClass(s.item.icon_class)} />
										{/if}
									</span>
									<span class="suggest__name">{s.item.name}</span>
								</button>
							</li>
						{/each}
						<li>
							<button
								class="suggest__all"
								type="button"
								data-testid="appshell-search-see-all-btn"
								onmousedown={goToResults}
							>
								{t('search.see_all', 'See all results')}
							</button>
						</li>
					</ul>
				{:else if suggestBusy}
					<ul class="suggest"><li class="suggest__busy">{t('common.loading', 'Loading…')}</li></ul>
				{/if}
			</form>
		</div>

		<div class="user-controls">
			<!-- Notifications -->
			<div class="notif-wrapper" class:open={notifOpen} data-testid="appshell-notif-menu">
				<button
					class="notif-bell-btn"
					class:active={notifOpen}
					class:ring={bellRinging}
					aria-label={t('notifications.title', 'Notifications')}
					aria-haspopup="true"
					data-testid="appshell-notif-bell-btn"
					onclick={(e) => {
						e.stopPropagation();
						notifOpen = !notifOpen;
						menuOpen = false;
						if (notifOpen) ui.markNotificationsRead();
					}}
				>
					<Icon name="bell" />
					{#if ui.unread > 0}<span class="notif-badge">{ui.unreadBadge}</span>{/if}
				</button>
				<div class="notif-panel">
					<div class="notif-panel-header">
						<span class="notif-panel-title">{t('notifications.title', 'Notifications')}</span>
						{#if ui.notifications.length > 0}
							<button
								class="notif-clear-btn"
								title={t('notifications.clear', 'Clear all')}
								aria-label={t('notifications.clear', 'Clear all')}
								data-testid="appshell-notif-clear-btn"
								onclick={(e) => {
									e.stopPropagation();
									ui.clearNotifications();
								}}
							>
								<Icon name="trash-alt" />
							</button>
						{/if}
					</div>
					<div class="notif-panel-body">
						{#if ui.notifications.length === 0}
							<div class="notif-empty">
								<Icon name="bell-slash" />
								<span>{t('notifications.empty', 'No notifications')}</span>
							</div>
						{:else}
							{#each ui.notifications as n (n.id)}
								<div class="notif-item notif-item--{n.kind}">
									<span class="notif-item-icon"><Icon name={n.icon ?? notifIcon(n.kind)} /></span>
									<div class="notif-item-body">
										<div class="notif-item-text">{n.message}</div>
										{#if n.currentFile}
											<div class="notif-item-current" title={n.currentFile}>{n.currentFile}</div>
										{/if}
										{#if n.progress !== undefined}
											<div
												class="notif-progress"
												role="progressbar"
												aria-valuenow={n.progress}
												aria-valuemin="0"
												aria-valuemax="100"
											>
												<div class="notif-progress__fill" style:width="{n.progress}%"></div>
											</div>
											<div class="notif-progress-detail">
												<span>{n.progress}%</span>
												{#if n.total !== undefined}
													<span>
														{t(
															'upload.files_counter',
															{ done: n.completed ?? 0, total: n.total },
															`${n.completed ?? 0} / ${n.total} files`
														)}
													</span>
												{/if}
											</div>
										{:else}
											<div class="notif-item-time">{formatTime(n.at)}</div>
										{/if}
									</div>
								</div>
							{/each}
						{/if}
					</div>
				</div>
			</div>

			<!-- User menu -->
			<div class="user-menu-wrapper" class:open={menuOpen} data-testid="appshell-user-menu">
				<button
					class="user-avatar-btn"
					aria-label={t('user_menu.title', 'User menu')}
					aria-haspopup="true"
					data-testid="appshell-user-menu-btn"
					onclick={(e) => {
						e.stopPropagation();
						menuOpen = !menuOpen;
						notifOpen = false;
					}}
				>
					{@render avatar(false)}
				</button>

				<div class="user-menu">
					{#if session.user}
						<div class="user-menu-header">
							{@render avatar(true)}
							<div class="user-menu-id">
								<div class="user-menu-name">{session.user.username || session.user.email}</div>
								<div class="user-menu-email">{session.user.email}</div>
							</div>
						</div>

						{#if isAdmin}
							<div class="user-menu-role-badge">
								<span class="role-badge role-badge-admin">
									<Icon name="shield-alt" />
									{t('user_menu.admin', 'Admin')}
								</span>
							</div>
						{/if}

						<div class="user-menu-storage">
							<div class="user-menu-storage-label">
								<Icon name="database" /> <span>{t('storage.title', 'Storage')}</span>
							</div>
							<div class="user-menu-storage-bar">
								<div class="user-menu-storage-fill" style:width="{storagePct}%"></div>
							</div>
							<div class="user-menu-storage-text">
								{#if (session.me?.full.storage_quota_bytes ?? 0) > 0}
									{t(
										'storage.used',
										{
											percentage: Math.round(storagePct),
											used: formatBytes(session.me?.full.storage_used_bytes ?? 0),
											total: formatBytes(session.me?.full.storage_quota_bytes ?? 0)
										},
										'{{percentage}}% used ({{used}} / {{total}})'
									)}
								{:else}
									{formatBytes(session.me?.full.storage_used_bytes ?? 0)}
								{/if}
							</div>
						</div>
					{/if}

					<div class="user-menu-divider"></div>

					{#if isAdmin}
						<a
							class="user-menu-item"
							href={resolve('/admin')}
							data-testid="appshell-user-menu-admin-item"
							onclick={() => (menuOpen = false)}
						>
							<Icon name="cogs" /> <span>{t('user_menu.admin_panel', 'Admin panel')}</span>
						</a>
						<a
							class="user-menu-item"
							href={resolve('/groups')}
							data-testid="appshell-user-menu-groups-item"
							onclick={() => (menuOpen = false)}
						>
							<Icon name="user-group" />
							<span>{t('user_menu.manage_groups', 'Manage groups')}</span>
						</a>
					{/if}
					<a
						class="user-menu-item"
						href={resolve('/profile')}
						data-testid="appshell-user-menu-profile-item"
						onclick={() => (menuOpen = false)}
					>
						<Icon name="user-circle" /> <span>{t('user_menu.profile', 'My profile')}</span>
					</a>

					<div class="user-menu-divider"></div>

					<div class="user-menu-item user-menu-item--lang">
						<Icon name="globe" />
						<span>{t('settings.language', 'Language')}</span>
						<div
							class="lang-selector"
							class:lang-selector--open={langOpen}
							data-testid="appshell-lang-menu"
						>
							<button
								type="button"
								class="lang-selector__toggle"
								aria-haspopup="listbox"
								aria-expanded={langOpen}
								data-testid="appshell-lang-toggle-btn"
								onclick={(e) => {
									e.stopPropagation();
									langOpen = !langOpen;
								}}
							>
								<span class="lang-selector__code">{(currentLang.code as string).toUpperCase()}</span
								>
								<Icon name="chevron-down" class="lang-selector__arrow" />
							</button>
							{#if langOpen}
								<ul class="lang-selector__dropdown" role="listbox">
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

					<button
						class="user-menu-item"
						data-testid="appshell-user-menu-about-item"
						onclick={openAbout}
					>
						<Icon name="info-circle" /> <span>{t('user_menu.about', 'About OxiCloud')}</span>
					</button>

					<div class="user-menu-divider"></div>

					<button
						class="user-menu-item user-menu-logout"
						data-testid="appshell-user-menu-logout-btn"
						onclick={onLogout}
					>
						<Icon name="sign-out-alt" /> <span>{t('actions.logout', 'Log out')}</span>
					</button>
				</div>
			</div>
		</div>
	</div>

	<div class="content-area">
		<!-- Server-wide maintenance banner. Fed by the
		     `x-server-status` header read on every API response by
		     `apiFetch` — no polling. Shows for every logged-in user
		     while a storage migration is running so they know why
		     writes are being refused, with live progress if
		     available. Disappears automatically on the next API
		     round-trip after the server clears the flag.

		     Reuses `ReadOnlyBanner` (same component that renders a
		     drive-frozen notice) with `variant="maintenance"` so the
		     two banners are visually indistinguishable — just
		     different copy. -->
		{#if serverStatus().readonly}
			<ReadOnlyBanner variant="maintenance" progress={serverStatus().migration} />
		{:else if serverStatus().rotation}
			<!-- K4 storage-key-rotation: rotation is running but
			     `readonly` is false — writes continue as normal.
			     Distinct banner variant so the copy reads
			     "background maintenance" rather than "server
			     frozen". -->
			<ReadOnlyBanner variant="rotating" progress={serverStatus().rotation} />
		{/if}
		{@render children()}
	</div>
</div>

{#snippet avatar(large: boolean)}
	{#if avatarPhoto}
		<img
			class="avatar avatar--photo"
			class:avatar--lg={large}
			src={avatarPhoto}
			alt={t('user_menu.title', 'User menu')}
		/>
	{:else}
		<span class="avatar avatar--c{avatarColor}" class:avatar--lg={large}>{initials}</span>
	{/if}
{/snippet}

{#if aboutOpen}
	<!-- About OxiCloud modal -->
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div class="about-overlay" onclick={(e) => e.target === e.currentTarget && (aboutOpen = false)}>
		<div class="about-modal" role="dialog" aria-modal="true" aria-labelledby="about-modal-title">
			<div class="about-modal__logo">
				<svg viewBox="95 67 320 320" aria-hidden="true">
					<path
						d="M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z"
					/>
				</svg>
			</div>
			<h2 id="about-modal-title" class="about-modal__title">OxiCloud</h2>
			<div class="about-modal__version">{appVersion || 'v…'}</div>
			<p class="about-modal__desc">
				{t(
					'user_menu.about_description',
					'Cloud storage platform built with Rust & Clean Architecture. Fast, secure, and private.'
				)}
			</p>
			<div class="about-modal__tech">
				<span class="about-modal__badge">Rust</span>
				<span class="about-modal__badge">Axum</span>
				<span class="about-modal__badge">PostgreSQL</span>
				<span class="about-modal__badge">Clean Architecture</span>
			</div>
			<div class="about-modal__links">
				<a
					class="about-modal__link"
					href="https://github.com/AtalayaLabs/OxiCloud/"
					target="_blank"
					rel="noopener"
					data-testid="appshell-about-github-link"
				>
					<Icon name="github" /> GitHub
				</a>
				<a
					class="about-modal__link"
					href="https://github.com/AtalayaLabs/OxiCloud/blob/main/LICENSE"
					target="_blank"
					rel="noopener"
					data-testid="appshell-about-license-link"
				>
					<Icon name="file-alt" />
					{t('user_menu.mit_license', 'MIT License')}
				</a>
			</div>
			<button
				class="about-modal__close"
				data-testid="appshell-about-close-btn"
				onclick={() => (aboutOpen = false)}
			>
				{t('actions.close', 'Close')}
			</button>
		</div>
	</div>
{/if}

{#if palette.component}
	{@const CommandPalette = palette.component}
	<CommandPalette autoOpen />
{/if}

<style>
	/* Body becomes the sidebar+main flex row only while the shell is mounted. */
	:global(body) {
		display: flex;
	}

	/* Clear (×) button sits left of the submit button inside the search field. */
	.search-clear {
		position: absolute;
		right: 44px;
		display: grid;
		place-items: center;
		width: 28px;
		height: 28px;
		border: none;
		border-radius: 50%;
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.search-clear:hover {
		background: var(--color-bg-hover);
		color: var(--color-text);
	}

	.suggest {
		position: absolute;
		top: calc(100% + 4px);
		left: 0;
		right: 0;
		/* Search suggestions render above `.page-sticky-header` — otherwise the
		   dropdown clips under the action bar on the content pages. Design-token
		   `--z-dropdown` (1000) sits above `--z-sticky` (100) by construction. */
		z-index: var(--z-dropdown);
		list-style: none;
		margin: 0;
		padding: 0.25rem;
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg, var(--radius-md));
		box-shadow: var(--shadow-lg, 0 10px 30px var(--color-overlay-shadow));
		max-height: 24rem;
		overflow: auto;
	}

	.suggest__item,
	.suggest__all {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		width: 100%;
		padding: 0.5rem 0.6rem;
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		text-align: left;
		border-radius: var(--radius-sm);
	}

	.suggest__item:hover,
	.suggest__all:hover {
		background: var(--color-bg-hover);
	}

	.suggest__all {
		justify-content: center;
		color: var(--color-primary);
		border-top: 1px solid var(--color-border);
		margin-top: 0.25rem;
	}

	.suggest__icon {
		color: var(--color-text-muted);
	}

	.suggest__name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.suggest__busy {
		padding: 0.6rem;
		color: var(--color-text-muted);
		text-align: center;
	}

	.notif-progress {
		height: 6px;
		margin-top: 0.35rem;
		border-radius: var(--radius-pill, 999px);
		background: var(--color-bg-muted);
		overflow: hidden;
	}

	.notif-progress__fill {
		height: 100%;
		background: var(--color-accent);
		transition: width 0.2s ease;
	}

	.notif-progress-detail {
		display: flex;
		justify-content: space-between;
		gap: 0.5rem;
		margin-top: 0.25rem;
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
	}

	.notif-item-current {
		margin-top: 0.15rem;
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Bell "ring" animation, replayed when bellRinging toggles on. */
	.notif-bell-btn.ring :global(svg),
	.notif-bell-btn.ring :global(i) {
		transform-origin: top center;
		animation: bell-ring 0.9s ease;
	}

	@keyframes bell-ring {
		0%,
		100% {
			transform: rotate(0);
		}

		10%,
		30%,
		50% {
			transform: rotate(12deg);
		}

		20%,
		40%,
		60% {
			transform: rotate(-12deg);
		}

		70% {
			transform: rotate(6deg);
		}

		80% {
			transform: rotate(-6deg);
		}
	}

	/* Avatar: deterministic coloured-initials vignette, or the uploaded photo. */
	.avatar {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 36px;
		height: 36px;
		border-radius: 50%;
		background: var(--color-accent-gradient, var(--color-accent));
		color: var(--color-on-accent);
		font-size: var(--text-sm);
		font-weight: var(--weight-bold);
		flex-shrink: 0;
		overflow: hidden;
	}

	.avatar--photo {
		object-fit: cover;
	}

	/* Colour buckets mirror userVignette's .uv-color-0..4 shared palette. */
	.avatar--c0 {
		background: var(--color-badge-indigo-bg);
		color: var(--color-badge-indigo-text);
	}

	.avatar--c1 {
		background: var(--color-badge-green-bg);
		color: var(--color-badge-green-text);
	}

	.avatar--c2 {
		background: var(--color-badge-orange-bg);
		color: var(--color-badge-orange-text);
	}

	.avatar--c3 {
		background: var(--color-badge-blue-bg);
		color: var(--color-badge-blue-text);
	}

	.avatar--c4 {
		background: var(--color-badge-amber-bg);
		color: var(--color-badge-amber-text);
	}

	.avatar--lg {
		width: 44px;
		height: 44px;
		font-size: var(--text-base);
	}

	.user-menu-header {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-4);
	}

	.user-menu-id {
		min-width: 0;
	}

	.user-menu-name {
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
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

	.lang-selector__code {
		font-weight: var(--weight-semibold);
	}

	:global(.lang-selector__arrow) {
		font-size: var(--text-xs, 0.7rem);
		transition: transform 0.15s ease;
	}

	.lang-selector--open :global(.lang-selector__arrow) {
		transform: rotate(180deg);
	}

	.lang-selector__dropdown {
		position: absolute;
		bottom: calc(100% + 4px);
		right: 0;
		/* Sits above `--z-sticky` for the same reason as `.suggest` above. */
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

	:global([dir='rtl']) .lang-selector__dropdown {
		right: auto;
		left: 0;
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

	/* About OxiCloud modal. */
	.about-overlay {
		position: fixed;
		inset: 0;
		z-index: var(--z-modal);
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 1rem;
		background: var(--color-overlay);
		animation: about-fade 0.18s ease;
	}

	.about-modal {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-3);
		width: min(92vw, 26rem);
		padding: var(--space-6) var(--space-5);
		background: var(--color-bg-surface);
		color: var(--color-text);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-xl);
		text-align: center;
		animation: about-pop 0.2s ease;
	}

	.about-modal__logo {
		/* 73px (not 64) so the cloud keeps its rendered scale after the viewBox
		   grew 280→320 to stop clipping its left bulge: 73/320 ≈ 64/280. */
		width: 73px;
		height: 73px;
		color: var(--color-accent);
	}

	.about-modal__logo svg {
		width: 100%;
		height: 100%;
		fill: currentColor;
	}

	.about-modal__title {
		margin: 0;
		font-size: var(--text-xl, 1.5rem);
		color: var(--color-text-heading);
	}

	.about-modal__version {
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}

	.about-modal__desc {
		margin: 0;
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		line-height: 1.5;
	}

	.about-modal__tech {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		gap: var(--space-2);
	}

	.about-modal__badge {
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius-pill, 999px);
		background: var(--color-bg-muted);
		color: var(--color-text-secondary, var(--color-text-muted));
		font-size: var(--text-xs, 0.75rem);
	}

	.about-modal__links {
		display: flex;
		gap: var(--space-4);
	}

	.about-modal__link {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		color: var(--color-primary);
		text-decoration: none;
		font-size: var(--text-sm);
	}

	.about-modal__link:hover {
		text-decoration: underline;
	}

	.about-modal__close {
		margin-top: var(--space-2);
		padding: var(--space-2) var(--space-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		cursor: pointer;
		font-size: var(--text-sm);
	}

	.about-modal__close:hover {
		background: var(--color-bg-hover);
	}

	@keyframes about-fade {
		from {
			opacity: 0;
		}

		to {
			opacity: 1;
		}
	}

	@keyframes about-pop {
		from {
			opacity: 0;
			transform: scale(0.96);
		}

		to {
			opacity: 1;
			transform: scale(1);
		}
	}
</style>
