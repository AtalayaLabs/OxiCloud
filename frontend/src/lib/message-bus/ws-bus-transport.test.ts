// `WsBusTransport` coverage — the WS wrapper + ticket exchange + the
// reconnect state machine the SharedWorker depends on.
//
// Pure Vitest. Fake WebSocket + fake fetch; no `new SharedWorker`,
// no browser, no server. Each test pins the specific invariant the
// matching per-tab client property pins (ticket-first upgrade,
// circuit-breaker on 403, stable-timer reset, etc.) so a future edit
// that drifts the two implementations apart gets caught here.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';

import { WsBusTransport, type WsBusTransportOptions } from './ws-bus-transport';
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

	// Test harness helpers (not spec)
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

function ticketResponse(body: Record<string, unknown>, status = 200) {
	return Promise.resolve({
		ok: status >= 200 && status < 300,
		status,
		json: () => Promise.resolve(body)
	} as Response);
}

function makeTransport(overrides?: Partial<WsBusTransportOptions>): {
	t: WsBusTransport;
	fetchMock: ReturnType<typeof vi.fn>;
	states: ConnectionState[];
} {
	const fetchMock = vi.fn(() =>
		ticketResponse({ ticket: 'uuid-1', expires_in_seconds: 30, subprotocol: 'oxi.ticket.uuid-1' })
	);
	const t = new WsBusTransport({
		wsUrl: 'ws://test/rt',
		ticketUrl: 'http://test/api/rt/ticket',
		csrfToken: 'csrf-abc',
		WebSocketCtor: FakeSocket as unknown as typeof WebSocket,
		fetchFn: fetchMock as unknown as typeof fetch,
		...overrides
	});
	const states: ConnectionState[] = [];
	t.onStateChange((s) => states.push(s));
	return { t, fetchMock, states };
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
});

