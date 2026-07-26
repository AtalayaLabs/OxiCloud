<script lang="ts">
	import { resolve } from '$app/paths';
	import { getFolderAncestors } from '$lib/api/endpoints/folders';
	import type { AccessSource, FolderAncestor, FolderAncestorsResponse } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';

	/**
	 * Shared breadcrumb component consuming
	 * `GET /api/folders/{id}/ancestors`. Renders the root icon
	 * (`access_source.kind` — drive / share / link) + a clickable
	 * chain of caller-visible ancestors down to the leaf.
	 *
	 * The endpoint's walk stops at the caller's share/drive-membership
	 * boundary, so this component never shows a folder the caller can't
	 * Read. If `folderId` is null (e.g. `/search` in "Everywhere" scope,
	 * or /files at the root listing) the component renders nothing.
	 *
	 * Optional `onDrop` prop enables `/files`-style drop-target behavior
	 * on each crumb (move dragged items into the target folder). Absent
	 * everywhere else. Uses the `application/x-oxi-item` MIME the row-drag
	 * emits — pass a matching handler.
	 */
	interface Props {
		/** Leaf folder id; null renders the component as empty. */
		folderId: string | null | undefined;
		/**
		 * Optional drop handler — enables per-crumb drop targets when
		 * provided. Called with the target folder id + the raw drop
		 * event; the caller performs the move.
		 */
		onDrop?: (targetFolderId: string, e: DragEvent) => void;
		/** MIME type of the row-drag payload — defaults to the shipped one. */
		dragMime?: string;
	}
	let { folderId, onDrop, dragMime = 'application/x-oxi-item' }: Props = $props();

	// Fetch chain when folderId changes. `$state` + `$effect` primer
	// avoids blocking the initial render — the breadcrumb slot appears
	// empty until the first response, then fills in.
	let chain = $state<FolderAncestorsResponse | null>(null);
	let dropTargetId = $state<string | null>(null);

	$effect(() => {
		const id = folderId;
		if (!id) {
			chain = null;
			return;
		}
		void getFolderAncestors(id)
			.then((c) => {
				// Guard against out-of-order responses if `folderId`
				// changed while awaiting.
				if (folderId === id) chain = c;
			})
			.catch(() => {
				// Silent failure — the breadcrumb collapses to empty. The
				// consuming page still shows its main content (folder
				// listing / search results); a missing crumb strip is a
				// degraded-but-usable state, not a fatal one.
				if (folderId === id) chain = null;
			});
	});

	/**
	 * Ancestors to render as crumbs, with the drive-root deduplicated
	 * when access is via drive-membership. Rationale (Ed 2026-07-26):
	 * for drive-kind access, the topmost accessible ancestor IS the
	 * drive's root folder, and the drive's display name equals the
	 * root folder's name (`docs/plan/drive.md §3` — a drive has no
	 * `name` column, its name lives on its root folder). So the pre-
	 * fix breadcrumb rendered `Personal > Personal > child > …` for
	 * personal drives and `my family > my family > child > …` for
	 * shared. The root chip already labels the drive; dropping the
	 * duplicate first crumb collapses to the natural `[home] Personal
	 * > child > …` shape.
	 *
	 * For `direct_share` / `token` access, the topmost ancestor is a
	 * shared folder (not a drive root), so no dedup — every ancestor
	 * survives.
	 */
	const visibleCrumbs = $derived<FolderAncestor[]>(
		chain
			? chain.access_source.kind === 'drive' && chain.ancestors.length > 0
				? chain.ancestors.slice(1)
				: chain.ancestors
			: []
	);

	// ── Root-icon derivation ────────────────────────────────────────────
	// One icon per `access_source.kind`. Personal drives use the home
	// glyph (they're the caller's own storage — signalling "home base");
	// shared drives use `users` (multi-member). Ed's 2026-07-26 UX call
	// bumped from the pre-fix `hard-drive` because personal drives
	// deserve the same "you're on your own turf" visual affordance the
	// legacy /files rootIcon used.
	function rootIcon(src: AccessSource): string {
		if (src.kind === 'drive') {
			return src.drive?.kind === 'shared' ? 'users' : 'home';
		}
		if (src.kind === 'direct_share') return 'share-alt';
		if (src.kind === 'token') return 'link';
		return 'home';
	}

	function rootTooltip(src: AccessSource): string {
		if (src.kind === 'drive' && src.drive) {
			return src.drive.kind === 'shared'
				? t('breadcrumb.root.shared_drive', { name: src.drive.name }, 'Shared drive: {{name}}')
				: t('breadcrumb.root.personal_drive', { name: src.drive.name }, 'Personal drive: {{name}}');
		}
		if (src.kind === 'direct_share') {
			return t('breadcrumb.root.direct_share', 'Shared with you');
		}
		if (src.kind === 'token') {
			return t('breadcrumb.root.token', 'Via shared link');
		}
		return t('breadcrumb.home', 'Home');
	}

	/**
	 * Href for the root chip. For drive-kind access, links to the
	 * drive's root folder (the ancestor we deduped above) so the user
	 * can jump home from any depth. For share/token access the "root"
	 * is an abstract boundary with no navigable page — stays null and
	 * the template renders the chip as a non-clickable `<span>`.
	 * Hoisted here (not `{@const}` inside `<nav>`) because Svelte 5
	 * only allows `{@const}` as an immediate child of specific block
	 * tags — plain HTML elements don't qualify.
	 */
	// Root chip href. Two "clickable root" cases:
	//   • drive-kind → the drive root folder (the ancestor we dedup
	//     out of the chain above), so users can jump home from any
	//     depth without leaving the /files context.
	//   • direct_share → `/shared-with-me`, so users can back out to
	//     the full listing of what's been shared with them (Ed's
	//     2026-07-26 UX ask: "when I clic on it that goes back to
	//     /shared-with-me").
	// Token access stays non-clickable — there's no equivalent user-
	// facing surface for a public-link session.
	//
	// Store the UNRESOLVED path here; `resolve()` runs in the template
	// so the `svelte/no-navigation-without-resolve` lint sees the
	// resolve call at the href site (the rule can't follow a state
	// variable back to its assignment).
	// Narrow union so SvelteKit's route-checked `resolve()` accepts it.
	// The two paths are the only ones this component ever emits.
	type RootHref = '/shared-with-me' | `/files/${string}`;

	// True when the caller is AT the drive root (or share boundary) —
	// no descendant crumbs to render. The root chip IS the current
	// location and gets the `breadcrumb-current` bold treatment.
	const isRootTheLeaf = $derived(chain !== null && visibleCrumbs.length === 0);

	// Root href stays populated even when root-is-leaf — clicking a leaf
	// crumb is a real navigation (from `/search` it jumps INTO the folder;
	// from `/files` at drive root it's a self-navigation no-op). Ed's
	// 2026-07-26 UX call: "all elements clickable, only the leaf bold."
	const rootHrefPath = $derived<RootHref | null>(
		chain === null
			? null
			: chain.access_source.kind === 'drive' && chain.ancestors.length > 0
				? `/files/${chain.ancestors[0].id}`
				: chain.access_source.kind === 'direct_share'
					? '/shared-with-me'
					: null
	);

	// Drop target for the root chip. Only meaningful when the root
	// resolves to a real folder (drive root). `/shared-with-me` is a
	// virtual listing — nothing to drop INTO — so direct_share and
	// token variants stay drop-inert even when the chip is clickable.
	const rootDropTarget = $derived<string | null>(
		chain && chain.access_source.kind === 'drive' ? (chain.ancestors[0]?.id ?? null) : null
	);
