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

/** Boot-time hydration of the authenticated server-status snapshot.
 *  Called from the root layout right after `session.load()` resolves
 *  to an authenticated session, so the store starts with the full
 *  payload (readonly + migration + rotation + holder + banners)
 *  instead of waiting for the first other request's `X-Server-Status`
 *  header to arrive — critical for pages that render maintenance /
 *  migration banners on mount without firing an API call of their own.
 *
 *  Same wire call as [`refetchBanners`], distinct name to make the
 *  caller's intent legible ("I'm hydrating the store", not "banners
 *  may have changed"). Idempotent — multiple calls just re-fetch. */
export function hydrateFromAuthenticatedBoot(): Promise<void> {
	return refetchBanners();
}

/** Refetch the full server-status payload and update the ENTIRE
 *  slice from it. Called from `updateFromHeader` on version-diff
 *  AND from the WS `server:status` push handler.
 *
 *  Why the full replacement (not just banners): the push fires on
 *  any server-status axis — ops banners, write-lock holder,
 *  migration/rotation state, future global flags. Updating only
 *  banners left `readonly` / `holder` stale and the maintenance
 *  banner failed to appear on sibling tabs when an operator
 *  acquired the external write-lock.
 *
 *  Endpoint choice: `/api/status` (authenticated) carries the full
 *  `HeaderPayload`; `/api/config.server_status` carries only the
 *  public-safe subset (readonly + banners), by design — see
 *  `middleware/server_status.rs::PublicHeaderPayload` for the
 *  privacy rationale (migration.target, holder.display leak
 *  infrastructure topology, hence auth-gated).
 *
 *  Try `/api/status` first; a 401 means we're pre-login (the WS
 *  bus is session-gated too, so a ServerStatusChanged push without
 *  a session shouldn't normally happen — but handles the paranoid
 *  case). On 401, fall back to `/api/config` for a banner-only
 *  refresh (readonly stays whatever the public config says). */
async function refetchBanners(): Promise<void> {
	ssLog.debug('refetching /api/status');
	try {
		// Dynamic import defers the SDK operation until the first
		// version-diff refetch; the boot path pulls the config via the
		// static import on `serverConfig.svelte.ts`.
		const { getServerStatus } = await import('$lib/api/generated/openapi/sdk.gen');
		const payload = await getServerStatus({ throwOnError: true });
		ssLog.debug('/api/status returned', payload);
		// Replace from the fresh body — one atomic assignment so
		// Svelte's reactivity notifies one time, not per-field.
		// Coalesce `null → undefined` at the SDK/store boundary. The
		// generated SDK models absent sub-fields as `null | undefined`
		// (serde `#[serde(skip_serializing_if = …)]` + an explicit
		// nullable in the schema), while the store deliberately speaks
		// `undefined`-only — a single "absence" shape keeps conditional
		// rendering (`{#if store.migration}`) idiomatic.
		current = {
			readonly: payload.readonly,
			migration: payload.migration ?? undefined,
			rotation: payload.rotation ?? undefined,
			holder: payload.holder ?? undefined,
			banners: payload.banners ?? undefined,
			banners_version: payload.banners_version ?? undefined
		};
	} catch (err) {
		ssLog.warn('refetch failed', err);
		// No fallback: `/api/config` no longer carries any runtime
		// status, so there's nothing useful to probe here. A 401
		// means we're pre-login — the login page doesn't render
		// banners anyway, and the store will hydrate properly as
		// soon as the user logs in via `hydrateFromAuthenticatedBoot`
		// in the root layout. Any other failure is transient and
		// the next header-diff or ServerStatusChanged push will
		// try again.
	}
}
