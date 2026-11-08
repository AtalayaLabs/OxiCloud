<script lang="ts">
	import { appPath } from '$lib/utils/appPath';
	import { onMount, type Snippet } from 'svelte';
	import { messageBus } from '$lib/message-bus/client.svelte';
	import { refetchServerStatusConfig } from '$lib/stores/serverStatus.svelte';
	import log from 'loglevel';
	// Logger for the server-status realtime push + refetch path.
	// Silent by default; operators enable with
	// `oxi.setLogLevel('oxi:server-status', 'debug')` in devtools to
	// trace whether the subscribe fired, whether rt.event arrived,
	// and what /api/config returned on refetch.
	const ssLog = log.getLogger('oxi:server-status');
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { searchResources } from '$lib/api/endpoints/search';
	import { deleteFile, moveFile } from '$lib/api/endpoints/files';
	import { deleteFolder, moveFolder } from '$lib/api/endpoints/folders';
	import { addFavorite } from '$lib/api/endpoints/favorites';
	import { copyFiles, copyFolders } from '$lib/api/endpoints/batch';
	import { isCopyDrop } from '$lib/utils/dragModifiers';
	import type { Drive } from '$lib/api/types';
	import type { FileItem, FolderItem, ItemType } from '$lib/api/types';
	import { isAtLeastAdmin } from '$lib/utils/roles';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import DrivePicker from '$lib/components/DrivePicker.svelte';
	import BrandMark from '$lib/components/BrandMark.svelte';
	import Banner from '$lib/components/Banner.svelte';
	import AboutDialog from '$lib/components/AboutDialog.svelte';
	import MobileNav from '$lib/components/MobileNav.svelte';
	import SelfAvatar from '$lib/components/SelfAvatar.svelte';
	import TransientNotificationRow from '$lib/components/TransientNotificationRow.svelte';
	import UserMenuContent from '$lib/components/UserMenuContent.svelte';
	import { renderOpsBannerMarkdown } from '$lib/utils/opsBannerMarkdown';
	import { adminAdvisories } from '$lib/stores/adminAdvisories.svelte';
	// Deliberate: no `onMount` import for a localStorage-backed
	// dismiss set. Ops banners are not user-dismissable — the server
	// is the single source of truth for what's visible. If a banner
	// is naggy, the operator deletes it, and that change propagates
	// to every open tab via the message-bus push + version-diff
	// refetch. No client-side per-user state → no stale-id leak.
	import Icon from '$lib/icons/Icon.svelte';
	import { iconNameFor } from '$lib/utils/display';
	import { t } from '$lib/i18n/index.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';
	import { serverStatus } from '$lib/stores/serverStatus.svelte';
	import { dialogs } from '$lib/stores/dialogs.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { session } from '$lib/stores/session.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import {
		notifications as persistentNotifications,
		useNotifications
	} from '$lib/composables/useNotifications.svelte';
	import NotificationRow from '$lib/components/NotificationRow.svelte';
	import { errorToast } from '$lib/utils/errors';
	import { formatBytes, storagePercent } from '$lib/utils/format';

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

	interface NavGroup {
		label: string | null;
		links: NavLink[];
		/** Indents the rows one step so they read as children of the header. */
		nested?: boolean;
	}

	// `$derived.by` so feature-flag gating drops entries when a feature is
	// disabled server-side. Mirrors the ADMIN_LINKS pattern below: the
	// backend's `/api/{photos,music,trash,shares,search}/*` routes are
	// also gated on these flags, so hiding the sidebar link keeps the
	// SPA consistent with what the backend serves — a stale link would
	// land on a 404. See `$lib/stores/serverConfig.svelte.ts` and
	// `src/common/di.rs` for the server-side side of each gate.
	const NAV_GROUPS = $derived.by<NavGroup[]>(() => {
		const library: NavLink[] = [
			{ href: '/recent', label: t('nav.recent', 'Recent'), icon: 'clock', section: 'recent' },
			{
				href: '/favorites',
				label: t('nav.favorites', 'Favorites'),
				icon: 'star',
				section: 'favorites'
			}
		];
		if (serverConfig.features.photos) {
			library.push({
				href: '/photos',
				label: t('nav.photos', 'Media'),
				icon: 'images',
				section: 'photos'
			});
		}
		if (serverConfig.features.music) {
			library.push({
				href: '/music',
				label: t('nav.music', 'Music'),
				icon: 'music',
				section: 'music'
			});
		}

		const sharing: NavLink[] = [];
		if (serverConfig.features.sharing) {
			sharing.push(
				{
					href: '/shared',
					label: t('nav.shared', 'My shares'),
					icon: 'oxiexport',
					section: 'shared'
				},
				{
					href: '/shared-with-me',
					label: t('nav.shared_with_me', 'Shared with me'),
					icon: 'oxiimport',
					section: 'shared-with-me'
				}
			);
		}
		// `features.contacts`, NOT `features.carddav`: that flag governs the
		// DAV sync endpoint, this one governs `/api/contacts` + this page,
		// and a deployment may want either without the other.
		if (serverConfig.features.contacts) {
			sharing.push({
				href: '/contacts',
				label: t('nav.contacts', 'Contacts'),
				icon: 'id-card',
				section: 'contacts'
			});
		}

		const groups: NavGroup[] = [
			{
				label: null,
				links: [
					{ href: '/files', label: t('nav.files', 'Files'), icon: 'folder', section: 'files' }
				]
			},
			{ label: t('nav.section_library', 'Library'), nested: true, links: library }
		];
		// Pushed unconditionally — the `filter` at the end of this block is
		// what drops it when `features.sharing` and `features.contacts` are
		// both off. This used to carry its own `if (sharing.length > 0)`
		// guard, which made the emptiness rule exist in two places: here
		// by hand, and there structurally. One enforcement point means a
		// new group cannot forget it, and it keeps the filter load-bearing
		// rather than belt-and-braces that no test can reach.
		groups.push({
			label: t('nav.section_sharing', 'Sharing'),
			nested: true,
			links: sharing
		});
		// ── TO THE /calendar IMPLEMENTER (PR #853) ───────────────────────
		// `serverConfig.features.calendars` already exists end to end —
		// `OXICLOUD_ENABLE_CALENDARS` → `FeaturesConfig::enable_calendars`
		// → `/api/config.features.calendars` → the generated SDK type.
		// It was added ahead of the feature so nothing about the flag has
		// to be invented while landing the page. Add the nav entry here,
		// gated the same way its neighbours are:
		//
		//   if (serverConfig.features.calendars) {
		//     groups.push({
		//       href: '/calendars',
		//       label: t('nav.calendars', 'Calendars'),
		//       icon: 'calendar',
		//       section: 'calendars'
		//     });
		//   }
		//
		// ⚠ PLURAL route, to match `/contacts` and `/photos` above, the
		// `features.calendars` flag and the `/api/calendars` REST surface.
		// PR #853 as currently written is split on this — its API is
		// `/api/calendars` but its page is `/calendar`. Settle it one way
		// when landing; this file's convention is that `section` equals
		// the last path segment, which the `appshell-nav-${section}-link`
		// test id depends on.
		//
		// Two things to do with it beyond this block: gate the
		// `/api/calendars` router in `routes.rs` on
		// `features.enable_calendars` (mirror the `/api/contacts` gate
		// above it) so non-SPA clients get a 404 rather than a surface
		// the SPA merely hides, and add the hide/show pins to
		// `AppShell.test.ts` next to the contacts ones. Gate on
		// `calendars`, NOT `caldav` — that is the sync protocol.
		// https://github.com/AtalayaLabs/OxiCloud/pull/853
		if (serverConfig.features.trash) {
			groups.push({
				label: null,
				links: [{ href: '/trash', label: t('nav.trash', 'Trash'), icon: 'trash', section: 'trash' }]
			});
		}
		// Drop any group whose every link was feature-flagged away. ONE
		// enforcement point, so a group added later cannot forget it —
		// `Sharing` carried its own guard until this replaced it.
		//
		// This is not cosmetic. The render keys the group list on
		// `group.links[0].href`, so an empty group makes `links[0]`
		// undefined and that dereference a TypeError — which takes the
		// whole sidebar down on every page, not just the affected
		// section. `Sharing` empties for real whenever `features.sharing`
		// and `features.contacts` are both off; `Library` survives only
		// because `/recent` and `/favorites` are unconditional while
		// Photos and Music are flagged, which is luck rather than design
		// and stops being true the day either gets a flag (see the
		// `features.calendars` note above for how often that happens).
		return groups.filter((g) => g.links.length > 0);
	});

	// Admin sidebar — populated when the URL is under /admin. The
	// admin +page.svelte used to render its own horizontal tab
	// strip; that was displaced here so the section navigation
	// scales past ~7 items and matches deep-link URLs from the
	// address bar.
	// `$derived` so feature-flag gating drops entries when a feature is
	// disabled server-side. Server-side the admin CRUD routes are also
	// gated (matching the message-bus pattern) — hiding the link here
	// keeps the sidebar consistent with what the backend actually
	// serves; a stale link would land on a 404. See
	// `$lib/stores/serverConfig.svelte.ts`.
	const ADMIN_LINKS = $derived.by<NavLink[]>(() => {
		const links: NavLink[] = [
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
				href: '/admin/policies',
				label: t('admin.drive_policies.tab', 'Drive Policies'),
				icon: 'shield-alt',
				section: 'admin-policies'
			}
		];
		if (serverConfig.features.external_mounts) {
			links.push({
				href: '/admin/mounts',
				label: t('admin.mounts', 'External Mounts'),
				icon: 'folder',
				section: 'admin-mounts'
			});
		}
		links.push(
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
				// Was /admin/smtp. The page covers every outbound transport
				// now — SMTP and webhook — so it is named for the question an
				// operator arrives with rather than for one answer.
				href: '/admin/notification',
				label: t('admin.notifications', 'Notifications'),
				icon: 'bell',
				section: 'admin-notification'
			},
			{
				// Runtime-state controls: backend write-lock,
				// metadata re-extract backfills, future ops banners.
				// Separated from /admin/dashboard (monitoring, read)
				// so "what I'm doing to the server right now" has one
				// home.
				href: '/admin/maintenance',
				label: t('admin.maintenance_tab', 'Maintenance'),
				icon: 'cog',
				section: 'admin-maintenance'
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
		);
		return links;
	});

	const isAdmin = $derived(isAtLeastAdmin(session.user?.role));

	// ── Server-status realtime push ──
	// Subscribe to `Topic::ServerStatus` so admin-posted banner
	// changes land within ~1s instead of waiting for this tab's
	// next natural API call to pick up the version-diff on
	// X-Server-Status. Falls back cleanly when the message bus is
	// disabled (`OXICLOUD_MESSAGEBUS_ENABLE=false`): the header-diff
	// path still works, just with per-tab-action latency.
	//
	// Subscribe is Public-authz'd (any session), so no per-user
	// predicate here. One refcount for the whole page session; the
	// release handle fires on component destroy.
	onMount(() => {
		if (!serverConfig.features.message_bus) {
			ssLog.debug('bus disabled server-side; skipping subscribe');
			return;
		}
		ssLog.debug('subscribing to server:status');
		const release = messageBus.subscribe('server:status', (params) => {
			ssLog.debug('rt.event received', params);
			if (params.event === 'server_status_changed') {
				// Push just signals "something changed, go look".
				// The actual banner bodies / write-lock holder /
				// migration progress all come from `/api/config`
				// via the same code path the header-diff fallback
				// uses — one source of truth for the full state.
				void refetchServerStatusConfig();
			}
		});
		return () => release();
	});

	/** Pick the body for the viewer's locale. Hard-coded fallback
	 *  chain: preferred_locale → bare language subtag → `en` →
	 *  first available key. */
	function pickOpsBannerBody(banner: { body: Record<string, string> }): string {
		const preferred = session.me?.full.preferred_locale;
		const prefs = [preferred, preferred?.split('-')[0], 'en'].filter(
			(v): v is string => typeof v === 'string' && v.length > 0
		);
		for (const key of prefs) {
			const hit = banner.body[key];
			if (typeof hit === 'string' && hit.length > 0) return hit;
		}
		for (const [, text] of Object.entries(banner.body)) {
			if (text) return text;
		}
		return '';
	}

	// Any URL under /admin swaps the sidebar to admin mode. Uses
	// startsWith so a trailing slash / query params / hash don't
	// desync. Root `/admin` counts too (dashboard).
	const isAdminSection = $derived(appPath(page.url.pathname).startsWith('/admin'));
	const navGroups: NavGroup[] = $derived(
		isAdminSection ? [{ label: null, links: ADMIN_LINKS }] : NAV_GROUPS
	);

	function active(href: string): boolean {
		const path = appPath(page.url.pathname);
		return path === href || path.startsWith(`${href}/`);
	}

	// Sidebar-item active check. Non-admin links use `active()`
	// (matches href + any subpath). Admin links need a stricter
	// rule for `/admin` itself — a plain `startsWith('/admin/')`
	// would light up the Dashboard item on `/admin/drives` too.
	// So `/admin` matches ONLY the exact path; every other admin
	// item uses the same startsWith rule as before.
	function activeLink(href: string): boolean {
		if (href === '/admin') return appPath(page.url.pathname) === '/admin';
		return active(href);
	}

	/**
	 * Data-driven sidebar links (`NAV_GROUPS`, `ADMIN_LINKS`) hold
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

	// ── Drop onto a drive in the picker ──────────────────────────────
	// Moves (or with the OS copy modifier, copies) the dragged items into
	// that drive's ROOT folder. Only reachable when the picker renders,
	// i.e. when the account has more than one drive — with a single drive
	// there is nowhere else to drop.
	//
	// Shares `sidebarDropHref` with the nav-link targets so exactly one
	// thing in the sidebar is ever armed; drives key on their id, which
	// cannot collide with an href.
	let driveDropId = $state<string | null>(null);

	/**
	 * Folder currently being browsed, from `/files/<…>/<leaf>` — the last
	 * segment, since the route is `[...path]`. `null` anywhere else.
	 *
	 * Used to refuse a drop onto the drive whose root you are already
	 * looking at: the target folder would be the one the items are
	 * already in, so the server no-ops and the UI would report a move
	 * that did not happen. Dropping onto that same drive from a SUBFOLDER
	 * is a real move to its root, and stays allowed.
	 */
	const currentFolderId = $derived.by(() => {
		const m = /^\/files\/(.+)$/.exec(appPath(page.url.pathname));
		if (!m) return null;
		return (
			m[1]
				.split('/')
				.filter((s) => s.length > 0)
				.at(-1) ?? null
		);
	});

	/** Would dropping here move items into the folder they are already in? */
	function isCurrentFolderDrive(d: Drive): boolean {
		return currentFolderId !== null && d.root_folder_id === currentFolderId;
	}

	function driveOnDragOver(e: DragEvent, _d: Drive) {
		if (!e.dataTransfer?.types.includes(SIDEBAR_DRAG_TYPE)) return;
		// Not a valid target when it resolves to the folder on screen —
		// no `preventDefault`, so the browser shows a no-drop cursor and
		// the row never highlights. Better than accepting the drop and
		// reporting a move the server didn't make.
		if (isCurrentFolderDrive(_d)) return;
		e.preventDefault();
		// Deliberately NOT setting `dropEffect`: leaving it alone lets the
		// browser negotiate the cursor from the live OS modifier keys
		// (arrow-with-plus for copy, plain for move), which is what makes
		// the copy gesture discoverable. The nav-link targets above DO set
		// it, because favorites/trash are single-meaning drops.
		driveDropId = _d.id;
	}

	function driveOnDragLeave(d: Drive) {
		if (driveDropId === d.id) driveDropId = null;
	}

	async function driveOnDrop(e: DragEvent, d: Drive) {
		driveDropId = null;
		if (!e.dataTransfer?.types.includes(SIDEBAR_DRAG_TYPE)) return;
		// Belt to the dragover brace: a drop can still arrive here if the
		// URL changed mid-drag, and silently doing nothing beats claiming
		// a move that the server treated as a no-op.
		if (isCurrentFolderDrive(d)) return;
		e.preventDefault();
		const items = parseDragItems(e);
		if (items.length === 0) return;

		// Dropping a folder onto a drive whose root IS that folder would
		// ask the server to move it inside itself.
		const target = d.root_folder_id;
		const moving = items.filter((it) => it.id !== target);
		if (moving.length === 0) return;

		// Copy-vs-move read off the drop event, same rule as the folder-row
		// and breadcrumb drops on /files — see `$lib/utils/dragModifiers`.
		const copy = isCopyDrop(e);
		try {
			if (copy) {
				// Batch endpoints: one call per kind, both independent.
				await Promise.all([
					copyFiles(
						moving.filter((it) => it.kind === 'file').map((it) => it.id),
						target
					),
					copyFolders(
						moving.filter((it) => it.kind === 'folder').map((it) => it.id),
						target
					)
				]);
			} else {
				// Per-item, bounded — mirrors `/files`'s `moveInto`. There is
				// no batch move endpoint, and a cross-drive move can fail
				// per item (the D5 `forbid_cross_drive_move` drive policy
				// refuses, or the destination drive's quota check returns
				// 507), so each is attempted and `fanout` toasts failures
				// individually rather than abandoning the rest.
				await fanout(moving, 6, async (it) => {
					if (it.kind === 'file') await moveFile(it.id, target);
					else await moveFolder(it.id, target);
				});
			}
		} catch (err) {
			errorToast(err);
			return;
		}
		ui.notify(
			copy
				? t(
						'drive.copied_n',
						{ n: moving.length, drive: d.name },
						'Copied {{n}} item(s) to {{drive}}'
					)
				: t(
						'drive.moved_n',
						{ n: moving.length, drive: d.name },
						'Moved {{n}} item(s) to {{drive}}'
					),
			'success'
		);
		// No refresh event dispatched, matching the trash drop above: the
		// source listing updates itself off the message bus, since the
		// move/copy services publish and `/files` subscribes to its
		// folder topic (`useFolderTopic`). The favorites drop needs its
		// `oxicloud:favorite-changed` event only because a star is local
		// UI state no server event carries.
		//
		// Consequence worth knowing: with the message bus disabled, the
		// row lingers until a manual reload — exactly as it already does
		// for a sidebar trash drop.
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

		// Trash: destructive → gate on confirm. Adapt copy to the
		// deployment's trash state (see the files/[...path] page's
		// `deleteItem` helper for the server-side rationale).
		const trashOn = serverConfig.features.trash;
		const ok = await dialogs.confirm({
			title: t('files.batch_delete', 'Delete selected'),
			message:
				items.length === 1
					? trashOn
						? t('files.confirm_delete', { name: items[0].name }, 'Move "{{name}}" to trash?')
						: t(
								'files.confirm_delete_permanent',
								{ name: items[0].name },
								'Permanently delete "{{name}}"? This cannot be undone.'
							)
					: trashOn
						? t('files.confirm_batch_delete', { n: items.length }, 'Move {{n}} items to trash?')
						: t(
								'files.confirm_batch_delete_permanent',
								{ n: items.length },
								'Permanently delete {{n}} items? This cannot be undone.'
							),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		await fanout(items, 6, (it) => (it.kind === 'file' ? deleteFile(it.id) : deleteFolder(it.id)));
	}

	let sidebarOpen = $state(false);
	let sidebarCollapsed = $state(false);
	// Read after mount, not at init: SSR renders the expanded sidebar and a
	// localStorage-seeded `$state` would desync hydration.
	onMount(() => {
		try {
			sidebarCollapsed = localStorage.getItem('oxi-sidebar-collapsed') === '1';
		} catch {
			/* private mode / quota — stay expanded */
		}
	});

	function toggleSidebarCollapsed() {
		sidebarCollapsed = !sidebarCollapsed;
		try {
			localStorage.setItem('oxi-sidebar-collapsed', sidebarCollapsed ? '1' : '0');
		} catch {
			/* private mode / quota — the toggle still works this session */
		}
	}

	let sidebarEl = $state<HTMLElement | null>(null);
	let sidebarReturnFocus: HTMLElement | null = null;

	$effect(() => {
		if (typeof document === 'undefined') return;
		if (sidebarOpen) {
			sidebarReturnFocus = document.activeElement as HTMLElement | null;
			queueMicrotask(() =>
				sidebarEl?.querySelector<HTMLElement>('a[href], button:not([disabled])')?.focus()
			);
		} else {
			sidebarReturnFocus?.focus?.();
			sidebarReturnFocus = null;
		}
	});

	function trapSidebarFocus(e: KeyboardEvent) {
		if (e.key !== 'Tab' || !sidebarOpen || !sidebarEl) return;
		const root = sidebarEl;
		const focusables = [
			...root.querySelectorAll<HTMLElement>(
				'a[href], button:not([disabled]), [tabindex]:not([tabindex="-1"])'
			)
		];
		if (!focusables.length) return;
		const first = focusables[0];
		const last = focusables[focusables.length - 1];
		if (!root.contains(document.activeElement)) {
			e.preventDefault();
			first.focus();
			return;
		}
		if (!e.shiftKey && document.activeElement === last) {
			e.preventDefault();
			first.focus();
		}
	}

	let notifOpen = $state(false);
	let menuOpen = $state(false);
	let searchQuery = $state('');
	/** Mobile collapsible-search overlay state (toggles .top-bar--search-active). */
	let searchActive = $state(false);
	let aboutOpen = $state(false);
	let searchInputEl = $state<HTMLInputElement | null>(null);
	let shortcutLabel = $state('Ctrl K');
	onMount(() => {
		if (/Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent)) {
			shortcutLabel = '⌘K';
		}
	});
	let online = $state(true);
	onMount(() => {
		online = navigator.onLine;
		const update = () => (online = navigator.onLine);
		window.addEventListener('online', update);
		window.addEventListener('offline', update);
		return () => {
			window.removeEventListener('online', update);
			window.removeEventListener('offline', update);
		};
	});

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

	// Persistent notifications (Slice E) — server-backed rows,
	// survive reload, delivered via `user:{me}:notifications` bus
	// topic + refetched from `GET /api/notifications`. Fires the
	// initial hydrate + subscribes to the topic. Independent of the
	// transient toast bell above (`ui.notifications`) — that stays
	// as-is for upload-progress / one-shot messages; this stream
	// carries `share_granted` and friends.
	useNotifications();

	// Merged unread count for the bell badge — transient toasts plus
	// persistent unread rows. Same wire and same UX affordance so a
	// user sees one number and one bell for both classes.
	const totalUnread = $derived(ui.unread + persistentNotifications.unread);
	const totalUnreadBadge = $derived(totalUnread > 99 ? '99+' : String(totalUnread));

	function openMobileSearch() {
		searchActive = true;
		requestAnimationFrame(() => searchInputEl?.focus());
	}

	function closeMobileSearch() {
		searchActive = false;
		clearSearch();
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
	let suggestIndex = $state(-1);

	$effect(() => {
		void suggestions;
		void searchQuery;
		suggestIndex = -1;
	});

	function onSearchKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape' && suggestOpen) {
			e.preventDefault();
			suggestOpen = false;
			suggestIndex = -1;
			return;
		}
		if (!suggestOpen) return;
		const total = suggestions.length + 1;
		if (e.key === 'ArrowDown') {
			e.preventDefault();
			suggestIndex = (suggestIndex + 1) % total;
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			suggestIndex = (suggestIndex - 1 + total) % total;
		} else if (e.key === 'Enter' && suggestIndex >= 0) {
			e.preventDefault();
			if (suggestIndex < suggestions.length) pickSuggestion(suggestions[suggestIndex]);
			else goToResults();
		}
	}

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
		if (s.kind === 'folder') {
			goto(resolve(`/files/${s.item.id}`));
			return;
		}
		// Open the file in the VIEWER, not by navigating the browser to
		// its raw bytes. The old `window.open(fileInlineUrl(id))` was a
		// top-level navigation to `?inline=true` with no mime check at
		// all, so picking an uploaded .html from search rendered it as a
		// document on this origin — one click, attacker-controlled HTML,
		// the victim's session. The server now sandboxes that response
		// (see the user-content branch in
		// `interfaces::middleware::security_headers`), but the UI should
		// not be asking the browser to render user content as a document
		// in the first place: the viewer dispatches on type and keeps
		// text in a read-only pane.
		goto(resolve(`/files/${s.item.folder_id}?file=${s.item.id}`));
	}

	const storagePct = $derived(
		storagePercent(
			session.me?.full.storage_used_bytes ?? 0,
			session.me?.full.storage_quota_bytes ?? 0
		)
	);

	function closeMenus() {
		notifOpen = false;
		menuOpen = false;
	}
