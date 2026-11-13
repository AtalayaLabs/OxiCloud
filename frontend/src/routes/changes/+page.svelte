<script lang="ts">
	// "What changed lately, and who changed it" — `docs/plan/recently-modified.md`.
	//
	// A sibling of `/recent`, not a tab on it: `/recent` is the viewer's own
	// access log, this is shared provenance across everyone who can reach the
	// file. The default axis is PEOPLE, because the question the page answers
	// is "what did my colleagues touch", and a swimlane per person answers it
	// at a glance where a flat chronological list does not.
	//
	// Phase 4: rides the generated SDK directly, like `/recent` — no
	// hand-written endpoint wrapper to drift from the spec.
	import { onMount } from 'svelte';
	import { listRecentChanges } from '$lib/api/generated/openapi/sdk.gen';
	import type {
		FileChangeItemDto,
		RecentChangesOrderBy
	} from '$lib/api/generated/openapi/types.gen';
	import ResourceList, {
		type ContextAction,
		type GroupByDef
	} from '$lib/components/ResourceList.svelte';
	import AvatarOverlayChip from '$lib/components/AvatarOverlayChip.svelte';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import type { FileItem, FolderItem } from '$lib/api/types';
	import UserAvatar from '$lib/components/UserAvatar.svelte';
	// Shared with every other listing — see `lib/utils/listActions.ts` for why
	// these stopped being per-route copies.
	import {
		downloadContextAction,
		openParentContextAction,
		prepareParentAccess,
		toggleFavorite
	} from '$lib/utils/listActions';
	// `resolveUser`, not `resolveOwnerName`: the lane header needs the photo
	// and the email as well as the name, and this is the resolver
	// `<UserAvatar>` itself calls. Its cache is keyed by id and holds the
	// PROMISE, so the avatar and this page share one round trip per person
	// instead of each hitting `/api/users/{id}`.
	import { resolveUser, type ResolvedUser } from '$lib/api/endpoints/users';
	import { dateBucket, typeLabelOf } from '$lib/utils/resourceBuckets';
	import { formatDate } from '$lib/utils/display';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import { t } from '$lib/i18n/index.svelte';

	/**
	 * One row of the feed: the file, plus THIS row's lane.
	 *
	 * The lane and the date live on the row rather than in `ResourceList`'s
	 * `contextMap`, and that is the whole reason this type exists. The context
	 * map is keyed by `item.id`, and one file legitimately appears in two
	 * lanes — Bob changed it Monday, Carol on Tuesday — so a map keyed by file
	 * id holds one of the two and silently mislabels the other. Per-row fields
	 * cannot collide.
	 */
	type ChangeRow = FileItem & {
		changedAt: string;
		changedBy: string;
		laneTotal: number | null;
	};

	/** `bucketOf` / snippets are typed against the component's union; every
	 *  item this page passes is a `ChangeRow`, so this narrows without a
	 *  per-callsite `'changedBy' in item` dance. */
	function row(item: FileItem | FolderItem): ChangeRow {
		return item as ChangeRow;
	}

	let raw = $state<FileChangeItemDto[]>([]);
	let cursor = $state<string | undefined>(undefined);
	let loading = $state(false);
	let error = $state<string | null>(null);
	/** People, per the plan's default. Matches `groupBys[0]`. */
	let groupBy = $state('modifier');
	// No `reversed` state, and `showSortDirection={false}` below: a change
	// feed is newest-first by nature, so `GET /api/files/recent-changes` has
	// no `reverse` parameter — reversing lane order would mean nothing, and
	// "oldest first inside the last 30 days" is not a question anyone asks.
	// The toggle used to render anyway (it keys off `groupBys`) and flipped a
	// variable `load()` could not send.
	/** Server default is 30 and its ceiling is 90 — a larger value is a 400
	 *  naming the bound, so the selector offers exactly what the API accepts. */
	let days = $state(30);
	/** modifier id → profile, for the lane header's avatar, name and email.
	 *  A record rather than a `Map` so nothing reactive is mutated inside a
	 *  derivation; reassigned per resolution, and a page has a handful of
	 *  lanes, not thousands. */
	let people = $state<Record<string, ResolvedUser>>({});

	/** Resolve the people a page introduced. Already-known ids are skipped,
	 *  and `resolveUser` de-duplicates whatever slips through. */
	async function resolvePeople(ids: string[]) {
		const wanted: Record<string, true> = {};
		for (const id of ids) if (id && !people[id]) wanted[id] = true;
		await Promise.all(
			Object.keys(wanted).map(async (id) => {
				const u = await resolveUser(id);
				if (u) people = { ...people, [id]: u };
			})
		);
	}

	const rows = $derived<ChangeRow[]>(
		raw.map((it) => ({
			...(it.resource as FileItem),
			changedAt: it.changed_at,
			changedBy: it.modifier_id,
			laneTotal: it.lane_total ?? null
		}))
	);

	/**
	 * What the list renders.
	 *
	 * On the people axis every row is kept: a file in two lanes is the point,
	 * and each lane is its own `#each`, so the repeated file id cannot collide.
	 *
	 * On the date axis there are no lanes — two people who both changed one
	 * file today produce two rows with the same id in ONE bucket, which is a
	 * duplicate `#each` key and a hard render error. So that axis shows each
	 * file once, at its most recent change; the server already orders touches
	 * newest-first, so the first occurrence is the one to keep. The per-row
	 * badge names who made it, which is the detail the lane header would have
	 * carried.
	 */
	// A plain record, not a `Set`: these are scratch lookups built inside a
	// `$derived`, and a `SvelteSet`/`SvelteMap` there would be reactive state
	// mutated during a derivation — the `state_unsafe_mutation` footgun. The
	// lint rule that asks for the reactive variants is about state that
	// outlives a computation; neither of these does.
	const items = $derived.by<ChangeRow[]>(() => {
		if (groupBy === 'modifier') return rows;
		const seen: Record<string, true> = {};
		const out: ChangeRow[] = [];
		for (const r of rows) {
			if (seen[r.id]) continue;
			seen[r.id] = true;
			out.push(r);
		}
		return out;
	});

	/** `lane_total` for the lane header, which the server counts AFTER the
	 *  access filter — so "Bob · 14 files" never hints at a file the viewer
	 *  cannot open. Arrives on the people axis only. */
	const laneTotals = $derived.by<Record<string, number>>(() => {
		const m: Record<string, number> = {};
		for (const r of rows) if (r.laneTotal != null) m[r.changedBy] = r.laneTotal;
		return m;
	});

	// Typed to the generated `RecentChangesOrderBy`, so a retired axis is an
	// `npm run check` failure rather than a runtime 400.
	const groupBys: GroupByDef<RecentChangesOrderBy>[] = [
		{
			key: 'modifier',
			// One key for one concept: the same string labels this axis and
			// the list-view column, and it lives with the other column
			// labels (`files.col_modified`, `col_created_by`, …).
			label: t('files.col_modified_by', 'Modified by'),
			orderBy: 'modifier',
			icon: 'pen',
			bucketOf: (item) => row(item).changedBy,
			// Recomputed on every `sync`, so a name that resolves after the
			// first paint replaces the id in the header rather than sticking.
			labelOf: (id) => people[id]?.name ?? id
		},
		{
			key: 'changedAt',
			label: t('groupby.changedAt', 'Changed date'),
			orderBy: 'changed_at',
			bucketOf: (item) => dateBucket(row(item).changedAt)
		},
		// Buckets client-side on `changed_at` order, where `/favorites` and
		// `/recent` have a server-side `type` axis of their own. The server
		// enum here is two values wide on purpose (§8), and a third would cost
		// a keyset shape and a cursor that must be refused on the wrong axis —
		// so this groups what the page has rather than asking the server to
		// sort by it. Consequence to know: a type's files arrive spread across
		// pages by date, so its bucket FILLS as you scroll instead of being
		// complete on page one. Identical to how `changedAt` already behaves
		// here, and the section builder merges an appended page into the open
		// bucket, so no bucket is ever drawn twice.
		//
		// Bucket ORDER is first-appearance, which on `changed_at` order means
		// types sorted by their most recent change — not alphabetical and not
		// `category_order`. That is a consequence of grouping client-side, and
		// the honest reading of this axis is "types, most recently touched
		// first". True type ordering needs the server axis.
		{
			key: 'type',
			label: t('groupby.type', 'Type'),
			orderBy: 'changed_at',
			// `?? null`, not a `'Unknown'` string literal: the bucket key is
			// the `ResourceCategory` enum value the server sent, and the
			// absence of one is the section builder's own null sentinel. The
			// label for it comes from `typeLabelOf`, which narrows against
			// `CATEGORIES` at runtime and falls back to the LOCALISED unknown
			// label — so an unrecognised category cannot render as a raw
			// English key, which is the i18n bug the typed union closed.
			// (`/recent` still carries the older literal form here.)
			bucketOf: (item) => item.category ?? null,
			labelOf: (k) => typeLabelOf(k)
		}
	];

	function orderByForGroup(): RecentChangesOrderBy {
		return groupBys.find((g) => g.key === groupBy)?.orderBy ?? 'changed_at';
	}

	async function load(reset = false, orderBy: RecentChangesOrderBy = orderByForGroup()) {
		loading = true;
		error = null;
		try {
			const page = await listRecentChanges({
				throwOnError: true,
				query: {
					cursor: reset ? null : (cursor ?? null),
					order_by: orderBy,
					days,
					limit: 50
				}
			});
			const fresh = page.items as FileChangeItemDto[];
			raw = reset ? fresh : [...raw, ...fresh];
			cursor = page.next_cursor ?? undefined;
			// One resolve per person per page, cached across pages — the DTO
			// carries `modifier_id` rather than an embedded user precisely so
			// this stays a handful of lookups instead of an N+1.
			void resolvePeople(fresh.map((i) => i.modifier_id));
		} catch (e) {
			console.error('changes: load error', e);
			error = t('errors_loadFailed', 'Failed to load items');
		} finally {
			loading = false;
		}
	}

	function setDays(next: number) {
		if (next === days) return;
		days = next;
		cursor = undefined;
		// The window decides which rows exist, so a cursor minted under the
		// old one is refused by the server (400) rather than silently
		// continuing across it. Always a fresh first page.
		void load(true);
	}

	let viewerOpen = $state(false);
	let viewerFile = $state<FileItem | null>(null);
	const viewerFiles = $derived(items as FileItem[]);
	const fileViewer = lazyComponent(() => import('$lib/components/FileViewer.svelte'));
	$effect(() => {
		if (viewerOpen) void fileViewer.load();
	});

	function open(item: FileItem | FolderItem) {
		viewerFile = row(item);
		viewerOpen = true;
	}

	/**
	 * Two entries only, deliberately.
	 *
	 * This is a provenance view, not a file manager: rename, move, share and
	 * delete act on the file and belong where the file lives. Offering them
	 * here would also be ambiguous on the people axis, where the same file is
	 * listed in two lanes — "delete" on a row the user sees twice is a
	 * question with no good answer. "Open parent folder" is the escape hatch
	 * to the real home, and download is the one read-only action that makes
	 * sense on a row you just spotted.
	 */
	const contextActions: ContextAction[] = [openParentContextAction(), downloadContextAction()];

	onMount(() => {
		void load(true);
	});
