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

/** Human label for a resource `category` / type group-by bucket. */
export function typeLabel(category: string): string {
	const labels: Record<string, string> = {
		Folder: t('groupby.folders', 'Folders'),
		Image: t('category.images', 'Images'),
		Video: t('category.videos', 'Videos'),
		Audio: t('category.audio', 'Audio'),
		PDF: 'PDF',
		Document: t('category.documents', 'Documents'),
		Spreadsheet: t('category.spreadsheets', 'Spreadsheets'),
		Presentation: t('category.presentations', 'Presentations'),
		Archive: t('category.archives', 'Archives'),
		Code: t('category.code', 'Code'),
		Markdown: t('category.markdown', 'Markdown'),
		Text: t('category.text', 'Text'),
		Installer: t('category.installers', 'Installers')
	};
	return labels[category] ?? category;
}
