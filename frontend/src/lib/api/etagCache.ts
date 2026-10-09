/**
 * SPA-level ETag cache on top of `apiFetch` — §2b of
 * `docs/plan/photos-resources-migration.md`.
 *
 * Server-side ETag headers (shipped by §2 via `CursorListResponse::weak_etag`
 * on every `/resources` endpoint) are only half the picture: they save
 * bandwidth only if the client actually sends `If-None-Match` on refetch AND
 * reuses the previous body on 304. The browser's HTTP cache does this
 * transparently for `apiFetch` already, so this module is NOT a replacement
 * — it is an in-memory JSON layer that stays alive after the browser cache
 * has been purged (private tabs, mobile under memory pressure) and that
 * exposes the "render immediately from cache, then revalidate" pattern the
 * browser cache can't surface.
 *
 * Keyed by the full URL (including query string), so each
 * `/api/photos/resources?...` filter combination gets its own entry. The
 * store is a FIFO-with-overflow — no explicit TTL; revalidation keeps
 * entries fresh and overflow evicts the oldest once `MAX_ENTRIES` is hit.
 *
 * Mutations invalidate by URL prefix: a photo upload drops every entry whose
 * URL starts with `/api/photos/resources`. See `invalidatePrefix` for the
 * contract.
 */
import { apiFetch } from '$lib/api/client';

interface CacheEntry<T> {
	etag: string;
	body: T;
	storedAt: number;
}

/**
 * FIFO cap. Listings are small JSON objects (one page per entry); 32 covers
 * every open view in a single session (five `/resources` roots × a handful
 * of filter combos each) without approaching anything that would show up in
 * profiling. Overflow drops the oldest entry.
 */
const MAX_ENTRIES = 32;

// `Map` preserves insertion order, so `store.keys().next().value` is the
// oldest key — standard "poor man's FIFO" pattern.
const store = new Map<string, CacheEntry<unknown>>();

function remember<T>(url: string, etag: string, body: T): void {
	if (store.size >= MAX_ENTRIES) {
		const oldest = store.keys().next().value;
		if (oldest !== undefined) store.delete(oldest);
	}
	store.set(url, { etag, body, storedAt: Date.now() });
}

export interface EtaggedResult<T> {
	body: T;
	/**
	 * How the body was sourced:
	 * - `'network'` — a 200 response, body freshly parsed and cached
	 * - `'revalidated'` — a 304 response reused the cached body (ETag matched)
	 * - `'cache'` — reserved for the stale-while-revalidate variant below
	 */
	from: 'network' | 'revalidated' | 'cache';
}

/**
 * ETag-aware GET.
 *
 * Attaches `If-None-Match: <stored etag>` when a prior response for `url` is
 * cached; on 304 returns the cached body (zero wire bytes beyond the request
 * headers). On 200 parses + caches the new body under the response's `ETag`
 * header. On any other non-ok status, throws.
 *
 * Only safe for pure GETs — write paths skip this entirely so there's no
 * question about cache-side effects of a POST.
 */
export async function apiFetchEtagged<T>(
	url: string,
	opts: RequestInit = {}
): Promise<EtaggedResult<T>> {
	const prev = store.get(url) as CacheEntry<T> | undefined;
	const headers = new Headers(opts.headers);
	if (prev) headers.set('If-None-Match', prev.etag);
	const res = await apiFetch(url, { ...opts, headers });
	if (res.status === 304 && prev) {
		// 304 refreshes the entry's position in the FIFO so a hot URL
		// doesn't age out of the cache under overflow pressure. Delete
		// + re-insert keeps the Map ordered newest-last.
		store.delete(url);
		store.set(url, prev);
		return { body: prev.body, from: 'revalidated' };
	}
	if (!res.ok) {
		// Attach the status so callers that discriminate on specific
		// codes (e.g. folders.ts rethrows 403 as "Forbidden") can
		// read `.status` on the caught error — same shape as the
		// pre-adoption `Object.assign(new Error(...), { status })`
		// pattern used across the endpoints.
		throw Object.assign(new Error(`${url}: ${res.status}`), { status: res.status });
	}
	const body = (await res.json()) as T;
	const etag = res.headers.get('ETag');
	if (etag) remember(url, etag, body);
	return { body, from: 'network' };
}

/**
 * Drop every cached entry whose URL starts with `prefix`. The invalidation
 * granularity is intentionally coarse: a photo upload drops the whole
 * `/api/photos/resources` family (every filter / cursor permutation) rather
 * than trying to prove which pages the mutation actually affects. The cost
 * is one extra refetch per cached filter combo on next read, and the
 * payoff is zero false-negatives on invalidation — a race where a cached
 * entry outlives the mutation that should have invalidated it is a user-
 * visible correctness bug; a redundant refetch is just a bit of bandwidth
 * covered by the server-side ETag anyway (a 304 if the server hasn't
 * actually changed state).
 *
 * Callers from mutation paths (upload, trash, favorite toggle, …) invoke
 * this against the listing roots they know they can affect. The
 * `/api/photos/resources` root covers kind + drive + favorite_only
 * combinations because they all share that prefix.
 */
export function invalidatePrefix(prefix: string): void {
	for (const key of store.keys()) {
		if (key.startsWith(prefix)) store.delete(key);
	}
}

/** Test-only: wipe the entire cache. */
export function _resetEtagCacheForTest(): void {
	store.clear();
}