</script>

<svelte:head><title>{t('nav.changes', 'Changes')} · OxiCloud</title></svelte:head>

<!--
	Deliberately NOT `selectable` for v1. Selection is an id-keyed set, so
	ticking a file in Bob's lane would tick the same file in Carol's — and a
	batch action would then act once on something that looks doubly selected.
	The semantics are genuinely ambiguous (it is one file either way), so the
	honest v1 is a view you click through. Making it selectable means keying
	selection by `lane:file`, which is a `useSelection` change, not a page one.
-->
<ResourceList
	title={t('nav.changes', 'Changes')}
	titleInToolbar
	{items}
	{loading}
	{error}
	emptyIcon="pen"
	emptyText={t('changes.empty_state', 'No recent changes')}
	emptyHint={t('changes.empty_hint', 'Changes to files you can reach will appear here')}
	hasMore={!!cursor}
	onloadmore={() => load(false)}
	onopen={open}
	onfavorite={toggleFavorite}
	{contextActions}
	menuPrepare={prepareParentAccess}
	showOwner
	ownerLabel={t('files.col_modified_by', 'Modified by')}
	showPath
	dateLabel={t('changes.col_changed', 'Changed')}
	{groupBys}
	bind:groupBy
	showSortDirection={false}
	onreload={(orderBy) => {
		cursor = undefined;
		load(true, orderBy);
	}}
