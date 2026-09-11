// Svelte 5 rune wrapper around `messageBus.onReconnect`.
//
// Fires the given callback the first time the WS reconnects after a
// prior disconnect (server restart, network blip, sleep/wake).
// **Not** called on the initial connect — the caller's own load path
// is already fetching then. Bridges the "events published during the
// disconnect window are lost" gap; consumers typically pass
// `reload()` so the view catches up with the server after the outage.
//
// See `client.svelte.ts::onReconnect` for lifecycle details and
// `project_message_bus_reconnect_gap` memory for the gap it closes.

import { messageBus } from '$lib/message-bus/client.svelte';

/**
 * Register `cb` as a reconnect handler for the lifetime of the
 * calling component. Auto-unregisters on destroy via `$effect`
 * cleanup. Passing `null`/`undefined` is a no-op — convenient for
 * conditional wiring (`useReconnect(handlers.onReconnect)`).
 */
export function useReconnect(cb: (() => void) | null | undefined): void {
	$effect(() => {
		if (!cb) return;
		const release = messageBus.onReconnect(cb);
		return () => release();
	});
}
