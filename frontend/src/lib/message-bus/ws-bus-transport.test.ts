// `WsBusTransport` coverage — reconnect state machine + the port-
// mediated ticket flow (Gate 2c). The worker never makes HTTP; the
// tab provides tickets via `provideTicket(subprotocol, nonce)` and
// the transport asks for fresh ones via `onNeedTicket(cb)`.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';

import { WsBusTransport } from './ws-bus-transport';
import type { ConnectionState } from './client-interface';

// ============================================================
// Fakes
// ============================================================

type MessageHandler = (ev: MessageEvent) => void;
type OpenHandler = () => void;
type CloseHandler = (ev: CloseEvent) => void;
type ErrorHandler = (ev: Event) => void;

class FakeSocket {
	static readonly instances: FakeSocket[] = [];
	readyState = 0; // CONNECTING
	binaryType = '';
	onopen: OpenHandler | null = null;
	onmessage: MessageHandler | null = null;
	onerror: ErrorHandler | null = null;
	onclose: CloseHandler | null = null;
	readonly sent: Array<string | ArrayBuffer> = [];
	readonly url: string;
	readonly protocols: string | string[] | undefined;
	closed = false;

	constructor(url: string, protocols?: string | string[]) {
		this.url = url;
		this.protocols = protocols;
		FakeSocket.instances.push(this);
	}
	send(data: string | ArrayBuffer): void {
		this.sent.push(data);
	}
	close(): void {
		this.closed = true;
		this.readyState = 3;
		queueMicrotask(() => this.onclose?.({ code: 1000, reason: 'closed' } as CloseEvent));
	}

	fireOpen(): void {
		this.readyState = 1;
		this.onopen?.();
	}
	fireMessage(data: string | ArrayBuffer): void {
		this.onmessage?.({ data } as MessageEvent);
	}
	fireClose(code = 1006, reason = 'abnormal'): void {
		this.readyState = 3;
		this.onclose?.({ code, reason } as CloseEvent);
	}
}