>
	{#snippet actions()}
		<!-- The window, as a plain select: three values, and the server
		     rejects anything outside them rather than clamping, so the
		     control offers exactly the accepted set. -->
		<label class="changes-window">
			<span class="changes-window__label">{t('changes.window', 'Time range')}</span>
			<!--
				`aria-label` as well as the visible `<span>`, because that span
				is `display: none` under 600px — which takes it out of the
				accessibility tree, not just out of the layout, leaving an
				unnamed combobox on exactly the viewport where the label was
				dropped for space. Same string, so no new translation.
			-->
			<select
				class="changes-window__select"
				aria-label={t('changes.window', 'Time range')}
				data-testid="changes-days-select"
				value={String(days)}
				onchange={(e) => setDays(Number(e.currentTarget.value))}
			>
				<option value="7">{t('changes.window_7', 'Last 7 days')}</option>
				<option value="30">{t('changes.window_30', 'Last 30 days')}</option>
				<option value="90">{t('changes.window_90', 'Last 90 days')}</option>
			</select>
		</label>
	{/snippet}
	{#snippet bucketLabel(key, label)}
		{@const total = laneTotals[key]}
		<!--
			ONE snippet, TWO axes — so the identity chrome is gated on the
			axis, not assumed. On the people axis the bucket key IS a user id;
			on the date axis it is a bucket name like "Today", and rendering
			`<UserAvatar userId="Today">` there would fetch `/api/users/Today`
			and then draw a coloured "T" circle as though a person called
			Today existed. The avatar and the email tooltip belong to the
			people axis alone.

			`<UserAvatar>` rather than a bespoke circle — its own docstring
			names the swimlane header as a target, and it already handles
			photo-or-coloured-initials and the lazy cached resolve. Its
			`title=""` suppresses ITS tooltip so the only one that fires is
			the email on the wrapper; with two nested `title`s the inner
			element wins and the email never shows.

			The email is the disambiguator: two colleagues can share a display
			name, and on a provenance view "who exactly" is the question.
		-->
		{@const isPerson = groupBy === 'modifier'}
		<span
			class="changes-lane"
			title={(isPerson && people[key]?.email) || undefined}
			data-testid={`changes-lane-${key}`}
		>
			{#if isPerson}
				<UserAvatar userId={key} fallbackLabel={label} size={20} title="" />
			{/if}
			<span>{label}</span>
			{#if total != null}
				<span class="changes-lane__count">
					· {t('changes.lane_files', { n: total }, '{{n}} file(s)')}
				</span>
			{/if}
		</span>
	{/snippet}
	{#snippet ownerCell(item)}
		<!--
			"Modified by", on EVERY axis including people.
			`ResourceList`'s default owner cell reads `ctx.ownerId` from the
			id-keyed context map, which cannot hold two values for one file —
			and on the people axis a file legitimately sits in two lanes with
			two different modifiers. So the person comes off the ROW, exactly
			as the date does, and the column no longer has to disappear on the
			page's default axis to stay truthful.
		-->
		{@const who = row(item).changedBy}
		<UserVignette userId={who} fallbackLabel={people[who]?.name ?? who} />
	{/snippet}
	{#snippet cardOverlay(item)}
		<!--
			Grid view: the same chip `/shared-with-me` anchors to the preview
			tile for "who shared this", now a shared component rather than a
			`:global` selector copied per page. List view has the column
			above; a card has no columns, so this is where the person goes.
		-->
		{@const who = row(item).changedBy}
		<AvatarOverlayChip
			userId={who}
			fallbackLabel={people[who]?.name ?? who}
			title={people[who]?.email || undefined}
		/>
	{/snippet}
	{#snippet dateCell(item)}
		<!-- This row's own touch, not the file's latest change by anyone: in
		     Bob's lane the date is when BOB changed it. Read off the row for
		     the id-collision reason on `ChangeRow`. -->
		{formatDate(row(item).changedAt)}
	{/snippet}
</ResourceList>

{#if fileViewer.component}
	{@const FileViewer = fileViewer.component}
	<FileViewer
		bind:open={viewerOpen}
		file={viewerFile}
		files={viewerFiles}
		onnavigate={(f) => (viewerFile = f)}
	/>
{/if}

<style>
	.changes-window {
		display: inline-flex;
		align-items: center;
		gap: 0.5rem;
	}

	/* Mobile-first: the three options describe themselves, so the label is
	   dead weight on a narrow toolbar and only earns its place once there is
	   room. The query expands, it does not restrict. */
	.changes-window__label {
		color: var(--color-text-secondary);
		display: none;
		font-size: 0.85rem;
	}

	@media (width >= 600px) {
		.changes-window__label {
			display: inline;
		}
	}

	/* Mirrors `LanguageSelect` — the one styled select in the app — rather
	   than inventing a second look for the same control. */
	.changes-window__select {
		background: var(--color-bg-input);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		color: var(--color-text);
		font-size: 0.85rem;
		padding: 0.35rem 0.5rem;
	}

	.changes-lane {
		align-items: center;
		display: inline-flex;
		gap: 0.4rem;
	}

	.changes-lane__count {
		color: var(--color-text-secondary);
		font-weight: 400;
	}
</style>
