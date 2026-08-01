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

/**
 * JSON shape emitted in the `x-server-status` header. Optional
 * `migration` field is present only while a migration is running.
 */
export interface ServerStatus {
	readonly: boolean;
	migration?: {
		target: string;
		migrated: number;
		total: number;
		percent: number;
	};
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
		// No header on this response = server not in maintenance
		// mode = reset the store to the default so any lingering
		// banner disappears. Cheap idempotent write.
		if (current.readonly || current.migration) current = DEFAULT;
		return;
	}
	try {
		const parsed = JSON.parse(rawHeader) as ServerStatus;
		// Basic shape validation — server should never send a
		// missing `readonly`, but be defensive.
		if (typeof parsed.readonly === 'boolean') {
			current = parsed;
		}
	} catch {
		// Malformed header — keep previous state rather than churn.
	}
}
