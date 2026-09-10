// Svelte 5 rune wrapper around `messageBus.subscribe`.
//
// Call from a component's initialisation phase — `$effect` handles the
// mount/unmount lifecycle so the caller never sees the underlying
// WebSocket or the refcount plumbing. Two subscribers of the same
// topic share one wire subscription automatically (refcount lives in
// `MessageBusClient`).
//
// Reactive `topic`: pass a `$derived` or a getter and the composable
// re-subscribes when it changes. Static `topic`: pass a plain string.

import { messageBus } from '$lib/message-bus/client.svelte';
import type RtEventParams from '$lib/generated/message-bus/RtEventParams';
import type RtRevokedParams from '$lib/generated/message-bus/RtRevokedParams';

/**
 * Subscribe to `topic` for the lifetime of the calling component.
 *
 * Accepts `topic` as either a plain string or a getter — pass a
 * function returning the current topic when it's reactive (e.g.
 * derived from a route param) and `$effect` will re-subscribe when
 * the returned value changes. A `null` value means "not subscribed
 * right now" — useful during route load before the folder id is known.
 *
 * `onRevoked` fires when the server evicts the subscription (grant
 * revoked, folder deleted, etc.); by then the local state is already
 * cleared, so the handler can safely re-subscribe or navigate away.
 */
export function useTopic(
	topic: string | null | (() => string | null),
	onEvent: (params: RtEventParams) => void,
	onRevoked?: (params: RtRevokedParams) => void
): void {
	$effect(() => {
		const resolved = typeof topic === 'function' ? topic() : topic;
		if (!resolved) return;
		const release = messageBus.subscribe(resolved, onEvent, onRevoked);
		return () => release();
	});
}
