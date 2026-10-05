/**
 * Reactive server-status store.
 *
 * Populated by the `apiFetch` wrapper, which reads the
 * `x-server-status` header off every API response and calls
 * `updateFromHeader(...)`. When no migration is running the header
 * is absent and the store stays at its default (readonly=false, no
 * migration info). See `middleware::server_status` on the server
 * for the header spec.
 *
 * The AppShell subscribes to this store to show/hide the
 * maintenance banner without polling — the state travels back to
 * the client on the piggyback of whatever API request the user was
 * making anyway. Zero extra network cost.
 */

import log from 'loglevel';

// Dedicated logger — silent by default; operators enable in devtools
// with `oxi.setLogLevel('oxi:server-status', 'debug')` to trace the
// WS push → refetch → store update path when banners / write-lock
// state fails to propagate.
const ssLog = log.getLogger('oxi:server-status');

/**
 * Progress snapshot shared by both migration and rotation fields.
 * Server-side struct is `ProgressHeader` — see
 * `middleware::server_status`.
 */
export interface ProgressStatus {
	target: string;
	migrated: number;
	total: number;
	percent: number;
}

/**
 * JSON shape emitted in the `x-server-status` header.
 *
 * * `migration` — present only during a `backend_migration` run;
 *   engages `readonly = true` (all writes are refused).
 * * `rotation` — present only during a `backend_rotate` run (K4
 *   storage-key-rotation); `readonly` stays false, uploads and
 *   reads continue normally throughout.
 *
 * Both can be `undefined` on the same response — that's the steady-
 * state "nothing running" case and the header may be omitted
 * entirely.
 */
export interface ServerStatus {
	readonly: boolean;
	migration?: ProgressStatus;
	rotation?: ProgressStatus;
	/** Public-safe projection of the backend write-lock holder;
	 *  mirrors the shape documented on
	 *  `src/interfaces/middleware/server_status.rs` ::
	 *  `HeaderPayload.holder`. */
	holder?: { kind: string; display: string };
	/** Short hash of the operator-authored banner list. On
	 *  mismatch with the store's current value, the apiFetch
	 *  wrapper triggers a `/api/config` refetch to pick up the
	 *  full `banners` list. */
	banners_version?: string;
	/** Full public-filtered banner list. ONLY populated by the
	 *  initial `/api/config` boot hydration or a version-diff
	 *  refetch — never by the X-Server-Status response header,
	 *  which carries only the version hash. */
	banners?: import('$lib/api/types').OpsBanner[];
}

const DEFAULT: ServerStatus = { readonly: false };

// Rune-based reactive state — `$state` in a `.svelte.ts` module.
let current = $state<ServerStatus>(DEFAULT);

/** Current server status. Reactively updates when apiFetch sees a new header. */
export function serverStatus(): ServerStatus {
	return current;
}

/**
 * Parse the raw header value and update the store. Silently
 * tolerates a missing header (resets to default: nothing to
 * broadcast means nothing wrong) and a malformed one (keeps the
 * previous value rather than surface a parse error to users).
 *
 * Called by `apiFetch` after every response — see `client.ts`.
 */
export function updateFromHeader(rawHeader: string | null): void {
	if (rawHeader == null) {
		// No header on this response = the response went through a
		// path OUTSIDE the server-status middleware stack. The
		// `/api/auth/*` nest is one such case (see the comment in
		// `interfaces/api/routes.rs::create_api_routes`), as are
		// `/api/wopi/*` and any future out-of-stack mount. The
		// server stamps the header UNCONDITIONALLY on any response
		// that reaches the middleware — even to signal "nothing
		// happening" — so an absent header reliably means "status
		// unknown via this response". Preserve all current state.
		// The authoritative source for state changes is a response
		// that DID stamp the header (any /api/* path within the
		// middleware), or the version-diff path below triggering a
		// `/api/config` refetch.
		return;
	}
	try {
		const parsed = JSON.parse(rawHeader) as ServerStatus;
		// Basic shape validation — server should never send a
		// missing `readonly`, but be defensive.
		if (typeof parsed.readonly === 'boolean') {
			const prevBannersVersion = current.banners_version;
			const prevBanners = current.banners;
			// Header carries `banners_version` only. Preserve the
			// previously-hydrated banner list across header-driven
			// updates — it refetches via the version-diff path below
			// when the hash actually changes.
			current = { ...parsed, banners: prevBanners };
			// Version-diff refetch: on first observed banners_version
			// (store had none) OR on hash mismatch, pull the full
			// `/api/config` body to pick up the actual list. Fire-and-
			// forget — the component re-renders via the reactive
			// `current` assignment inside `refetchBanners`.
			if (parsed.banners_version && parsed.banners_version !== prevBannersVersion) {
				void refetchBanners();
			} else if (!parsed.banners_version && prevBanners && prevBanners.length > 0) {
				// Server has no banners anymore (hash absent on the
				// header) — drop the list so the AppShell stops
				// rendering stale entries.
				current = { ...current, banners: undefined };
			}
		}
	} catch {
		// Malformed header — keep previous state rather than churn.
	}
}

/** Set the banner list directly — called from the serverConfig
 *  store on boot hydration, so the first page render already has
 *  the full list without waiting for a version-diff refetch. */
export function hydrateBanners(
	banners: import('$lib/api/types').OpsBanner[] | undefined,
	version: string | undefined
): void {
	current = { ...current, banners, banners_version: version };
}

/** Public alias of [`refetchBanners`] — called by the WS
 *  `Topic::ServerStatus` subscriber in `AppShell.svelte` when the
 *  server broadcasts `OpsBannerChanged`. Same code path as the
 *  header-diff refetch; the WS push just gets there faster. */
export function refetchServerStatusConfig(): Promise<void> {
	return refetchBanners();
}

/** Refetch `/api/config` and update the ENTIRE server-status slice
 *  from the fresh body. Called from `updateFromHeader` on version-
 *  diff AND from the WS `server:status` push handler.
 *
 *  Why the full replacement (not just banners): the push fires on
 *  any server-status axis — ops banners, write-lock holder,
 *  migration/rotation state, future global flags. Updating only
 *  banners left `readonly` / `holder` stale and the maintenance
 *  banner failed to appear on sibling tabs when an operator
 *  acquired the external write-lock.
 *
 *  The `fetchServerConfig()` call itself ALSO triggers
 *  `updateFromHeader` via `apiFetch` — but we overwrite with the
 *  body-side `server_status` after it resolves, so the two sources
 *  can't race to a stale value. The body is authoritative because
 *  it carries the full typed banner list (the header only has the
 *  version hash). */
async function refetchBanners(): Promise<void> {
	ssLog.debug('refetching /api/config');
	try {
		const mod = await import('$lib/api/endpoints/config');
		const cfg = await mod.fetchServerConfig();
		ssLog.debug('/api/config returned', cfg.server_status);
		// Replace from the fresh body — one atomic assignment so
		// Svelte's reactivity notifies one time, not per-field.
		current = {
			readonly: cfg.server_status.readonly,
			migration: cfg.server_status.migration,
			rotation: cfg.server_status.rotation,
			holder: cfg.server_status.holder,
			banners: cfg.server_status.banners,
			banners_version: cfg.server_status.banners_version
		};
	} catch (err) {
		ssLog.warn('refetch failed', err);
		// Transient — the next version-diff tick will try again.
	}
}
