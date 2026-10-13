/**
 * Files view state — replaces the navigation-related fields of the original `app`
 * state object (currentFolder, currentFolderInfo, breadcrumbPath, view mode,
 * section, selection). Dialog/context-menu targets stay component-local until a
 * view proves they must be shared.
 */
import { SvelteSet } from 'svelte/reactivity';
import type { FolderItem } from '$lib/api/types';
import { t } from '$lib/i18n/index.svelte';

// Re-exported so the files view's grouping-helper barrel stays a single import
// site; the implementation lives in the shared time util.
export { relativeTimeAgo } from '$lib/utils/time';

export type ViewMode = 'grid' | 'list';

// ── Group-by / display helpers ───────────────────────────────────────────────
// Ported from static/js/core/formatters.js (sizeBucket, normalizeDateBucket,
// formatRelativeTime) and static/js/components/resourceList.js (type label,
// owner label). Pure functions, shared by the files view's swimlane grouping
// and cell rendering so the same bucketing logic isn't duplicated per call site.

/** Normalise a timestamp into a Date. Accepts an RFC 3339 UTC string
 * (modern wire shape, post the ISO-8601 pivot) or an epoch number in
 * seconds OR milliseconds (legacy fixtures / numeric sort axes). */
function toDate(value: number | string): Date {
	if (typeof value === 'string') return new Date(value);
	return new Date(value < 1e12 ? value * 1000 : value);
}

/** Coarse size bucket label. `bytes < 0` is the "Folders" sentinel. */
export function sizeBucket(bytes: number): string {
	if (bytes < 0) return t('sizeBucket.folders', 'Folders');
	if (bytes === 0) return t('sizeBucket.empty', 'Empty (0 B)');
	if (bytes < 1_048_576) return t('sizeBucket.tiny', '< 1 MB');
	if (bytes < 104_857_600) return t('sizeBucket.small', '1 – 100 MB');
	if (bytes < 1_073_741_824) return t('sizeBucket.medium', '100 MB – 1 GB');
	if (bytes < 5 * 1_073_741_824) return t('sizeBucket.large', '1 – 5 GB');
	return t('sizeBucket.huge', '> 5 GB');
}

/** Coarse date bucket: Today | Last 7 days | Last 30 days | <YYYY>. */
export function dateBucket(value: number | string | null | undefined): string {
	if (!value) return t('dateBucket.unknown', 'Unknown');
	const diffDays = Math.floor((Date.now() - toDate(value).getTime()) / 86_400_000);
	if (diffDays <= 0) return t('dateBucket.today', 'Today');
	if (diffDays <= 7) return t('dateBucket.last7days', 'Last 7 days');
	if (diffDays <= 30) return t('dateBucket.last30days', 'Last 30 days');
	return String(toDate(value).getFullYear());
}

/** Localise a file `category` (e.g. "Image") via files.file_types.* keys. */
export function typeLabel(category: string | null | undefined): string {
	if (!category) return t('files.file_types.document', 'Document');
	return t(`files.file_types.${category.toLowerCase()}`, category);
}

/** Owner display: "Me" for the current user, else a short id fallback. */
export function ownerLabel(
	ownerId: string | null | undefined,
	currentUserId: string | null
): string {
	if (!ownerId) return '';
	if (currentUserId && ownerId === currentUserId) return t('files.owner_me', 'Me');
	return ownerId.slice(0, 8);
}

export type Section =
	| 'files'
	| 'shared'
	| 'shared-with-me'
	| 'recent'
	| 'favorites'
	| 'trash'
	| 'photos'
	| 'music';

const VIEW_KEY = 'oxi-view-mode';

function readViewMode(): ViewMode {
	if (typeof localStorage === 'undefined') return 'grid';
	return localStorage.getItem(VIEW_KEY) === 'list' ? 'list' : 'grid';
}

class FilesStore {
	currentFolder = $state<string | null>(null);
	currentFolderInfo = $state<FolderItem | null>(null);
	breadcrumbPath = $state<Array<{ id: string; name: string }>>([]);
	// View mode INTENTIONALLY lives here (localStorage) rather than in
	// the server-side `preferences` bag. See the note in
	// `preferences.svelte.ts::UiPreferences` for the full rationale —
	// short version: server persistence broke Playwright test
	// isolation (favorites.spec's list-view click leaked into every
	// downstream test's context), and view mode isn't a preference
	// users have asked to sync across devices.
	viewMode = $state<ViewMode>(readViewMode());
	section = $state<Section>('files');
	isSearchMode = $state(false);
	// Reactive set: in-place mutations below drive template/$derived reads.
	selection = new SvelteSet<string>();

	setViewMode(mode: ViewMode): void {
		this.viewMode = mode;
		if (typeof localStorage !== 'undefined') localStorage.setItem(VIEW_KEY, mode);
	}

	clearSelection(): void {
		this.selection.clear();
	}

	// Soft ceiling so the per-item toggle can't grow the set without bound.
	// (Bulk "select all" lives in the views and intentionally isn't capped —
	// silently dropping ids there would break batch delete/move.)
	static readonly MAX_SELECTION = 10_000;

	toggleSelected(id: string): void {
		if (this.selection.has(id)) this.selection.delete(id);
		else if (this.selection.size < FilesStore.MAX_SELECTION) this.selection.add(id);
	}
}

export const files = new FilesStore();
