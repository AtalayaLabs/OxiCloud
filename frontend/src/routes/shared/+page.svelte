<script lang="ts">
	import EmptyState from '$lib/components/EmptyState.svelte';
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { onMount } from 'svelte';
	import {
		fetchMyShares,
		notifyGrantRecipient,
		revokeGrant,
		updateGrantRole,
		type NotifyOutcome,
		type OutgoingGrantItem,
		type OutgoingResourceGrant,
		type ShareRole
	} from '$lib/api/endpoints/grants';
	import { displayRole } from '$lib/utils/grantFormat';
	import { expiryToIso } from '$lib/utils/dateInput';
	import { copyShareLink, deleteShare, getShareById, updateShare } from '$lib/api/endpoints/shares';
	import { ensureResolvers, resolveLabel } from '$lib/api/endpoints/recipients';
	import { fileInlineUrl } from '$lib/api/endpoints/files';
	import type { FileItem } from '$lib/api/types';
	import type { GrantResourceType } from '$lib/api/endpoints/grants';
	import Icon from '$lib/icons/Icon.svelte';
	import ListToolbar from '$lib/components/ListToolbar.svelte';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import type { OutgoingGrantOrderBy } from '$lib/api/generated/openapi/types.gen';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { formatDate, iconNameFor } from '$lib/utils/display';
	import { SharedLanesBuilder, type LaneGrouping } from '$lib/utils/sharedLanes';

	type GroupBy = 'items' | 'sharedWith';

	// §7 part 3g: `orderBy` is the generated OUTGOING union — `subject` is
	// valid only on this endpoint, and the type now says so.
	const GROUP_BYS: { key: GroupBy; label: string; orderBy: OutgoingGrantOrderBy }[] = [
		{ key: 'items', label: t('groupby.byFiles', 'By files'), orderBy: 'type' },
		{ key: 'sharedWith', label: t('groupby.sharedWith', 'Shared with'), orderBy: 'subject' }
	];

	const ROLES: { v: ShareRole; l: string; icon: string }[] = [
		{ v: 'owner', l: t('share.role.canManage', 'Can manage'), icon: 'crown' },
		{ v: 'editor', l: t('share.role.canEdit', 'Can edit'), icon: 'pencil-alt' },
		{ v: 'viewer', l: t('share.role.canView', 'Can view'), icon: 'eye' }
	];
	function roleMeta(r: string) {
		return ROLES.find((x) => x.v === displayRole(r)) ?? ROLES[2];
	}

	let raw = $state<OutgoingGrantItem[]>([]);
	let cursor = $state<string | undefined>(undefined);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let groupBy = $state<GroupBy>('items');
	let reversed = $state(false);

	// ── Kind filter ─────────────────────────────────────────────────────────
	// Client-side filter over `raw`. The backend endpoint
	// `GET /api/grants/outgoing/resources` currently emits `file`, `folder`,
	// and `drive` only. Calendar / contact / playlist grants exist as
	// backend resource kinds (`ResourceKind::Calendar` etc.) but aren't
	// aggregated by `list_my_shares` — a separate backend PR will extend
	// the endpoint, at which point another kind entry is added here.
	//
	// Filtering happens after pagination fetch, not inside the request,
	// so unchecking a kind is instant and doesn't cost a reload. The
	// pagination cursor is unaffected — Load more still fetches all kinds
	// and the filter re-applies to the growing list.
	const KIND_OPTIONS: { key: GrantResourceType; label: string; icon: string }[] = [
		{ key: 'file', label: t('myshares.filter.files', 'Files'), icon: 'file' },
		{ key: 'folder', label: t('myshares.filter.folders', 'Folders'), icon: 'folder' },
		{ key: 'drive', label: t('myshares.filter.drives', 'Drives'), icon: 'hdd' }
	];

	// Default: files + folders visible, drives hidden. Drives share
	// less frequently (whole-tree grants) and clutter the list when
	// what the user wants is a file/folder audit.
	const DEFAULT_KINDS: Record<GrantResourceType, boolean> = {
		file: true,
		folder: true,
		drive: false
	};

	// Persist filter selection across sessions on THIS device. Not
	// stored server-side because the kind filter is a device-local
	// view choice — a user auditing shared drives on their admin
	// machine likely has a different filter than what they use to
	// track files on their laptop. Contrast `preferences.hideDotfiles`
	// which is per-user + cross-device (JSONB on the user row).
	//
	// localStorage key uses the `oxi-*` prefix so it participates in
	// the switch-account wipe in `localStoragePrefs.ts` — a fresh
	// login starts with defaults, not the previous user's choice.
	const STORAGE_KEY = 'oxi-shared-kinds';

	function loadSelectedKinds(): Record<GrantResourceType, boolean> {
		if (typeof localStorage === 'undefined') return { ...DEFAULT_KINDS };
		try {
			const raw = localStorage.getItem(STORAGE_KEY);
			if (!raw) return { ...DEFAULT_KINDS };
			const parsed = JSON.parse(raw) as Partial<Record<GrantResourceType, boolean>>;
			// Merge over DEFAULT_KINDS so a stored record from a build
			// before some kind existed still yields a full record.
			// Rejects any junk (non-boolean values) by ignoring them.
			const merged: Record<GrantResourceType, boolean> = { ...DEFAULT_KINDS };
			for (const opt of KIND_OPTIONS) {
				const v = parsed[opt.key];
				if (typeof v === 'boolean') merged[opt.key] = v;
			}
			return merged;
		} catch {
			return { ...DEFAULT_KINDS };
		}
	}

	function saveSelectedKinds(kinds: Record<GrantResourceType, boolean>): void {
		if (typeof localStorage === 'undefined') return;
		try {
			localStorage.setItem(STORAGE_KEY, JSON.stringify(kinds));
		} catch {
			/* quota / private mode — silently skip, filter still works this session */
		}
	}

	let selectedKinds = $state<Record<GrantResourceType, boolean>>(loadSelectedKinds());
	let filterOpen = $state(false);

	function toggleKind(k: GrantResourceType) {
		selectedKinds[k] = !selectedKinds[k];
		saveSelectedKinds(selectedKinds);
	}
	function resetKinds() {
		selectedKinds = { ...DEFAULT_KINDS };
		saveSelectedKinds(selectedKinds);
	}

	const activeKindCount = $derived(KIND_OPTIONS.filter((k) => selectedKinds[k.key]).length);
	const filteredRaw = $derived(raw.filter((item) => selectedKinds[item.resource_type]));

	// Edit-sharing dialog
	let dialogOpen = $state(false);
	let dialogItem = $state<{ id: string; name: string; kind: GrantResourceType } | null>(null);

	// ShareDialog is heavy and only opens on demand — keep it out of this route's
	// initial chunk and load it the first time the dialog is opened.
	const shareDialog = lazyComponent(() => import('$lib/components/ShareDialog.svelte'));
	$effect(() => {
		if (dialogOpen) void shareDialog.load();
	});

	// Open kebab menu, keyed by grant id.
	let menuFor = $state<string | null>(null);

	function expiryTier(iso: string | null | undefined): 'never' | 'active' | 'soon' | 'expired' {
		if (!iso) return 'never';
		const ms = new Date(iso).getTime() - Date.now();
		if (ms < 0) return 'expired';
		if (ms <= 30 * 86_400_000) return 'soon';
		return 'active';
	}
	function expiryLabel(iso: string | null | undefined): string {
		if (!iso) return t('share.noExpiry', 'No expiry');
		// Same semantics as before (`''` for unparseable dates), now via the
		// shared util so it reuses the cached Intl.DateTimeFormat.
		return formatDate(iso);
	}
	function isoToDate(iso: string | null | undefined): string {
		return iso ? String(iso).slice(0, 10) : '';
	}

	// ── Swimlane assembly ───────────────────────────────────────────────────
	interface Lane {
		key: string;
		header:
			| { kind: 'resource'; item: OutgoingGrantItem }
			| { kind: 'user'; id: string }
			| { kind: 'group'; id: string }
			| { kind: 'linkPublic' }
			| { kind: 'linkPassword' };
		rows: { grant: OutgoingResourceGrant; item: OutgoingGrantItem }[];
	}

	type LaneRow = Lane['rows'][number];

	// The active grouping as a stable-identity descriptor: `groupKey` changes
	// only when the user switches group-by, so an infinite-scroll page (or a
	// grant edit that reassigns `raw`) takes the builder's O(N) incremental path
	// instead of re-bucketing the whole accumulated list. `emit` reproduces the
	// old derive exactly — `open` is the old unconditional `ensure` (a by-files
	// lane exists even with zero grants); `push` is `ensure(...).rows.push`.
	const laneGrouping = $derived.by(
		(): LaneGrouping<OutgoingGrantItem, Lane['header'], LaneRow> =>
			groupBy === 'items'
				? {
						groupKey: 'items',
						emit: (item, sink) => {
							const key = `resource:${item.resource.id}`;
							const header: Lane['header'] = { kind: 'resource', item };
							sink.open(key, header);
							for (const grant of item.grants) sink.push(key, header, { grant, item });
						}
					}
				: {
						groupKey: 'sharedWith',
						emit: (item, sink) => {
							for (const grant of item.grants) {
								let key: string;
								let header: Lane['header'];
								if (grant.subject_type === 'user') {
									key = `user:${grant.subject_id}`;
									header = { kind: 'user', id: grant.subject_id };
								} else if (grant.subject_type === 'group') {
									key = `group:${grant.subject_id}`;
									header = { kind: 'group', id: grant.subject_id };
								} else if (grant.has_password) {
									key = 'links:password';
									header = { kind: 'linkPassword' };
								} else {
									key = 'links:public';
									header = { kind: 'linkPublic' };
								}
								sink.push(key, header, { grant, item });
							}
						}
					}
	);

	// Persistent across reactive ticks: re-buckets only the freshly-appended page
	// and hands back the same rows-array reference for untouched lanes, falling
	// back to a full rebuild (deep-equal to the pure `buildLanes` reference) on a
	// group-by switch, grant edit or kind-filter toggle. Mirrors ResourceList's
	// `sectionsBuilder` (benches/ROUND16.md §F1).
	const lanesBuilder = new SharedLanesBuilder<OutgoingGrantItem, Lane['header'], LaneRow>();
	const lanes = $derived.by(() => lanesBuilder.sync(filteredRaw, laneGrouping));

	function laneTitle(header: Lane['header']): string {
		switch (header.kind) {
			case 'user':
				return resolveLabel('user', header.id);
			case 'group':
				return resolveLabel('group', header.id);
			case 'linkPublic':
				return t('myshares.publicLinks', 'Public links');
			case 'linkPassword':
				return t('myshares.passwordLinks', 'Password-protected links');
			case 'resource':
				return header.item.resource.name;
		}
	}

	// ── Data loading ────────────────────────────────────────────────────────
	async function load(reset = false) {
		loading = true;
		error = null;
		try {
			await ensureResolvers();
			const order = GROUP_BYS.find((g) => g.key === groupBy)?.orderBy;
			const page = await fetchMyShares({
				cursor: reset ? undefined : cursor,
				orderBy: order,
				reverse: reversed
			});
			raw = reset ? page.items : [...raw, ...page.items];
			cursor = page.next_cursor;
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loading = false;
		}
	}

	function reload() {
		cursor = undefined;
		raw = [];
		void load(true);
	}

	function setGroupBy(key: GroupBy) {
		if (groupBy === key) return;
		groupBy = key;
		reload();
	}
	function toggleDirection() {
		reversed = !reversed;
		reload();
	}

	function openResource(item: OutgoingGrantItem) {
		// Drives don't have a Files-page deep-link the same way folders do
		// (their id is the drive UUID, not a folder UUID); route to the
		// per-drive settings page so the user lands somewhere meaningful.
		if (item.resource_type === 'drive') {
			goto(resolve(`/config/drive/${item.resource.id}`));
			return;
		}
		if (item.resource_type === 'folder') goto(resolve(`/files/${item.resource.id}`));
		else window.open(fileInlineUrl(item.resource.id), '_blank', 'noopener');
	}

	function editSharing(item: OutgoingGrantItem) {
		dialogItem = { id: item.resource.id, name: item.resource.name, kind: item.resource_type };
		dialogOpen = true;
	}

	function toggleMenu(grantId: string) {
		menuFor = menuFor === grantId ? null : grantId;
	}
	function closeMenu() {
		menuFor = null;
	}

	function summarize(outcomes: NotifyOutcome[]) {
		if (!outcomes || outcomes.length === 0) {
			ui.notify(t('myshares.notifySent', 'Notification sent.'), 'success');
			return;
		}
		const sent = outcomes.filter((o) => o.kind === 'sent').length;
		const coalesced = outcomes.filter((o) => o.kind === 'coalesced').length;
		const rate = outcomes.filter((o) => o.kind === 'rate_limited').length;
		const skipped = outcomes.filter((o) => o.kind === 'not_applicable').length;
		const lines: string[] = [];
		if (sent > 0) lines.push(t('share.notify.sent', { n: sent }, '{{n}} notified by email.'));
		if (coalesced > 0)
			lines.push(t('share.notify.coalesced', { n: coalesced }, '{{n}} already notified recently.'));
		if (rate > 0)
			lines.push(
				t('share.notify.rateLimited', { n: rate }, '{{n}} hit the rate limit — try later.')
			);
		if (skipped > 0)
			lines.push(
				t('share.notify.skipped', { n: skipped }, '{{n}} skipped (no email / opted out).')
			);
		ui.notify(
			lines.join(' ') || t('myshares.notifySent', 'Notification sent.'),
			rate || skipped ? 'info' : 'success'
		);
	}

	// ── Per-grant actions ───────────────────────────────────────────────────
	async function changeRole(g: OutgoingResourceGrant, item: OutgoingGrantItem, role: ShareRole) {
		closeMenu();
		if (g.role === role) return;
		try {
			await updateGrantRole(
				{ type: g.subject_type, id: g.subject_id },
				{ type: item.resource_type, id: item.resource.id },
				role,
				expiryToIso(isoToDate(g.expires_at) || null)
			);
			g.role = role;
			raw = [...raw];
		} catch (e) {
			errorToast(e);
		}
	}

	async function changeExpiry(g: OutgoingResourceGrant, item: OutgoingGrantItem, date: string) {
		try {
			const iso = expiryToIso(date || null);
			await updateGrantRole(
				{ type: g.subject_type, id: g.subject_id },
				{ type: item.resource_type, id: item.resource.id },
				g.role,
				iso
			);
			g.expires_at = iso;
			raw = [...raw];
		} catch (e) {
			errorToast(e);
		}
	}

	async function notify(g: OutgoingResourceGrant) {
		closeMenu();
		try {
			summarize((await notifyGrantRecipient(g.grant_id)).outcomes);
		} catch (e) {
			errorToast(e);
		}
	}

	async function removeAccess(g: OutgoingResourceGrant) {
		closeMenu();
		try {
			await revokeGrant(g.grant_id);
			dropGrant(g.grant_id);
		} catch (e) {
			errorToast(e);
		}
	}

	async function copyLink(g: OutgoingResourceGrant) {
		closeMenu();
		try {
			const share = await getShareById(g.subject_id);
			if (await copyShareLink(share.url)) ui.notify(t('share.copied', 'Link copied'), 'success');
			else ui.notify(t('share.copy_failed', 'Could not copy link'), 'error');
		} catch (e) {
			errorToast(e);
		}
	}

	async function changeLinkExpiry(g: OutgoingResourceGrant, date: string) {
		try {
			await updateShare(g.subject_id, { expiresAt: date || null });
			g.expires_at = expiryToIso(date || null);
			raw = [...raw];
		} catch (e) {
			errorToast(e);
		}
	}

	async function editLinkPassword(g: OutgoingResourceGrant) {
		closeMenu();
		const pw = window.prompt(
			g.has_password
				? t('share.passwordPrompt_clear', 'New password (blank to remove):')
				: t('share.passwordPrompt', 'Set a password:')
		);
		if (pw === null) return;
		try {
			await updateShare(g.subject_id, { password: pw || null });
			g.has_password = !!pw;
			raw = [...raw];
			ui.notify(
				pw
					? t('share.password_set', 'Password updated')
					: t('share.password_cleared', 'Password removed'),
				'success'
			);
		} catch (e) {
			errorToast(e);
		}
	}

	async function deleteLink(g: OutgoingResourceGrant) {
		closeMenu();
		try {
			await deleteShare(g.subject_id);
			dropGrant(g.grant_id);
		} catch (e) {
			errorToast(e);
		}
	}

	/** Remove a grant locally, pruning now-empty resources. */
	function dropGrant(grantId: string) {
		raw = raw
			.map((item) => ({ ...item, grants: item.grants.filter((g) => g.grant_id !== grantId) }))
			.filter((item) => item.grants.length > 0);
	}

	function linkLabel(g: OutgoingResourceGrant): string {
		return `${t('share.link', 'Link')} · …${g.subject_id.slice(-4)} · ${g.subject_display}`;
	}

	function resourceIcon(item: OutgoingGrantItem): string {
		// Drives use the `hdd` glyph (shared with DrivePicker / breadcrumb)
		// so a shared drive reads as a distinct kind at a glance — folder
		// and drive both grant access to a tree, but the scope is very
		// different.
		if (item.resource_type === 'drive') return 'hdd';
		if (item.resource_type === 'folder') return 'folder';
		const file = item.resource as FileItem;
		return iconNameFor(file.name, file.mime_type);
	}

	const isEmpty = $derived(!loading && raw.length === 0 && !error);
	// `raw` has data but the kind filter hides all of it — distinct empty
	// state so we can offer a "reset filter" affordance instead of the
	// generic "you haven't shared anything" hint.
	const noMatchesForFilter = $derived(
		!loading && !error && raw.length > 0 && filteredRaw.length === 0
	);

	onMount(() => load(true));
