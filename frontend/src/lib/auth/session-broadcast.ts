/**
 * Cross-tab session invalidation via `BroadcastChannel` — the
 * "logout on tab A, tab B knows immediately" wire.
 *
 * Why this exists: after a logout on Tab A, Tab B still holds an
 * in-memory `CryptoKey` handle to the (now-cleared) DPoP keypair
 * and a session cookie whose server-side row was just revoked.
 * Without a cross-tab signal, Tab B doesn't notice until its next
 * network request — at which point the server 401s and the SPA
 * bounces to `/login`. That's correctness-safe (see
 * `docs/plan/dpop.md` Gate 8), but UX-poor: an idle Tab B silently
 * pretends to be logged in for as long as it stays idle.
 *
 * This module fires a `BroadcastChannel` message so every other
 * tab of the same origin can react synchronously — reset its
 * session store, redirect to `/login`, no visible drift.
 *
 * Scope: session **invalidation** only. Not for cross-tab login
 * (a fresh sign-in on Tab B while Tab A sits on `/login`); that's
 * a general auth-store consistency concern, not DPoP-specific,
 * and can be layered on later using the same primitive if needed.
 *
 * Fail-open contract mirrors the rest of the DPoP stack: if
 * `BroadcastChannel` is unavailable (very old Safari, restricted
 * webviews), broadcast/subscribe are no-ops. Users lose the
 * instant-redirect UX; the natural 401-on-next-request path
 * kicks in as before.
 */

const CHANNEL_NAME = 'oxicloud-session-cleared';

/**
 * Post a "session cleared" event to every other tab of this
 * origin. The current tab does NOT receive its own message —
 * `BroadcastChannel` skips the sender by design.
 *
 * Called from `logout()` after the server round trip completes
 * (success or failure — user intent is what matters). Non-fatal
 * on failure so the logout flow always finishes.
 */
export function broadcastSessionCleared(): void {
	try {
		const ch = new BroadcastChannel(CHANNEL_NAME);
		ch.postMessage({ kind: 'session_cleared', at: Date.now() });
		ch.close();
	} catch (err) {
		console.debug('session-broadcast: postMessage failed', err);
	}
}

/**
 * Subscribe to cross-tab session-cleared events. Wire this once
 * from the root layout's `onMount`; the callback should reset
 * the SPA's session store and navigate to `/login`.
 *
 * Returns a cleanup function that closes the channel — call it
 * from the layout's `onDestroy` so hot-reload during dev doesn't
 * leak listeners.
 *
 * Errors during subscription are swallowed to a no-op: same
 * degradation posture as the rest of the DPoP stack.
 */
export function onSessionCleared(callback: () => void): () => void {
	try {
		const ch = new BroadcastChannel(CHANNEL_NAME);
		ch.onmessage = () => {
			try {
				callback();
			} catch (err) {
				console.debug('session-broadcast: callback threw', err);
			}
		};
		return () => ch.close();
	} catch (err) {
		console.debug('session-broadcast: subscribe failed', err);
		return () => {};
	}
}
