/**
 * Pure display helpers for resource listings — group-by labels.
 *
 * These used to live next to the favorites API wrappers in
 * `$lib/api/endpoints/favorites.ts`, which conflated two concerns:
 * HTTP endpoint calls and view-layer formatting. The helpers below
 * are reused across every listing route (favorites, recent, trash,
 * search, shared-with-me, photos) so they belong in `utils/`, not
 * under `api/endpoints/`. The move is prep for Phase 4 of the
 * OpenAPI migration — once the API wrappers finish migrating to the
 * generated SDK, `endpoints/favorites.ts` disappears entirely and
 * these helpers already live in a stable home.
 */
import { t } from '$lib/i18n/index.svelte';
import type { ResourceCategory } from '$lib/api/types';

/**
 * Coarse "how long ago" bucket for date group-bys (favorited /
 * accessed / modified). Returns a locale-sensitive label, or `null`
 * when the input isn't a usable date.
 *
 * Ported from `normalizeDateBucket` in the legacy
 * `static/js/core/formatters.js`.
 */
export function dateBucket(value: number | string | null | undefined): string | null {
	if (value === null || value === undefined) return null;
	let date: Date;
	if (typeof value === 'number') date = new Date(value < 1e12 ? value * 1000 : value);
	else date = new Date(value);
	if (Number.isNaN(date.getTime())) return null;
	const diffDays = Math.floor((Date.now() - date.getTime()) / 86_400_000);
	if (diffDays <= 0) return t('dateBucket.today', 'Today');
	if (diffDays <= 7) return t('dateBucket.last7days', 'Last 7 days');
	if (diffDays <= 30) return t('dateBucket.last30days', 'Last 30 days');
	return String(date.getFullYear());
}

/**
 * Coarse size bucket label. Pass `null` for folders — they receive
 * the "Folders" bucket rather than being lumped with files by size.
 */
export function sizeBucket(bytes: number | null | undefined): string {
	if (bytes === null || bytes === undefined) return t('sizeBucket.folders', 'Folders');
	if (bytes === 0) return t('sizeBucket.empty', 'Empty (0 B)');
	if (bytes < 1_048_576) return t('sizeBucket.tiny', '< 1 MB');
	if (bytes < 104_857_600) return t('sizeBucket.small', '1 – 100 MB');
	if (bytes < 1_073_741_824) return t('sizeBucket.medium', '100 MB – 1 GB');
	if (bytes < 5 * 1_073_741_824) return t('sizeBucket.large', '1 – 5 GB');
	return t('sizeBucket.huge', '> 5 GB');
}

/** Every `ResourceCategory` the server can send, as a runtime list.
 *
 * Needed because a type union does not exist at runtime and group-by
 * bucket keys arrive as plain strings. `satisfies` ties it to the union,
 * so a category added server-side fails the type-check here too.
 */
const CATEGORIES = [
	'Image',
	'Video',
	'Audio',
	'PDF',
	'WordProcessor',
	'Spreadsheet',
	'Presentation',
	'Archive',
	'Code',
	'Markdown',
	'Text',
	'Package',
	'Config',
	'DiskImage',
	'Font',
	'Ebook',
	'Executable',
	'Folder',
	'Unknown'
] as const satisfies readonly ResourceCategory[];

/** Label a group-by bucket key that is only known to be a string.
 *
 * The group-by machinery keys buckets by `string` — owner names, date and
 * size buckets all share that path — so the `type` group-by cannot hand
 * `typeLabel` a narrowed value. Rather than casting (which would let an
 * unexpected value render as a raw English key again, the exact bug the
 * typed union closed), this narrows at runtime and falls back to the
 * localised Unknown label.
 */
export function typeLabelOf(key: string): string {
	return (CATEGORIES as readonly string[]).includes(key)
		? typeLabel(key as ResourceCategory)
		: t('category.unknown', 'Unknown');
}

/** Human label for a resource `category` / type group-by bucket.
 *
 * `Record<ResourceCategory, string>` rather than `Record<string, string>`:
 * the map must be EXHAUSTIVE, so adding a server-side category fails the
 * type-check here instead of silently rendering its raw English key. That
 * passthrough was the i18n bug step 1 of
 * `retire-precomputed-display-fields.md` fixed, and an inexhaustive map
 * would reintroduce it one category at a time.
 *
 * The label is a free choice — the enum value is a machine key, so
 * `Executable` can read "Applications" without touching the wire.
 */
export function typeLabel(category: ResourceCategory): string {
	const labels: Record<ResourceCategory, string> = {
		Image: t('category.images', 'Images'),
		Video: t('category.videos', 'Videos'),
		Audio: t('category.audio', 'Audio'),
		'PDF': 'PDF',
		WordProcessor: t('category.documents', 'Documents'),
		Spreadsheet: t('category.spreadsheets', 'Spreadsheets'),
		Presentation: t('category.presentations', 'Presentations'),
		Archive: t('category.archives', 'Archives'),
		Code: t('category.code', 'Code'),
		Markdown: t('category.markdown', 'Markdown'),
		Text: t('category.text', 'Text'),
		Package: t('category.packages', 'Packages'),
		Config: t('category.config', 'Configuration'),
		DiskImage: t('category.disk_images', 'Disk images'),
		Font: t('category.fonts', 'Fonts'),
		Ebook: t('category.ebooks', 'Ebooks'),
		Executable: t('category.executables', 'Applications'),
		Folder: t('groupby.folders', 'Folders'),
		Unknown: t('category.unknown', 'Unknown')
	};
	return labels[category];
}

