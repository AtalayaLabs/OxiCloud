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
 * * `migration` — present only during a `storage_migration` run;
 *   engages `readonly = true` (all writes are refused).
 * * `rotation` — present only during a `storage_rotate` run (K4
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
		// No header on this response = nothing running server-side
		// = reset the store to the default so any lingering banner
		// disappears. Cheap idempotent write.
		if (current.readonly || current.migration || current.rotation) current = DEFAULT;
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