describe('WsBusTransport — ticket-first upgrade', () => {
	it('subscribe triggers ticket POST + WS upgrade carrying the subprotocol', async () => {
		const { t, fetchMock } = makeTransport();
		const sub = t.subscribe('folder:X');
		await flushN();

		// Ticket POST landed with the CSRF header + credentials.
		expect(fetchMock).toHaveBeenCalledTimes(1);
		const [url, init] = fetchMock.mock.calls[0];
		expect(url).toBe('http://test/api/rt/ticket');
		expect((init as RequestInit).method).toBe('POST');
		expect((init as RequestInit).credentials).toBe('include');
		expect((init as RequestInit).headers).toMatchObject({ 'X-CSRF-Token': 'csrf-abc' });

		// WS constructor received the subprotocol from the ticket response.
		expect(FakeSocket.instances).toHaveLength(1);
		const ws = FakeSocket.instances[0]!;
		expect(ws.url).toBe('ws://test/rt');
		expect(ws.protocols).toEqual(['oxi.ticket.uuid-1']);

		// Open the socket + reply with the subscribe ack.
		ws.fireOpen();
		await flushN();
		// Grab the subscribe request id from what was sent.
		const sentFrame = JSON.parse(ws.sent[0] as string) as {
			id: number;
			method: string;
			params: { topic: string };
		};
		expect(sentFrame.method).toBe('rt.subscribe');
		expect(sentFrame.params.topic).toBe('folder:X');

		// Server responds with success.
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

	it('does NOT send X-CSRF-Token when csrfToken is empty', async () => {
		const { fetchMock } = makeTransport({ csrfToken: '' });
		const t = new WsBusTransport({
			wsUrl: 'ws://test/rt',
			ticketUrl: 'http://test/api/rt/ticket',
			csrfToken: '',
			WebSocketCtor: FakeSocket as unknown as typeof WebSocket,
			fetchFn: fetchMock as unknown as typeof fetch
		});
		void t.subscribe('t');
		await flushN();
		const init = fetchMock.mock.calls[0]![1] as RequestInit;
		expect(init.headers).not.toHaveProperty('X-CSRF-Token');
	});
});

describe('WsBusTransport — circuit breaker on 403', () => {
	it('flips to unavailable on a 403 ticket response + does not retry', async () => {
		const fetchMock = vi.fn(() => ticketResponse({ error: 'forbidden' }, 403));
		const t = new WsBusTransport({
			wsUrl: 'ws://test/rt',
			ticketUrl: 'http://test/api/rt/ticket',
			csrfToken: 'c',
			WebSocketCtor: FakeSocket as unknown as typeof WebSocket,
			fetchFn: fetchMock as unknown as typeof fetch
		});
		void t.subscribe('t').catch(() => {}); // we don't await this; subscribe awaits connect
		await flushN();
		// Need to advance timers because `#whenConnected` is awaited;
		// but the state should be `unavailable` by now from the ticket
		// failure path.
		expect(t.state).toBe('unavailable');
		// Advance the clock past any possible backoff — no additional
		// fetches should fire.
		vi.advanceTimersByTime(60_000);
		await flushN();
		expect(fetchMock).toHaveBeenCalledTimes(1);
		expect(FakeSocket.instances).toHaveLength(0);
	});
});

describe('WsBusTransport — WS lifecycle', () => {
	it('onOpen replays active topics', async () => {
		const { t } = makeTransport();
		void t.subscribe('folder:A').catch(() => {});
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();
		// First subscribe sent — then if we drop and reconnect, the
		// replay sends the subscribe again on the next WS.
		ws.fireClose(1006, 'abnormal'); // drop
		// Advance into the backoff window.
		vi.advanceTimersByTime(1_000);
		await flushN();
		// A new WS was constructed for the reconnect.
		expect(FakeSocket.instances.length).toBeGreaterThan(1);
		const ws2 = FakeSocket.instances.at(-1)!;
		ws2.fireOpen();
		await flushN();
		// The replay sent subscribe for `folder:A`.
		const subscribeFrames = ws2.sent
			.filter((s): s is string => typeof s === 'string')
			.map((s) => JSON.parse(s) as { method: string; params?: { topic?: string } })
			.filter((f) => f.method === 'rt.subscribe');
		expect(subscribeFrames.length).toBeGreaterThanOrEqual(1);
		expect(subscribeFrames[0].params?.topic).toBe('folder:A');
	});

	it('rejects pending calls on close', async () => {
		const { t } = makeTransport();
		const sub = t.subscribe('t');
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();
		// Close before any ack arrives.
		ws.fireClose(1006, 'abnormal');
		await expect(sub).rejects.toMatchObject({ message: 'ws_closed' });
	});
});

describe('WsBusTransport — event + revoked dispatch', () => {
	it('fans event frames to onEvent listeners with the topic extracted from params', async () => {
		const { t } = makeTransport();
		const events: Array<[string, unknown]> = [];
		t.onEvent((topic, params) => events.push([topic, params]));
		void t.subscribe('folder:X').catch(() => {});
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

	it('fans revoked frames and drops the topic from active replay set', async () => {
		// Two active topics. Revoke one; after reconnect the replay
		// must re-subscribe to the SURVIVOR and NOT to the revoked
		// one — else the server would audit-log a denied subscribe
		// every reconnect for the duration of the session.
		const { t } = makeTransport();
		const revoked: string[] = [];
		t.onRevoked((topic) => revoked.push(topic));
		void t.subscribe('folder:A').catch(() => {});
		void t.subscribe('folder:B').catch(() => {});
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
		const { t } = makeTransport();
		const received: Array<[string, number]> = [];
		t.onBinary((fileId, bytes) => received.push([fileId, bytes.byteLength]));
		void t.subscribe('collab:X').catch(() => {});
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();

		// Build a frame: 1 byte kind + 16 bytes UUID + 4 bytes payload.
		const buf = new ArrayBuffer(21);
		const u8 = new Uint8Array(buf);
		u8[0] = 0x01; // KIND_UPDATE
		// UUID '00112233-4455-6677-8899-aabbccddeeff' as 16 bytes.
		const uuidBytes = [
			0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff
		];
		for (let i = 0; i < 16; i++) u8[1 + i] = uuidBytes[i]!;

		ws.fireMessage(buf);
		await flushN();
		expect(received).toHaveLength(1);
		expect(received[0]![0]).toBe('00112233-4455-6677-8899-aabbccddeeff');
		expect(received[0]![1]).toBe(21);
	});

	it('drops a binary frame shorter than 17 bytes', async () => {
		const { t } = makeTransport();
		const received: unknown[] = [];
		t.onBinary((_fileId, bytes) => received.push(bytes));
		void t.subscribe('t').catch(() => {});
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();

		ws.fireMessage(new ArrayBuffer(10));
		await flushN();
		expect(received).toHaveLength(0);
	});
});

describe('WsBusTransport — sendBinary gating', () => {
	it('returns false when the WS is not open', () => {
		const { t } = makeTransport();
		expect(t.sendBinary(new ArrayBuffer(4))).toBe(false);
	});

	it('returns true on a best-effort send to an open WS', async () => {
		const { t } = makeTransport();
		void t.subscribe('t').catch(() => {});
		await flushN();
		const ws = FakeSocket.instances[0]!;
		ws.fireOpen();
		await flushN();
		expect(t.sendBinary(new ArrayBuffer(4))).toBe(true);
		expect(ws.sent.filter((s) => s instanceof ArrayBuffer)).toHaveLength(1);
	});
});
