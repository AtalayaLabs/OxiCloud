// Search endpoint — hits the normalized `/*/resources` envelope shape.
import { apiFetch, apiJson } from '$lib/api/client';
import type { ItemType, SearchResourcesResponse, SortBy } from '$lib/api/types';

export interface SearchOptions {
	folderId?: string;
	recursive?: boolean;
	fileTypes?: string[];
	minSize?: number;
	maxSize?: number;
	/** Unix-seconds lower bound on created time. */
	createdAfter?: number;
	/** Unix-seconds upper bound on created time. */
	createdBefore?: number;
	/** Unix-seconds lower bound on modified time. */
	modifiedAfter?: number;
	/** Unix-seconds upper bound on modified time. */
	modifiedBefore?: number;
	/** Page size (1–200 server-side; default 50). */
	limit?: number;
	/**
	 * Cursor from a previous response's `next_cursor`. Absent → first page.
	 * The wire uses cursor pagination now; the old `offset` param is gone.
	 */
	cursor?: string;
	/** Sort dimension; maps to backend `order_by`. */
	sortBy?: SortBy;
	/** Reverse sort direction; maps to backend `reverse`. */
	reverse?: boolean;
	/** Restrict to files, folders, or both (default). */
	resourceTypes?: ItemType[];
	/** Abort the request when a newer search supersedes it. */
	signal?: AbortSignal;
}

/**
 * Cursor-paginated search. Returns the shared envelope
 * `{ items[], next_cursor?, query_time_ms, total? }` — same shape as
 * favorites / recent / trash / folder listings so `ResourceList`
 * consumes the items without a demux step.
 */
export function searchResources(
	query: string,
	opts: SearchOptions = {}
): Promise<SearchResourcesResponse> {
	const params = new URLSearchParams();
	params.append('query', query);
	if (opts.folderId) params.append('folder_id', opts.folderId);
	if (opts.recursive !== undefined) params.append('recursive', String(opts.recursive));
	// The backend expects a single comma-separated `type` param (it splits on
	// ','); appending one param per type yields a "duplicate field" 400.
	if (opts.fileTypes?.length) params.append('type', opts.fileTypes.join(','));
	if (opts.minSize != null) params.append('min_size', String(opts.minSize));
	if (opts.maxSize != null) params.append('max_size', String(opts.maxSize));
	if (opts.createdAfter != null) params.append('created_after', String(opts.createdAfter));
	if (opts.createdBefore != null) params.append('created_before', String(opts.createdBefore));
	if (opts.modifiedAfter != null) params.append('modified_after', String(opts.modifiedAfter));
	if (opts.modifiedBefore != null) params.append('modified_before', String(opts.modifiedBefore));
	if (opts.resourceTypes?.length) params.append('resource_types', opts.resourceTypes.join(','));
	if (opts.limit != null) params.append('limit', String(opts.limit));
	if (opts.cursor) params.append('cursor', opts.cursor);
	if (opts.sortBy) params.append('order_by', opts.sortBy);
	if (opts.reverse) params.append('reverse', 'true');
	return apiJson<SearchResourcesResponse>(`/api/search?${params.toString()}`, {
		credentials: 'same-origin',
		signal: opts.signal
	});
}

/** A single autocomplete suggestion returned by the lightweight suggest endpoint. */
export interface SearchSuggestions {
	suggestions: string[];
	query_time_ms: number;
}

export interface SuggestOptions {
	folderId?: string;
	limit?: number;
	/** Abort the request when a newer keystroke supersedes it. */
	signal?: AbortSignal;
}

/**
 * Lightweight autocomplete suggestions from the backend `GET /api/search/suggest`
 * endpoint — name-only hints without the full search overhead.
 */
export function searchSuggest(
	query: string,
	opts: SuggestOptions = {}
): Promise<SearchSuggestions> {
	const params = new URLSearchParams();
	params.append('query', query);
	if (opts.folderId) params.append('folder_id', opts.folderId);
	if (opts.limit != null) params.append('limit', String(opts.limit));
	return apiJson<SearchSuggestions>(`/api/search/suggest?${params.toString()}`, {
		credentials: 'same-origin',
		signal: opts.signal
	});
}

/**
 * Clear the shared server-side search cache
 * (`DELETE /api/admin/search/cache`). Admin-only — moved from
 * `/api/search/cache` on 2026-07-17 because the underlying
 * `invalidate_all()` touches every tenant (see AuthZ audit #14).
 */
export async function clearSearchCache(): Promise<void> {
	const res = await apiFetch('/api/admin/search/cache', {
		method: 'DELETE',
		credentials: 'same-origin'
	});
	if (!res.ok) throw new Error(`Failed to clear search cache: ${res.status} ${res.statusText}`);
}