</script>

<svelte:window
	onclick={closeMenus}
	onkeydown={(e) => {
		trapSidebarFocus(e);
		// First Cmd/Ctrl+K loads the palette and mounts it open; once mounted,
		// the palette's own handler takes over toggling/closing.
		if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k' && !palette.component) {
			e.preventDefault();
			void palette.load();
			return;
		}
		if (e.key !== 'Escape') return;
		if (searchActive) closeMobileSearch();
		else if (sidebarOpen) sidebarOpen = false;
		else closeMenus();
	}}
/>

<a class="skip-link" href="#main-content">{t('nav.skip_to_content', 'Skip to content')}</a>

<div
	class="sidebar-overlay"
	class:active={sidebarOpen}
	onclick={() => (sidebarOpen = false)}
	role="presentation"
></div>

<aside
	class="sidebar"
	class:open={sidebarOpen}
	class:sidebar--collapsed={sidebarCollapsed}
	bind:this={sidebarEl}
>
	<div class="sidebar-header">
		<BrandMark href={resolve('/files')} testId="appshell-logo-link" />
		<hr class="sidebar-divider" />
		<button
			class="sidebar-collapse-btn"
			aria-label={sidebarCollapsed
				? t('nav.expand', 'Expand sidebar')
				: t('nav.collapse', 'Collapse sidebar')}
			aria-expanded={!sidebarCollapsed}
			title={sidebarCollapsed
				? t('nav.expand', 'Expand sidebar')
				: t('nav.collapse', 'Collapse sidebar')}
			data-testid="appshell-sidebar-collapse-btn"
			onclick={toggleSidebarCollapsed}
		>
			<Icon name={sidebarCollapsed ? 'chevron-right' : 'chevron-left'} />
		</button>
	</div>

	<hr class="sidebar-divider" />

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
				title={sidebarCollapsed ? t('nav.back_to_app', 'Back to OxiCloud') : undefined}
				onclick={() => (sidebarOpen = false)}
			>
				<Icon name="arrow-left" />
				<span>{t('nav.back_to_app', 'Back to OxiCloud')}</span>
			</a>
		{/if}
		<!--
		  Sidebar links come from data-driven arrays (NAV_GROUPS,
		  ADMIN_LINKS), so hrefs are runtime strings rather than
		  compile-time-known route keys. `navHref()` internally
		  routes them through `resolve()`, but ESLint's static
		  check can't see through the wrapper. Disabling the
		  rule here rather than sprinkling per-line directives.
		-->
		<!-- eslint-disable svelte/no-navigation-without-resolve -->
		{#each navGroups as group, i (group.links[0].href)}
			{#if i > 0}<hr class="sidebar-divider" />{/if}
			<div class="nav-group" class:nav-group--nested={group.nested}>
				{#if group.label}
					<div class="nav-group__label">{group.label}</div>
				{/if}
				{#each group.links as link (link.href)}
					{@const isDropTarget =
						!isAdminSection && (link.href === '/favorites' || link.href === '/trash')}
					<a
						class="nav-item"
						class:active={activeLink(link.href)}
						class:nav-item--drop-target={isDropTarget && sidebarDropHref === link.href}
						href={navHref(link.href)}
						data-section={link.section}
						data-testid={`appshell-nav-${link.section}-link`}
						title={sidebarCollapsed ? link.label : undefined}
						onclick={() => (sidebarOpen = false)}
						ondragover={isDropTarget ? (e) => sidebarOnDragOver(e, link.href) : undefined}
						ondragleave={isDropTarget ? () => sidebarOnDragLeave(link.href) : undefined}
						ondrop={isDropTarget ? (e) => void sidebarOnDrop(e, link.href) : undefined}
					>
						<Icon name={link.icon} />
						<span>{link.label}</span>
					</a>
					{#if !isAdminSection && link.href === '/files' && !session.isExternalUser}
						<DrivePicker
							onnavigate={() => (sidebarOpen = false)}
							dropTargetId={driveDropId}
							ondragoverDrive={driveOnDragOver}
							ondragleaveDrive={driveOnDragLeave}
							onDropIntoDrive={(e, d) => void driveOnDrop(e, d)}
						/>
					{/if}
				{/each}
			</div>
		{/each}
		<!-- eslint-enable svelte/no-navigation-without-resolve -->
	</nav>

	{#if page.url.pathname.startsWith('/admin') && serverConfig.loaded && serverConfig.version}
		<!-- Admin context: swap the user's personal-quota widget for
		     the OxiCloud version. Storage usage of the admin's own
		     account is the least useful signal while auditing the
		     whole system; build provenance is the most useful. Same
		     container class + spacing so the sidebar footer footprint
		     doesn't jump between routes. -->
		<div class="storage-container storage-container--version">
			<div class="storage-title">
				<Icon name="tag" /> <span>{t('sidebar.oxicloud_version', 'OxiCloud')}</span>
			</div>
			<div class="storage-version-value">v{serverConfig.version}</div>
		</div>
	{:else if session.user}
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
</aside>

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

		<!--
			Search surface is gated on `serverConfig.features.search` —
			when the operator sets `OXICLOUD_ENABLE_SEARCH=false` the
			backend returns 404 on `/api/search/*` (routes aren't mounted
			via `routes.rs:313`'s `if search_service.is_some()`), so the
			SPA must not render a search bar that would issue requests
			that can't succeed. The mobile toggle + back-arrow are
			icon-only and sized by their intrinsic width, so hiding them
			via `{#if}` doesn't shift the layout. The `.search-slot`
			stays always-rendered as the flex-grow spacer — hiding its
			CONTENT (not the slot itself) keeps notifications + the
			user menu anchored on the right instead of collapsing left.
		-->
		{#if serverConfig.features.search}
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
		{/if}

		<div class="search-slot">
			{#if serverConfig.features.search}
				<form class="search-container" onsubmit={onSearch}>
					<input
						type="text"
						bind:this={searchInputEl}
						bind:value={searchQuery}
						data-testid="appshell-search-input"
						oninput={onSearchInput}
						onkeydown={onSearchKeydown}
						aria-keyshortcuts="Control+K Meta+K"
						onfocus={() => (suggestOpen = suggestions.length > 0)}
						onblur={() => setTimeout(() => (suggestOpen = false), 150)}
						placeholder={t('actions.search', 'Search files, folders...')}
						autocomplete="off"
					/>
					{#if !searchQuery}
						<kbd class="search-kbd" aria-hidden="true">{shortcutLabel}</kbd>
					{/if}
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
							{#each suggestions as s, i (s.kind + s.item.id)}
								<li>
									<button
										class="suggest__item"
										class:suggest__item--active={i === suggestIndex}
										type="button"
										data-testid={`appshell-search-suggestion-${s.kind}-${s.item.id}-item`}
										onmousedown={() => pickSuggestion(s)}
									>
										<span class="suggest__icon">
											{#if s.kind === 'folder'}
												<Icon name="folder" />
											{:else}
												<Icon name={iconNameFor(s.item.name, s.item.mime_type)} />
											{/if}
										</span>
										<span class="suggest__name">{s.item.name}</span>
									</button>
								</li>
							{/each}
							<li>
								<button
									class="suggest__all"
									class:suggest__all--active={suggestIndex === suggestions.length}
									type="button"
									data-testid="appshell-search-see-all-btn"
									onmousedown={goToResults}
								>
									{t('search.see_all', 'See all results')}
								</button>
							</li>
						</ul>
					{:else if suggestBusy}
						<ul class="suggest">
							<li class="suggest__busy">{t('common.loading', 'Loading…')}</li>
						</ul>
					{/if}
				</form>
			{/if}
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
						if (notifOpen) {
							ui.markNotificationsRead();
							// Persistent rows stay unread until the user
							// explicitly clicks one — opening the panel
							// doesn't mark them read (unlike the transient
							// toast bell, which resets on view). Keeps the
							// bell's badge accurate to "still-relevant
							// server-side rows" without a bulk mark-read.
						}
					}}
				>
					<Icon name="bell" />
					{#if totalUnread > 0}<span class="notif-badge">{totalUnreadBadge}</span>{/if}
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
						{#if ui.notifications.length === 0 && persistentNotifications.items.length === 0}
							<div class="notif-empty">
								<Icon name="bell-slash" />
								<span>{t('notifications.empty', 'No notifications')}</span>
							</div>
						{:else}
							{#each ui.notifications as n (n.id)}
								<TransientNotificationRow {n} />
							{/each}
							{#if persistentNotifications.items.length > 0}
								{#if ui.notifications.length > 0}
									<div
										class="notif-section-divider"
										role="separator"
										aria-orientation="horizontal"
									></div>
								{/if}
								{#each persistentNotifications.items as row (row.id)}
									<NotificationRow {row} onactivate={() => (notifOpen = false)} />
								{/each}
							{/if}
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
					<SelfAvatar />
				</button>

				<div class="user-menu">
					<UserMenuContent
						onNavigate={() => (menuOpen = false)}
						onabout={() => {
							menuOpen = false;
							aboutOpen = true;
						}}
					/>
				</div>
			</div>
		</div>
	</div>

	<main class="content-area" id="main-content" tabindex="-1">
		{#if !online}
			<Banner
				severity="warning"
				title={t('offline.title', 'You are offline')}
				body={t('offline.body', 'Uploads and changes will fail until the connection comes back.')}
				role="status"
				testid="offline-banner"
				ariaLabel={t('offline.title', 'You are offline')}
			/>
		{/if}
		<!-- Server-wide maintenance banner. Fed by the
		     `x-server-status` header read on every API response by
		     `apiFetch` — no polling. Shows for every logged-in user
		     while a storage migration is running so they know why
		     writes are being refused, with live progress if
		     available. Disappears automatically on the next API
		     round-trip after the server clears the flag.

		     Uses the shared `Banner` primitive — same component the
		     drive-frozen banner and the operator-authored announcements
		     render through, so every banner reads as the same visual
		     family. -->
		{#if serverStatus().readonly}
			<Banner
				severity="warning"
				icon="lock"
				title={t('server_status.readonly_title', 'Server maintenance in progress')}
				body={serverStatus().migration
					? undefined
					: t(
							'server_status.readonly_body',
							'Uploads, renames, deletes, and shares are refused temporarily. Reads and downloads work as normal.'
						)}
				progress={serverStatus().migration}
				role="alert"
				testid="server-status-banner"
				ariaLabel={t('server_status.readonly_banner_aria', 'Server maintenance in progress')}
			/>
		{:else if serverStatus().rotation}
			<!-- K4 storage-key-rotation: rotation is running but
			     `readonly` is false — writes continue as normal.
			     Distinct variant styling (info, not warning) so the
			     copy reads "background maintenance" rather than
			     "server frozen". -->
			<Banner
				severity="info"
				icon="key"
				title={t('server_status.rotating_title', 'Storage key rotation in progress')}
				body={serverStatus().rotation
					? undefined
					: t(
							'server_status.rotating_body',
							'A background key rotation is normalising storage. All operations continue normally.'
						)}
				progress={serverStatus().rotation}
				role="status"
				testid="server-status-rotating-banner"
				ariaLabel={t('server_status.rotating_banner_aria', 'Storage key rotation in progress')}
			/>
		{/if}
		<!-- Operator-authored banners — rendered BELOW the system
		     banner so write-refusal state (which is the one thing
		     users must see) stays top-of-page. Each entry picks the
		     viewer's locale body and renders via the sanitizing
		     markdown util.
		     Deliberately NOT dismissable: the server is the single
		     source of truth for "what's visible", operator deletes a
		     row when it's no longer relevant, and the message-bus
		     push propagates the removal within ~1s. See the memo
		     above on `renderOpsBannerMarkdown` import. -->
		{#each serverStatus().banners ?? [] as banner (banner.id)}
			<Banner
				severity={banner.severity}
				html={renderOpsBannerMarkdown(pickOpsBannerBody(banner))}
				role={banner.severity === 'warning' ? 'alert' : 'status'}
				testid="ops-banner"
			/>
		{/each}
		<!-- Admin-only advisories. Gated on `isAdminSection && isAdmin`
		     — the flag is populated by the admin dashboard loader
		     (`loadDashboard()` in /admin/[[tab]]/+page.svelte mirrors
		     `storage_cache_recommended` into `adminAdvisories`), and
		     this double guard means a non-admin whose session somehow
		     carried a stale value doesn't see the banner, and an
		     admin who navigated away from /admin stops seeing it
		     too. Not a security surface — the advisory is purely a
		     performance hint — but keeping it scoped to admin UX
		     avoids confusing regular users with a config-env
		     reference they can't act on. -->
		{#if isAdminSection && isAdmin && adminAdvisories().cacheRecommended}
			<Banner
				severity="warning"
				title={t('admin.storage_cache_recommended_title', 'Enable the local blob cache')}
				body={t(
					'admin.storage_cache_recommended_body',
					'It is highly recommended to enable the local cache when using a remote backend (cache to SSD / NVMe preferred).'
				)}
				progressLine={t(
					'admin.storage_cache_recommended_howto',
					'Set OXICLOUD_STORAGE_CACHE_ENABLED=true to enable it.'
				)}
				role="status"
				testid="admin-cache-advisory"
				ariaLabel={t('admin.storage_cache_recommended_title', 'Enable the local blob cache')}
			/>
		{/if}
		{#if isAdminSection && isAdmin && adminAdvisories().notificationSinkMissing}
			<Banner
				severity="warning"
				title={t('admin.notification_sink_missing_title', 'Configure a notification sink')}
				body={t(
					'admin.notification_sink_missing_body',
					'Job-failure findings have nowhere to go — nobody is notified when something breaks. Configure at least one sink.'
				)}
				progressLine={t(
					'admin.notification_sink_missing_howto',
					'Set BOTH OXICLOUD_JOBS_NOTIFY_EMAIL_TO (comma-separated addresses) AND OXICLOUD_SMTP_HOST, OR set OXICLOUD_WEBHOOK_URL — then restart.'
				)}
				role="status"
				testid="admin-notification-sink-advisory"
				ariaLabel={t('admin.notification_sink_missing_title', 'Configure a notification sink')}
			/>
		{/if}
		{@render children()}
	</main>
</div>

<MobileNav />

<AboutDialog bind:open={aboutOpen} />

{#if palette.component}
	{@const CommandPalette = palette.component}
	<CommandPalette autoOpen />
{/if}

<style>
	/* Body becomes the sidebar+main flex row only while the shell is mounted. */
	:global(body) {
		display: flex;
	}

	#main-content:focus-visible {
		outline: none;
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

	.search-kbd {
		position: absolute;
		right: 50px;
		display: none;
		padding: 1px var(--space-1-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm);
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
		font-family: var(--font-sans);
		font-size: var(--text-2xs);
		line-height: 1.4;
		pointer-events: none;
	}

	@media (width >= 769px) {
		.search-kbd {
			display: block;
		}
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

	.suggest__item--active,
	.suggest__all--active {
		background: var(--color-accent-bg-sm);
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

	/* Divider between transient toasts and persistent (server-backed)
	   rows. Slice E adds a section under the toast list; the divider
	   is only rendered when both sections have content. */
	.notif-section-divider {
		border-top: 1px solid var(--color-border);
		margin: 0.5rem 0;
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

	@media (width <= 768px) {
		/* The bottom bar owns both destinations on phones. */
		.notif-wrapper,
		.user-menu-wrapper {
			display: none;
		}
	}
</style>