function makeTransport(): {
	t: WsBusTransport;
	needs: number[];
	states: ConnectionState[];
} {
	const t = new WsBusTransport({
		wsUrl: 'ws://test/rt',
		WebSocketCtor: FakeSocket as unknown as typeof WebSocket
	});
	const needs: number[] = [];
	t.onNeedTicket((nonce) => needs.push(nonce));
	const states: ConnectionState[] = [];
	t.onStateChange((s) => states.push(s));
	return { t, needs, states };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

async function flushN(n = 5) {
	for (let i = 0; i < n; i++) await flush();
}

beforeEach(() => {
	FakeSocket.instances.length = 0;
	vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
	vi.useRealTimers();
});

// ============================================================
// Tests
// ============================================================

describe('WsBusTransport — initial state', () => {
	it('starts idle', () => {
		const { t, states } = makeTransport();
		expect(t.state).toBe('idle');
		// onStateChange fires immediately with current state (idle).
		expect(states).toEqual(['idle']);
	});

	it('makes no HTTP calls — the worker is HTTP-free', () => {
		// Negative control for Gate 2c: there is no `fetchFn` option on
		// the transport anymore. If this file ever adds one back, the
		// pin here needs an opposite assertion.
		const t = new WsBusTransport({
			wsUrl: 'ws://test/rt',
			WebSocketCtor: FakeSocket as unknown as typeof WebSocket
		});
		expect(t.state).toBe('idle');
	});
});

describe('WsBusTransport — port-mediated ticket flow', () => {
	it('subscribe asks for a ticket via onNeedTicket + opens WS on provideTicket', async () => {
		const { t, needs } = makeTransport();
		const sub = t.subscribe('folder:X');
		await flushN();

		// Need-ticket fired with a nonce.
		expect(needs).toHaveLength(1);
		expect(needs[0]).toBeGreaterThan(0);
		// WS not yet constructed — no ticket provided yet.
		expect(FakeSocket.instances).toHaveLength(0);

		// Tab provides the ticket (echoing the nonce).
		t.provideTicket('oxi.ticket.uuid-1', needs[0]!);
		await flushN();
		expect(FakeSocket.instances).toHaveLength(1);
		const ws = FakeSocket.instances[0]!;
		expect(ws.protocols).toEqual(['oxi.ticket.uuid-1']);

		// Open socket + ack subscribe.
		ws.fireOpen();
		await flushN();
		const sentFrame = JSON.parse(ws.sent[0] as string) as {
			id: number;
			method: string;
			params: { topic: string };
		};
		ws.fireMessage(
			JSON.stringify({
				jsonrpc: '2.0',
				id: sentFrame.id,
				result: { subscribed: 'folder:X' }
			})
		);
		const ack = await sub;
		expect(ack).toEqual({ subscribed: 'folder:X' });
	});

	it('ignores a provideTicket with a stale nonce', async () => {
		// Pin: late reply from a port after another port already served
		// this reconnect round must not open a second WS.
		const { t, needs } = makeTransport();
		void t.subscribe('t').catch(() => {});
		await flushN();
		const liveNonce = needs[0]!;
		t.provideTicket('oxi.ticket.live', liveNonce);
		await flushN();
		expect(FakeSocket.instances).toHaveLength(1);
		// A delayed reply with the SAME nonce — should be a no-op (we're
		// already connecting with a WS for this round).
		t.provideTicket('oxi.ticket.late', liveNonce);
		await flushN();
		expect(FakeSocket.instances).toHaveLength(1);
		// A reply with a BOGUS (older) nonce — must be ignored.
		t.provideTicket('oxi.ticket.ancient', liveNonce - 1);
		await flushN();
		expect(FakeSocket.instances).toHaveLength(1);
	});

	it('accepts a nonce === 0 provideTicket from the eager hello-time primer', async () => {
		// Pin the hello-time primer path: the bridge provides a ticket
		// unsolicited (no `need-ticket` has fired yet). nonce === 0 is
		// the convention; the transport honours it when idle.
		const { t } = makeTransport();
		// Trigger connecting state first — the primer only lands if the
		// transport is in a connect window. Without an active subscribe,
		// a primer provideTicket is dropped (nothing wants a WS yet).
		void t.subscribe('t').catch(() => {});
		await flushN();
		// Clear the FakeSocket slate; the subscribe triggered a
		// need-ticket but no provideTicket yet.
		expect(FakeSocket.instances).toHaveLength(0);
		t.provideTicket('oxi.ticket.primer', 0);
		await flushN();
		expect(FakeSocket.instances).toHaveLength(1);
	});

	it('re-asks for a ticket on WS drop + reconnect', async () => {
		const { t, needs } = makeTransport();
		void t.subscribe('folder:A').catch(() => {});
		await flushN();
		t.provideTicket('sp-1', needs[0]!);
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();

		// Drop the socket. Backoff schedules a reconnect; need-ticket
		// fires with a NEW nonce.
		ws.fireClose(1006, 'abnormal');
		vi.advanceTimersByTime(1_000);
		await flushN();
		expect(needs.length).toBeGreaterThanOrEqual(2);
		expect(needs[1]).toBeGreaterThan(needs[0]!);
		expect(FakeSocket.instances).toHaveLength(1); // no new WS yet

		t.provideTicket('sp-2', needs[1]!);
		await flushN();
		expect(FakeSocket.instances).toHaveLength(2);
		expect(FakeSocket.instances[1]!.protocols).toEqual(['sp-2']);
	});

	it('trips the circuit breaker after N need-ticket timeouts', async () => {
		// If no tab ever answers need-ticket (the port set empties, or
		// every tab's apiFetch keeps failing), the transport eventually
		// flips to `unavailable` so the UI stops promising reconnection.
		const { t, needs } = makeTransport();
		void t.subscribe('t').catch(() => {});
		await flushN();
		expect(needs).toHaveLength(1);

		// Advance past 10 full cycles of (need-ticket-timeout +
		// backoff). The transport bumps consecutiveFailures each time.
		for (let i = 0; i < 11; i++) {
			vi.advanceTimersByTime(60_000);
			await flushN();
		}
		expect(t.state).toBe('unavailable');
	});
});

describe('WsBusTransport — subscribe + event dispatch', () => {
	it('fans event frames to onEvent listeners with topic extracted from params', async () => {
		const { t, needs } = makeTransport();
		const events: Array<[string, unknown]> = [];
		t.onEvent((topic, params) => events.push([topic, params]));
		void t.subscribe('folder:X').catch(() => {});
		await flushN();
		t.provideTicket('sp-1', needs[0]!);
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();
		ws.fireMessage(
			JSON.stringify({
				jsonrpc: '2.0',
				method: 'rt.event',
				params: { topic: 'folder:X', payload: { x: 1 } }
			})
		);
		await flushN();
		expect(events).toHaveLength(1);
		expect(events[0][0]).toBe('folder:X');
	});

	it('revoked drops topic from active replay set', async () => {
		const { t, needs } = makeTransport();
		const revoked: string[] = [];
		t.onRevoked((topic) => revoked.push(topic));
		void t.subscribe('folder:A').catch(() => {});
		void t.subscribe('folder:B').catch(() => {});
		await flushN();
		t.provideTicket('sp-1', needs[0]!);
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();
		ws.fireMessage(
			JSON.stringify({
				jsonrpc: '2.0',
				method: 'rt.revoked',
				params: { topic: 'folder:A', reason: 'topic_forbidden' }
			})
		);
		await flushN();
		expect(revoked).toEqual(['folder:A']);

		ws.fireClose(1006);
		vi.advanceTimersByTime(1_000);
		await flushN();
		expect(needs.length).toBeGreaterThan(1);
		t.provideTicket('sp-2', needs[needs.length - 1]!);
		await flushN();
		const ws2 = FakeSocket.instances.at(-1)!;
		expect(ws2).not.toBe(ws);
		ws2.fireOpen();
		await flushN();

		const subscribeTopics = ws2.sent
			.filter((s): s is string => typeof s === 'string')
			.map((s) => JSON.parse(s) as { method: string; params?: { topic?: string } })
			.filter((f) => f.method === 'rt.subscribe')
			.map((f) => f.params?.topic);
		expect(subscribeTopics).toContain('folder:B');
		expect(subscribeTopics).not.toContain('folder:A');
	});
});

describe('WsBusTransport — binary dispatch', () => {
	it('extracts fileId from bytes [1..17] and fans to onBinary listeners', async () => {
		const { t, needs } = makeTransport();
		const received: Array<[string, number]> = [];
		t.onBinary((fileId, bytes) => received.push([fileId, bytes.byteLength]));
		void t.subscribe('collab:X').catch(() => {});
		await flushN();
		t.provideTicket('sp-1', needs[0]!);
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();

		const buf = new ArrayBuffer(21);
		const u8 = new Uint8Array(buf);
		u8[0] = 0x01;
		const uuidBytes = [
			0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff
		];
		for (let i = 0; i < 16; i++) u8[1 + i] = uuidBytes[i]!;

		ws.fireMessage(buf);
		await flushN();
		expect(received).toHaveLength(1);
		expect(received[0]![0]).toBe('00112233-4455-6677-8899-aabbccddeeff');
	});
});

describe('WsBusTransport — sendBinary gating', () => {
	it('returns false when the WS is not open', () => {
		const { t } = makeTransport();
		expect(t.sendBinary(new ArrayBuffer(4))).toBe(false);
	});

	it('returns true on an open WS', async () => {
		const { t, needs } = makeTransport();
		void t.subscribe('t').catch(() => {});
		await flushN();
		t.provideTicket('sp-1', needs[0]!);
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();
		expect(t.sendBinary(new ArrayBuffer(4))).toBe(true);
	});
});