</script>

{#if chain && (visibleCrumbs.length > 0 || chain.access_source.kind === 'drive')}
	<nav class="breadcrumb" aria-label={t('breadcrumb.aria', 'Breadcrumb')}>
		<!--
			Root chip: `<a>` when drive-kind access (jumps to the drive
			root — the ancestor we dedup out of the chain above), `<span>`
			for share/token (abstract boundary, no navigable target).
			Icon + tooltip both derive from `access_source.kind`; the drive
			arm additionally paints the drive name next to the icon so the
			user sees which drive they're browsing at a glance.
		-->
		{#if rootHrefPath}
			<a
				href={resolve(rootHrefPath)}
				class="breadcrumb-item breadcrumb-home breadcrumb-link"
				class:breadcrumb-current={isRootTheLeaf}
				class:drop-target={onDrop != null &&
					rootDropTarget != null &&
					dropTargetId === rootDropTarget}
				title={rootTooltip(chain.access_source)}
				data-testid="folder-breadcrumb-root-link"
				data-access-kind={chain.access_source.kind}
				ondragover={onDrop && rootDropTarget
					? (e) => e.dataTransfer?.types.includes(dragMime) && e.preventDefault()
					: undefined}
				ondragenter={onDrop && rootDropTarget
					? (e) => {
							if (e.dataTransfer?.types.includes(dragMime)) dropTargetId = rootDropTarget;
						}
					: undefined}
				ondragleave={onDrop && rootDropTarget
					? () => {
							if (dropTargetId === rootDropTarget) dropTargetId = null;
						}
					: undefined}
				ondrop={onDrop && rootDropTarget
					? (e) => {
							dropTargetId = null;
							onDrop(rootDropTarget, e);
						}
					: undefined}
			>
				<Icon name={rootIcon(chain.access_source)} />
				{#if chain.access_source.kind === 'drive' && chain.access_source.drive}
					<span class="breadcrumb-root-name">{chain.access_source.drive.name}</span>
				{/if}
			</a>
		{:else}
			<!--
				Non-link root chip. Three cases land here:
				  1. `access_source.kind === 'token'` — no navigable target.
				  2. Drive-kind AND caller is AT the drive root (no
				     descendant crumbs). Gets `breadcrumb-current` so the
				     styling matches a deep-folder leaf (bold, no
				     underline) — Ed's 2026-07-26 UX ask: keep the leaf
				     look consistent regardless of depth.
				  3. Drive-kind with no ancestors at all (degenerate).
				Drop target only wires when there's a real folder id AND
				the caller opted in with an `onDrop` handler.
			-->
			<!-- svelte-ignore a11y_no_static_element_interactions -->
			<span
				class="breadcrumb-item breadcrumb-home"
				class:breadcrumb-current={isRootTheLeaf}
				class:drop-target={onDrop != null &&
					rootDropTarget != null &&
					dropTargetId === rootDropTarget}
				title={rootTooltip(chain.access_source)}
				data-testid="folder-breadcrumb-root-icon"
				data-access-kind={chain.access_source.kind}
				ondragover={onDrop && rootDropTarget
					? (e) => e.dataTransfer?.types.includes(dragMime) && e.preventDefault()
					: undefined}
				ondragenter={onDrop && rootDropTarget
					? (e) => {
							if (e.dataTransfer?.types.includes(dragMime)) dropTargetId = rootDropTarget;
						}
					: undefined}
				ondragleave={onDrop && rootDropTarget
					? () => {
							if (dropTargetId === rootDropTarget) dropTargetId = null;
						}
					: undefined}
				ondrop={onDrop && rootDropTarget
					? (e) => {
							dropTargetId = null;
							onDrop(rootDropTarget, e);
						}
					: undefined}
			>
				<Icon name={rootIcon(chain.access_source)} />
				{#if chain.access_source.kind === 'drive' && chain.access_source.drive}
					<span class="breadcrumb-root-name">{chain.access_source.drive.name}</span>
				{/if}
			</span>
		{/if}
		{#each visibleCrumbs as c, i (c.id)}
			<span class="breadcrumb-separator">&gt;</span>
			<!--
				Every crumb links to `/files/{id}` — leaf included (Ed's
				2026-07-26 UX call: from `/search` clicking the leaf jumps
				INTO the searched folder in one click; from `/files` a
				leaf-click is a self-navigation no-op). The leaf gets
				`breadcrumb-current` for bold styling; intermediates stay
				regular weight. No underline on either — the hover
				background alone is the affordance.

				Drop-target props fire only when the host page passed an
				`onDrop` handler. Absent everywhere except `/files`.
			-->
			{@const isLeaf = i === visibleCrumbs.length - 1}
			<a
				href={resolve(`/files/${c.id}`)}
				class="breadcrumb-item breadcrumb-link"
				class:breadcrumb-current={isLeaf}
				class:drop-target={onDrop != null && dropTargetId === c.id}
				data-testid={isLeaf ? `folder-breadcrumb-current-${c.id}` : `folder-breadcrumb-${c.id}`}
				ondragover={onDrop
					? (e) => e.dataTransfer?.types.includes(dragMime) && e.preventDefault()
					: undefined}
				ondragenter={onDrop
					? (e) => {
							if (e.dataTransfer?.types.includes(dragMime)) dropTargetId = c.id;
						}
					: undefined}
				ondragleave={onDrop
					? () => {
							if (dropTargetId === c.id) dropTargetId = null;
						}
					: undefined}
				ondrop={onDrop
					? (e) => {
							dropTargetId = null;
							onDrop(c.id, e);
						}
					: undefined}
			>
				{c.name}
			</a>
		{/each}
	</nav>
{/if}

<style>
	/* Chip attached to the drive-root icon; only present in the drive
	   arm of access_source. Kept a tight max-width so a long drive name
	   truncates gracefully instead of shoving the breadcrumb off-screen. */
	.breadcrumb-root-name {
		margin-left: var(--space-1);
		max-width: 12ch;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Drop-target flicker fix: without this the SVG icon + name chip act
	   as event targets, so `dragenter` fires on the anchor → highlight
	   sets → pointer crosses into a child → `dragleave` fires on the
	   anchor → highlight clears (Ed's 2026-07-26 report). Pointer-events
	   off on children collapses the whole chip to a single drag target;
	   drop still lands because the anchor's own handlers stay live.
	   Intermediate crumbs don't need this (they contain only a text
	   node — no child element to cross into). */
	.breadcrumb-home > * {
		pointer-events: none;
	}
</style>
