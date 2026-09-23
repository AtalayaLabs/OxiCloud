import { describe, it, expect, vi, beforeEach } from 'vitest';

/**
 * A public-share visitor holds an ANONYMOUS session, and
 * `POST /api/rt/ticket` is off the anonymous allowlist by design — the
 * server refuses it and writes an `authz.denied` audit line every time.
 *
 * Handling the 403 gracefully still costs one line per connect attempt.
 * Not sending the request costs none, which is the point of this guard:
 * a frontend mistake must not become a permanent trickle of noise in a
 * security log, where it buries real denials.
 */
const { ticket, sess } = vi.hoisted(() => ({
	ticket: vi.fn(async () => ({ subprotocol: 'tkt', expires_in_seconds: 30 })),
	sess: { isAuthenticated: false, load: vi.fn(async () => null) }
}));

vi.mock('$lib/api/client', () => ({ apiJson: ticket }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
vi.mock('$app/paths', () => ({ base: '' }));
vi.mock('$lib/stores/session.svelte', () => ({ session: sess }));

import { MessageBusClient } from './client.svelte';

class FakeSocket {
	readyState = 0;
	binaryType = '';
	onopen: (() => void) | null = null;
	onmessage: ((ev: MessageEvent) => void) | null = null;
	onerror: ((ev: Event) => void) | null = null;
	onclose: ((ev: CloseEvent) => void) | null = null;
	send() {}
	close() {}
}

const newBus = () =>
	new MessageBusClient({
		url: 'ws://test/rt',
		WebSocketCtor: FakeSocket as unknown as typeof WebSocket
	});

const flush = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
	ticket.mockClear();
	sess.load.mockClear();
	sess.isAuthenticated = false;
});

describe('anonymous sessions never request a WS ticket', () => {
	it('does not POST /api/rt/ticket without an authenticated session', async () => {
		const bus = newBus();
		bus.subscribe('collab:x', () => {});
		await flush();

		expect(sess.load).toHaveBeenCalled(); // it did check
		expect(ticket).not.toHaveBeenCalled(); // …and then kept quiet
	});

	it('leaves the bus recoverable rather than terminally unavailable', async () => {
		const bus = newBus();
		bus.subscribe('collab:x', () => {});
		await flush();

		// `unavailable` is the circuit-breaker state, re-armed only by an
		// explicit `reconnect()` that nothing calls on login. Parking
		// there would leave a user who signs in inside the same tab with
		// a bus that never connects again.
		expect(bus.state).not.toBe('unavailable');
	});

	it('does request a ticket once a session exists', async () => {
		sess.isAuthenticated = true;
		const bus = newBus();
		bus.subscribe('collab:x', () => {});
		await flush();

		// The positive control: without this the first test would pass
		// even if the guard refused everyone.
		expect(ticket).toHaveBeenCalledTimes(1);
	});
});