</script>

<svelte:head><title>{t('nav.shared', 'Shared')} · OxiCloud</title></svelte:head>
<svelte:window
	onclick={() => {
		if (menuFor) closeMenu();
		if (filterOpen) filterOpen = false;
	}}
/>

<div class="page-sticky-header">
	<h1 class="page-title">{t('nav.shared', 'Shared')}</h1>
	<ListToolbar
		groups={GROUP_BYS}
		{groupBy}
		{reversed}
		ongroup={(key) => setGroupBy(key as GroupBy)}
		ondirection={toggleDirection}
		showViewToggle={false}
	>
		{#snippet beforeGroupBy()}
			<div class="group-by-selector ms-filter" data-testid="shared-filter-menu">
				<button
					class="toggle-btn group-by-btn active"
					title={t('myshares.filter.title', 'Filter by kind')}
					aria-haspopup="true"
					aria-expanded={filterOpen}
					data-testid="shared-filter-btn"
					onclick={(e) => {
						e.stopPropagation();
						filterOpen = !filterOpen;
					}}
				>
					<Icon name="filter" />
					<span class="group-by-label">
						{t('myshares.filter.button', 'Kinds')}
						{#if activeKindCount < KIND_OPTIONS.length}
							<span class="ms-filter__badge">{activeKindCount}</span>
						{/if}
					</span>
				</button>
				{#if filterOpen}
					<div
						class="group-by-menu"
						role="menu"
						tabindex="-1"
						onclick={(e) => e.stopPropagation()}
						onkeydown={(e) => e.key === 'Escape' && (filterOpen = false)}
					>
						{#each KIND_OPTIONS as k (k.key)}
							<label class="group-by-option ms-filter__row" class:active={selectedKinds[k.key]}>
								<input
									type="checkbox"
									data-testid={`shared-filter-${k.key}`}
									checked={selectedKinds[k.key]}
									onchange={() => toggleKind(k.key)}
								/>
								<Icon name={k.icon} />
								{k.label}
							</label>
						{/each}
					</div>
				{/if}
			</div>
		{/snippet}
	</ListToolbar>
</div>

{#if error}
	<EmptyState icon="exclamation-circle" title={error} error />
{:else if isEmpty}
	<EmptyState
		icon="share-alt"
		title={t('myshares.emptyStateTitle', "You haven't shared anything yet")}
		hint={t('myshares.emptyStateDesc', 'Items you share with others will appear here')}
	/>
{:else if noMatchesForFilter}
	<EmptyState
		icon="filter"
		title={t('myshares.filter.emptyTitle', 'No shares match the current filter')}
		hint={t(
			'myshares.filter.emptyHint',
			'Adjust the kind filter or reset it to the default (Files + Folders).'
		)}
	>
		<button class="btn btn-secondary" data-testid="shared-filter-reset" onclick={resetKinds}>
			<Icon name="rotate-left" />
			{t('myshares.filter.reset', 'Reset filter')}
		</button>
	</EmptyState>
{:else}
	<div class="ms-lanes">
		{#each lanes as lane (lane.key)}
			<section class="ms-lane">
				<header class="ms-lane__header">
					{#if lane.header.kind === 'resource'}
						{@const laneItem = lane.header.item}
						<button
							class="ms-lane__resource"
							data-testid={`shared-lane-open-${laneItem.resource.id}`}
							onclick={() => openResource(laneItem)}
						>
							<Icon name={resourceIcon(laneItem)} />
							<span class="ms-lane__name">{laneItem.resource.name}</span>
						</button>
						<button
							class="btn btn-secondary ms-lane__edit"
							data-testid={`shared-edit-sharing-${laneItem.resource.id}`}
							onclick={() => editSharing(laneItem)}
						>
							<Icon name="pencil-alt" />
							{t('myshares.editSharing', 'Edit sharing')}
						</button>
					{:else if lane.header.kind === 'user'}
						<span class="ms-lane__subject">
							<UserVignette
								userId={lane.header.id}
								fallbackLabel={resolveLabel('user', lane.header.id)}
							/>
						</span>
					{:else}
						<span class="ms-lane__subject">
							<Icon
								name={lane.header.kind === 'group'
									? 'user-group'
									: lane.header.kind === 'linkPassword'
										? 'lock'
										: 'link'}
							/>
							<span class="ms-lane__name">{laneTitle(lane.header)}</span>
						</span>
					{/if}
				</header>

				<ul class="ms-rows">
					{#each lane.rows as { grant, item } (grant.grant_id)}
						{@const tier = expiryTier(grant.expires_at)}
						<li class="ms-row" class:ms-row--expired={tier === 'expired'}>
							<!-- Identity -->
							<span class="ms-row__identity">
								{#if (grant.subject_type === 'user' || grant.subject_type === 'group') && groupBy === 'sharedWith'}
									<button
										class="ms-link-btn"
										data-testid={`shared-row-open-${grant.grant_id}`}
										onclick={() => openResource(item)}
									>
										<Icon name={resourceIcon(item)} />
										<span class="ms-row__name">{item.resource.name}</span>
									</button>
								{:else if grant.subject_type === 'user'}
									<UserVignette
										userId={grant.subject_id}
										fallbackLabel={resolveLabel('user', grant.subject_id)}
									/>
								{:else if grant.subject_type === 'group'}
									<Icon name="user-group" />
									<span class="ms-row__name">{resolveLabel('group', grant.subject_id)}</span>
								{:else}
									<button
										class="ms-chip ms-chip--link"
										class:ms-chip--locked={grant.has_password}
										data-testid={`shared-copy-link-${grant.grant_id}`}
										onclick={() => copyLink(grant)}
										title={t('share.copyLink', 'Copy link')}
									>
										<Icon name={grant.has_password ? 'lock' : 'link'} />
										<span class="ms-row__name">{linkLabel(grant)}</span>
									</button>
									{#if groupBy === 'sharedWith'}
										<span class="ms-arrow">→</span>
										<button
											class="ms-link-btn"
											data-testid={`shared-link-open-${grant.grant_id}`}
											onclick={() => openResource(item)}
										>
											<Icon name={resourceIcon(item)} />
											<span>{item.resource.name}</span>
										</button>
									{/if}
								{/if}
							</span>

							<!-- Role pill (not shown for token subjects) -->
							{#if grant.subject_type !== 'token'}
								<span class="ms-role ms-role--{roleMeta(grant.role).v}">
									<Icon name={roleMeta(grant.role).icon} />
									{roleMeta(grant.role).l}
								</span>
							{/if}

							<!-- Expiry chip -->
							<span class="ms-expiry ms-expiry--{tier}" title={expiryLabel(grant.expires_at)}>
								<Icon name={grant.expires_at ? 'clock' : 'infinity'} />
								{expiryLabel(grant.expires_at)}
							</span>

							<!-- Kebab -->
							<div class="ms-kebab">
								<button
									class="btn-icon"
									aria-label={t('myshares.manageAccess', 'Manage access')}
									aria-haspopup="menu"
									aria-expanded={menuFor === grant.grant_id}
									data-testid={`shared-kebab-${grant.grant_id}`}
									onclick={(e) => {
										e.stopPropagation();
										toggleMenu(grant.grant_id);
									}}><Icon name="ellipsis-v" /></button
								>
								{#if menuFor === grant.grant_id}
									<div
										class="ms-menu"
										role="menu"
										tabindex="-1"
										data-testid={`shared-menu-${grant.grant_id}`}
										onclick={(e) => e.stopPropagation()}
										onkeydown={(e) => e.key === 'Escape' && closeMenu()}
									>
										{#if grant.subject_type === 'user' || grant.subject_type === 'group'}
											<button
												class="ms-menu__item"
												role="menuitem"
												data-testid={`shared-notify-${grant.grant_id}`}
												onclick={() => notify(grant)}
											>
												<Icon name="paper-plane" />
												{grant.subject_type === 'group'
													? t('myshares.notifyGroupMembers', 'Notify group members')
													: grant.is_external
														? t('myshares.resendInvitation', 'Resend invitation email')
														: t('myshares.notifyByEmail', 'Notify by email')}
											</button>
											<div class="ms-menu__sep"></div>
											{#each ROLES as r (r.v)}
												<button
													class="ms-menu__item"
													class:ms-menu__item--current={grant.role === r.v}
													role="menuitem"
													data-testid={`shared-role-${r.v}-${grant.grant_id}`}
													onclick={() => changeRole(grant, item, r.v)}
												>
													<Icon name={grant.role === r.v ? 'check' : r.icon} />
													{r.l}
												</button>
											{/each}
											<div class="ms-menu__sep"></div>
											<div class="ms-menu__field">
												<span class="ms-menu__label">{t('share.expiry', 'Expiry')}</span>
												<input
													type="date"
													class="ms-menu__date"
													data-testid={`shared-expiry-${grant.grant_id}-input`}
													value={isoToDate(grant.expires_at)}
													onchange={(e) =>
														changeExpiry(grant, item, (e.currentTarget as HTMLInputElement).value)}
												/>
											</div>
											<div class="ms-menu__sep"></div>
											<button
												class="ms-menu__item ms-menu__item--danger"
												role="menuitem"
												data-testid={`shared-remove-access-${grant.grant_id}`}
												onclick={() => removeAccess(grant)}
											>
												<Icon name="user-xmark" />
												{t('myshares.removeAccess', 'Remove access')}
											</button>
										{:else}
											<button
												class="ms-menu__item"
												role="menuitem"
												data-testid={`shared-menu-copy-link-${grant.grant_id}`}
												onclick={() => copyLink(grant)}
											>
												<Icon name="copy" />
												{t('myshares.copyLink', 'Copy link')}
											</button>
											<div class="ms-menu__sep"></div>
											<div class="ms-menu__field">
												<span class="ms-menu__label">{t('share.expiry', 'Expiry')}</span>
												<input
													type="date"
													class="ms-menu__date"
													data-testid={`shared-link-expiry-${grant.grant_id}-input`}
													value={isoToDate(grant.expires_at)}
													onchange={(e) =>
														changeLinkExpiry(grant, (e.currentTarget as HTMLInputElement).value)}
												/>
											</div>
											<button
												class="ms-menu__item"
												role="menuitem"
												data-testid={`shared-edit-password-${grant.grant_id}`}
												onclick={() => editLinkPassword(grant)}
											>
												<Icon name={grant.has_password ? 'lock' : 'lock-open'} />
												{grant.has_password
													? t('share.changePassword', 'Change password')
													: t('share.addPassword', 'Add password')}
											</button>
											<div class="ms-menu__sep"></div>
											<button
												class="ms-menu__item ms-menu__item--danger"
												role="menuitem"
												data-testid={`shared-delete-link-${grant.grant_id}`}
												onclick={() => deleteLink(grant)}
											>
												<Icon name="trash" />
												{t('myshares.deleteLink', 'Delete link')}
											</button>
										{/if}
									</div>
								{/if}
							</div>
						</li>
					{/each}
				</ul>
			</section>
		{/each}

		{#if cursor}
			<button
				class="btn btn-secondary ms-more"
				data-testid="shared-load-more-btn"
				onclick={() => load(false)}
				disabled={loading}
			>
				{loading ? t('common.loading', 'Loading…') : t('common.load_more', 'Load more')}
			</button>
		{/if}
	</div>
{/if}

{#if shareDialog.component}
	{@const ShareDialog = shareDialog.component}
	<!-- Drives don't support shareable URLs — hide the Public-link tab
	     when the dialog is opened for a drive resource. File/folder
	     resources keep the default tab set. -->
	<ShareDialog bind:open={dialogOpen} item={dialogItem} allowLinks={dialogItem?.kind !== 'drive'} />
{/if}

<style>
	.ms-lanes {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		padding-top: var(--space-3);
	}

	.ms-lane {
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		overflow: visible;
	}

	.ms-lane__header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-bottom: 1px solid var(--color-border-faint, var(--color-border));
		background: var(--color-bg-muted);
	}

	.ms-lane__resource,
	.ms-lane__subject {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		font-weight: var(--weight-semibold, 600);
		color: var(--color-text);
		background: none;
		border: none;
		cursor: pointer;
		min-width: 0;
	}

	.ms-lane__subject {
		cursor: default;
	}

	.ms-lane__name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.ms-lane__edit {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		flex: none;
	}

	.ms-rows {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
	}

	.ms-row {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border-top: 1px solid var(--color-border-faint, var(--color-border));
	}

	.ms-row:first-child {
		border-top: none;
	}

	.ms-row--expired {
		opacity: 0.6;
	}

	.ms-row__identity {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex: 1;
		min-width: 0;
	}

	.ms-row__name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.ms-link-btn,
	.ms-chip {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		background: none;
		border: none;
		color: var(--color-text);
		cursor: pointer;
		min-width: 0;
		padding: 0;
	}

	.ms-chip--link {
		padding: var(--space-1) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-pill, 999px);
	}

	.ms-chip--locked {
		color: var(--color-accent);
	}

	.ms-arrow {
		color: var(--color-text-muted);
	}

	/* Role pill */
	.ms-role {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius-pill, 999px);
		font-size: var(--text-sm);
		background: var(--color-bg-muted);
		color: var(--color-text-secondary);
		flex: none;
	}

	.ms-role--admin {
		background: var(--color-warning-bg, var(--color-bg-muted));
		color: var(--color-warning-text-amber, var(--color-text-secondary));
	}

	.ms-role--editor {
		background: var(--color-accent-bg, var(--color-bg-muted));
		color: var(--color-accent-text, var(--color-text-secondary));
	}

	/* Expiry chip tiers */
	.ms-expiry {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius-pill, 999px);
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		background: var(--color-bg-muted);
		flex: none;
	}

	.ms-expiry--soon {
		color: var(--color-warning-text-amber, var(--color-text-secondary));
		background: var(--color-warning-bg, var(--color-bg-muted));
	}

	.ms-expiry--expired {
		color: var(--color-danger-text);
		background: var(--color-danger-bg, var(--color-bg-muted));
	}

	/* Kebab + menu */
	.ms-kebab {
		position: relative;
		flex: none;
	}

	.btn-icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--color-text-secondary);
		cursor: pointer;
	}

	.btn-icon:hover {
		background: var(--color-bg-hover);
	}

	.ms-menu {
		position: absolute;
		right: 0;
		top: 100%;
		z-index: 50;
		min-width: 14rem;
		margin-top: var(--space-1);
		padding: var(--space-1);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
	}

	.ms-menu__item {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--color-text);
		text-align: left;
		cursor: pointer;
	}

	.ms-menu__item:hover {
		background: var(--color-bg-hover);
	}

	.ms-menu__item--current {
		font-weight: var(--weight-semibold, 600);
	}

	.ms-menu__item--danger {
		color: var(--color-danger-text);
	}

	.ms-menu__sep {
		height: 1px;
		margin: var(--space-1) 0;
		background: var(--color-border);
	}

	.ms-menu__field {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
	}

	.ms-menu__label {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.ms-menu__date {
		padding: var(--space-1) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: var(--text-sm);
	}

	.ms-more {
		margin: var(--space-3) auto 0;
	}

	/* Kind filter — nested inside ListToolbar's `.view-toggle`, styled
	   as a sibling of the group-by dropdown. The `.group-by-selector`,
	   `.group-by-btn`, `.group-by-menu`, `.group-by-option` classes
	   are inherited from the global `ported/buttons.css` — see the
	   `beforeGroupBy` snippet in the template. Only the local tweaks
	   below (checkbox layout + active-count badge) stay page-scoped. */

	.ms-filter__row {
		cursor: pointer;
	}

	.ms-filter__row input[type='checkbox'] {
		margin: 0;
		cursor: pointer;
	}

	/* Count of active kinds when the filter is narrower than "all
	   kinds" — small pill inside the button's label so the button
	   still reads as a single group-by-style control. */
	.ms-filter__badge {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 1.25rem;
		height: 1.1rem;
		margin-left: var(--space-1);
		padding: 0 var(--space-1);
		border-radius: var(--radius-pill, 999px);
		background: var(--color-accent);
		color: var(--color-text-light);
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold, 600);
	}
</style>
