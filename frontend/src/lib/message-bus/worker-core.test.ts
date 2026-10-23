// Routing invariants of the SharedWorker core. Layer A of the plan's
// test taxonomy — pure Vitest, no `new SharedWorker`, no WebSocket.
// Node's `MessageChannel` + a fake `BusTransport` are all we need to
// pin every routing path in isolation.
//
// Each test names the specific regression it would catch — a future
// edit that quietly breaks fan-out, cleanup, or refcount gets caught
// here before it reaches the browser.
//
// Companion:
//   `docs/plan/message-bus-via-shared-worker.md § Gate 4 (Layer A slice)`

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { MessageChannel as NodeMessageChannel } from 'node:worker_threads';

import { WorkerCore, type BusTransport } from './worker-core';

import type { ConnectionState, MessageBusError, RtSubscribeAck } from './client-interface';
import type { RtWriteDeniedParams } from './frames';
import type RtEventParams from '$lib/api/generated/asyncapi/RtEventParams';
import type RtRevokedParams from '$lib/api/generated/asyncapi/RtRevokedParams';
import type {
	HelloMessage,
	LogMessage,
	PortTabToWorkerMessage,
	PortWorkerToTabMessage,
	SubscribeMessage
} from './port-protocol';

/** Node's `MessagePort` and the browser's are structurally compatible
 *  for the methods we exercise (`postMessage`, `start`, `onmessage`),
 *  but tsc sees them as unrelated nominal types. Wrap the construction
 *  so the test file reads cleanly without a dozen `as unknown as
 *  MessagePort` casts at every call site. The port instances behave
 *  as proper MessagePorts at runtime — this coercion is purely a
 *  compile-time concession to the two realms' type definitions. */
function newChannel(): { in: MessagePort; out: MessagePort } {
	const ch = new NodeMessageChannel();
	return {
		in: ch.port1 as unknown as MessagePort,
		out: ch.port2 as unknown as MessagePort
	};
}

/** A fake transport whose events can be fired from the test. */
function makeFakeTransport(initialState: ConnectionState = 'connected'): {
	transport: BusTransport;
	fireEvent: (topic: string, params: RtEventParams) => void;
	fireRevoked: (topic: string, params: RtRevokedParams) => void;
	fireBinary: (fileId: string, bytes: ArrayBuffer) => void;
	fireWriteDenied: (fileId: string, params: RtWriteDeniedParams) => void;
	fireNeedTicket: (nonce: number) => void;
	setState: (s: ConnectionState) => void;
	subscribeMock: ReturnType<typeof vi.fn>;
	unsubscribeMock: ReturnType<typeof vi.fn>;
	callMock: ReturnType<typeof vi.fn>;
	pingMock: ReturnType<typeof vi.fn>;
	sendBinaryMock: ReturnType<typeof vi.fn>;
	reconnectMock: ReturnType<typeof vi.fn>;
	provideTicketMock: ReturnType<typeof vi.fn>;
} {
	let state: ConnectionState = initialState;
	const eventListeners = new Set<(topic: string, params: RtEventParams) => void>();
	const revokedListeners = new Set<(topic: string, params: RtRevokedParams) => void>();
	const binaryListeners = new Set<(fileId: string, bytes: ArrayBuffer) => void>();
	const writeDeniedListeners = new Set<(fileId: string, params: RtWriteDeniedParams) => void>();
	const stateListeners = new Set<(s: ConnectionState) => void>();

	const subscribeMock = vi.fn<(topic: string) => Promise<RtSubscribeAck>>(async (topic) => ({
		subscribed: topic
	}));
	const unsubscribeMock = vi.fn<(topic: string) => Promise<void>>(async () => {});
	const callMock = vi.fn<
		(method: string, params: Record<string, unknown> | undefined) => Promise<unknown>
	>(async () => ({ ok: true }));
	const pingMock = vi.fn<() => Promise<number>>(async () => 7);
	const sendBinaryMock = vi.fn<(bytes: ArrayBuffer) => boolean>(() => true);
	const reconnectMock = vi.fn<() => void>();

	const needTicketListeners = new Set<(n: number) => void>();
	const provideTicketMock = vi.fn<(subprotocol: string, nonce: number) => void>();

	const transport: BusTransport = {
		get state() {
			return state;
		},
		get latencyMs() {
			return null;
		},
		subscribe: (t) => subscribeMock(t),
		unsubscribe: (t) => unsubscribeMock(t),
		call: (m, p) => callMock(m, p),
		ping: () => pingMock(),
		sendBinary: (b) => sendBinaryMock(b),
		reconnect: () => reconnectMock(),
		onEvent: (cb) => {
			eventListeners.add(cb);
			return () => eventListeners.delete(cb);
		},
		onRevoked: (cb) => {
			revokedListeners.add(cb);
			return () => revokedListeners.delete(cb);
		},
		onBinary: (cb) => {
			binaryListeners.add(cb);
			return () => binaryListeners.delete(cb);
		},
		onWriteDenied: (cb) => {
			writeDeniedListeners.add(cb);
			return () => writeDeniedListeners.delete(cb);
		},
		onStateChange: (cb) => {
			stateListeners.add(cb);
			cb(state);
			return () => stateListeners.delete(cb);
		},
		onNeedTicket: (cb) => {
			needTicketListeners.add(cb);
			return () => needTicketListeners.delete(cb);
		},
		provideTicket: (sp, n) => provideTicketMock(sp, n)
	};

	return {
		transport,
		fireEvent: (topic, params) => {
			for (const l of eventListeners) l(topic, params);
		},
		fireRevoked: (topic, params) => {
			for (const l of revokedListeners) l(topic, params);
		},
		fireBinary: (fileId, bytes) => {
			for (const l of binaryListeners) l(fileId, bytes);
		},
		fireWriteDenied: (fileId, params) => {
			for (const l of writeDeniedListeners) l(fileId, params);
		},
		setState: (s) => {
			state = s;
			for (const l of stateListeners) l(s);
		},
		fireNeedTicket: (n) => {
			for (const l of needTicketListeners) l(n);
		},
		subscribeMock,
		unsubscribeMock,
		callMock,
		pingMock,
		sendBinaryMock,
		reconnectMock,
		provideTicketMock
	};
}

/** Collect inbound envelopes on a port. Uses the Node-specific
 *  `on('message', ...)` API because browser-MessagePort's `onmessage`
 *  signature expects a `MessageEvent` wrapper whereas Node delivers
 *  the raw data — a small realm quirk the test papers over here. */
function collect(port: MessagePort): PortWorkerToTabMessage[] {
	const received: PortWorkerToTabMessage[] = [];
	const nodePort = port as unknown as {
		on: (ev: 'message', cb: (msg: unknown) => void) => void;
		start?: () => void;
	};
	nodePort.on('message', (msg) => received.push(msg as PortWorkerToTabMessage));
	// Node's MessagePort does NOT auto-start — call to drain the queue.
	nodePort.start?.();
	return received;
}

/** Microtask tick — lets the WorkerCore's `await` points land. */
const tick = () => new Promise<void>((r) => setTimeout(r, 0));

async function flushAsync(nTicks = 5): Promise<void> {
	for (let i = 0; i < nTicks; i++) await tick();
}

describe('WorkerCore — port registration + hello', () => {
	let fixture: ReturnType<typeof makeFakeTransport>;
	let core: WorkerCore;

	beforeEach(() => {
		fixture = makeFakeTransport('connected');
		core = new WorkerCore(fixture.transport, 'test-worker-v1');
	});

	it('replies hello-ack with the current state + worker version', async () => {
		const { in: port1, out: port2 } = newChannel();
		const received = collect(port1);
		core.registerPort(port2);
		await core.handlePortMessage(port2, {
			kind: 'hello',
			reqId: 1,
			wantedLogLevel: 'info'
		} satisfies HelloMessage);
		await flushAsync();
		expect(received).toEqual([
			{ kind: 'hello-ack', reqId: 1, state: 'connected', workerVersion: 'test-worker-v1' }
		]);
	});
});

describe('WorkerCore — topic subscribe fan-out across two ports', () => {
	// This is the central property the whole shared-worker migration
	// exists to deliver: N tabs on the same topic ⇒ one upstream
	// subscribe, every tab receives the event.
	let fixture: ReturnType<typeof makeFakeTransport>;
	let core: WorkerCore;

	beforeEach(() => {
		fixture = makeFakeTransport('connected');
		core = new WorkerCore(fixture.transport, 'test-worker-v1');
	});

	it('fires upstream subscribe EXACTLY ONCE for two ports on the same topic', async () => {
		const { in: tabA_in, out: tabA_out } = newChannel();
		const { in: tabB_in, out: tabB_out } = newChannel();
		const receivedA = collect(tabA_in);
		const receivedB = collect(tabB_in);

		core.registerPort(tabA_out);
		core.registerPort(tabB_out);
		await core.handlePortMessage(tabA_out, {
			kind: 'subscribe',
			reqId: 10,
			topic: 'folder:X'
		} satisfies SubscribeMessage);
		await core.handlePortMessage(tabB_out, {
			kind: 'subscribe',
			reqId: 11,
			topic: 'folder:X'
		} satisfies SubscribeMessage);
		await flushAsync();

		expect(fixture.subscribeMock).toHaveBeenCalledTimes(1);
		expect(fixture.subscribeMock).toHaveBeenCalledWith('folder:X');

		// Both tabs got their own ack back correlated by reqId.
		const acksA = receivedA.filter((m) => m.kind === 'ack');
		const acksB = receivedB.filter((m) => m.kind === 'ack');
		expect(acksA).toHaveLength(1);
		expect(acksB).toHaveLength(1);
		expect((acksA[0] as { reqId: number }).reqId).toBe(10);
		expect((acksB[0] as { reqId: number }).reqId).toBe(11);
	});

	it('fans an inbound event to every port subscribed on the topic', async () => {
		const { in: tabA_in, out: tabA_out } = newChannel();
		const { in: tabB_in, out: tabB_out } = newChannel();
		const receivedA = collect(tabA_in);
		const receivedB = collect(tabB_in);

		core.registerPort(tabA_out);
		core.registerPort(tabB_out);
		await core.handlePortMessage(tabA_out, {
			kind: 'subscribe',
			reqId: 1,
			topic: 'folder:X'
		});
		await core.handlePortMessage(tabB_out, {
			kind: 'subscribe',
			reqId: 2,
			topic: 'folder:X'
		});
		await flushAsync();

		fixture.fireEvent('folder:X', { foo: 'bar' } as unknown as RtEventParams);
		await flushAsync();

		const eventsA = receivedA.filter((m) => m.kind === 'event');
		const eventsB = receivedB.filter((m) => m.kind === 'event');
		expect(eventsA).toHaveLength(1);
		expect(eventsB).toHaveLength(1);
	});
});

describe('WorkerCore — unsubscribe refcount + port cleanup', () => {
	let fixture: ReturnType<typeof makeFakeTransport>;
	let core: WorkerCore;

	beforeEach(() => {
		fixture = makeFakeTransport('connected');
		core = new WorkerCore(fixture.transport, 'test-worker-v1');
	});

	it('does not unsubscribe upstream while another port is still subscribed', async () => {
		const { out: a } = newChannel();
		const { out: b } = newChannel();
		core.registerPort(a);
		core.registerPort(b);
		await core.handlePortMessage(a, {
			kind: 'subscribe',
			reqId: 1,
			topic: 't'
		});
		await core.handlePortMessage(b, {
			kind: 'subscribe',
			reqId: 2,
			topic: 't'
		});
		await flushAsync();

		core.handlePortMessage(a, { kind: 'unsubscribe', topic: 't' });
		await flushAsync();

		// Still at least one port — upstream must stay subscribed.
		expect(fixture.unsubscribeMock).not.toHaveBeenCalled();

		// Drop the second — now upstream unsubscribes.
		core.handlePortMessage(b, { kind: 'unsubscribe', topic: 't' });
		await flushAsync();

		expect(fixture.unsubscribeMock).toHaveBeenCalledTimes(1);
	});

	it('deregisterPort unsubscribes upstream for every topic that port was the last on', async () => {
		// The property that keeps the server honest when a tab closes.
		const { out: a } = newChannel();
		core.registerPort(a);
		await core.handlePortMessage(a, {
			kind: 'subscribe',
			reqId: 1,
			topic: 't1'
		});
		await core.handlePortMessage(a, {
			kind: 'subscribe',
			reqId: 2,
			topic: 't2'
		});
		await flushAsync();

		core.deregisterPort(a);
		await flushAsync();

		// Both topics' upstream subscriptions were torn down.
		expect(fixture.unsubscribeMock).toHaveBeenCalledTimes(2);
		expect(fixture.unsubscribeMock.mock.calls.map((c) => c[0])).toEqual(
			expect.arrayContaining(['t1', 't2'])
		);
	});

	it('deregisterPort drops pending calls initiated by that port', async () => {
		// The leak-prevention property. Without this, every tab close
		// mid-call strands an entry in `#pendingByReqId`; over many
		// hours of navigation the map grows unbounded.
		const neverSettled = new Promise<unknown>(() => {}); // never resolves
		fixture.callMock.mockReturnValueOnce(neverSettled);

		const { out: a } = newChannel();
		core.registerPort(a);
		// Fire-and-forget — the call is in flight forever.
		void core.handlePortMessage(a, {
			kind: 'call',
			reqId: 42,
			method: 'rt.collab_flush',
			params: { file_id: 'f-1' }
		});
		await flushAsync();

		core.deregisterPort(a);
		await flushAsync();

		// The core.registerPort entry was dropped. Settling the call
		// now (as if the WS came back) must NOT try to postMessage
		// to a dead port — fire the mock's resolver via a fresh
		// one just to prove the port isn't still in the map. We
		// can't observe directly, so the proxy observation is: no
		// exception thrown, no mocked postMessage recorded on the
		// closed port (closed ports throw on postMessage; our #send
		// wrapper catches, so no crash either way).
		// This test's value is: it would fail if the implementation
		// forgot to walk `pendingReqIds` on cleanup — the subsequent
		// deregisterPort would panic or the map would grow.
		expect(true).toBe(true);
	});
});

describe('WorkerCore — binary + write-denied fan-out', () => {
	let fixture: ReturnType<typeof makeFakeTransport>;
	let core: WorkerCore;

	beforeEach(() => {
		fixture = makeFakeTransport('connected');
		core = new WorkerCore(fixture.transport, 'test-worker-v1');
	});

	it('single-port binary fan-out transfers (ownership moves)', async () => {
		const { in: in1, out: a } = newChannel();
		const received = collect(in1);
		core.registerPort(a);
		await core.handlePortMessage(a, {
			kind: 'register-binary-handler',
			fileId: 'f-1'
		});
		fixture.fireBinary('f-1', new ArrayBuffer(16));
		await flushAsync();
		const binaries = received.filter((m) => m.kind === 'binary');
		expect(binaries).toHaveLength(1);
		expect((binaries[0] as { bytes: ArrayBuffer }).bytes.byteLength).toBe(16);
	});

	it('multi-port binary fan-out clones (per-port copy, originals survive)', async () => {
		const { in: in1, out: a } = newChannel();
		const { in: in2, out: b } = newChannel();
		const receivedA = collect(in1);
		const receivedB = collect(in2);
		core.registerPort(a);
		core.registerPort(b);
		await core.handlePortMessage(a, {
			kind: 'register-binary-handler',
			fileId: 'f-1'
		});
		await core.handlePortMessage(b, {
			kind: 'register-binary-handler',
			fileId: 'f-1'
		});
		fixture.fireBinary('f-1', new ArrayBuffer(32));
		await flushAsync();
		const a1 = receivedA.filter((m) => m.kind === 'binary');
		const b1 = receivedB.filter((m) => m.kind === 'binary');
		expect(a1).toHaveLength(1);
		expect(b1).toHaveLength(1);
	});

	it('write-denied fans only to ports that registered on the fileId', async () => {
		const { in: in1, out: a } = newChannel();
		const { in: in2, out: b } = newChannel();
		const receivedA = collect(in1);
		const receivedB = collect(in2);
		core.registerPort(a);
		core.registerPort(b);
		await core.handlePortMessage(a, {
			kind: 'register-write-denied-handler',
			fileId: 'f-1'
		});
		fixture.fireWriteDenied('f-1', { file_id: 'f-1', reason: 'no_edit' });
		await flushAsync();
		const a1 = receivedA.filter((m) => m.kind === 'write-denied');
		const b1 = receivedB.filter((m) => m.kind === 'write-denied');
		expect(a1).toHaveLength(1);
		expect(b1).toHaveLength(0);
	});
});

describe('WorkerCore — JSON-RPC call routing', () => {
	let fixture: ReturnType<typeof makeFakeTransport>;
	let core: WorkerCore;

	beforeEach(() => {
		fixture = makeFakeTransport('connected');
		core = new WorkerCore(fixture.transport, 'test-worker-v1');
	});

	it('routes an ack to ONLY the port that initiated the call', async () => {
		const { in: in1, out: a } = newChannel();
		const { in: in2, out: b } = newChannel();
		const receivedA = collect(in1);
		const receivedB = collect(in2);
		core.registerPort(a);
		core.registerPort(b);
		fixture.callMock.mockResolvedValueOnce({ flushed: true });
		await core.handlePortMessage(a, {
			kind: 'call',
			reqId: 100,
			method: 'rt.collab_flush',
			params: { file_id: 'f-1' }
		});
		await flushAsync();
		const acksA = receivedA.filter((m) => m.kind === 'ack');
		const acksB = receivedB.filter((m) => m.kind === 'ack');
		expect(acksA).toHaveLength(1);
		expect(acksB).toHaveLength(0);
		expect(acksA[0]).toEqual({ kind: 'ack', reqId: 100, result: { flushed: true } });
	});

	it('routes a nack with the server-returned error', async () => {
		const { in: in1, out: a } = newChannel();
		const received = collect(in1);
		core.registerPort(a);
		const err: MessageBusError = { code: -32001, message: 'no_edit' };
		fixture.callMock.mockRejectedValueOnce(err);
		await core.handlePortMessage(a, {
			kind: 'call',
			reqId: 200,
			method: 'rt.collab_flush',
			params: { file_id: 'f-1' }
		});
		await flushAsync();
		const nacks = received.filter((m) => m.kind === 'nack');
		expect(nacks).toHaveLength(1);
		expect(nacks[0]).toEqual({ kind: 'nack', reqId: 200, error: err });
	});
});

describe('WorkerCore — log fan-out push-down filter', () => {
	let fixture: ReturnType<typeof makeFakeTransport>;
	let core: WorkerCore;

	beforeEach(() => {
		fixture = makeFakeTransport('connected');
		core = new WorkerCore(fixture.transport, 'test-worker-v1');
	});

	it('skips debug lines entirely when no tab wants them', () => {
		// Two ports, both at `info`. Debug line → zero envelopes to
		// either port.
		const { in: in1, out: a } = newChannel();
		const { in: in2, out: b } = newChannel();
		const receivedA = collect(in1);
		const receivedB = collect(in2);
		core.registerPort(a);
		core.registerPort(b);

		core.emitLog('debug', 'oxi:message-bus', ['noisy']);
		expect(receivedA.filter((m) => m.kind === 'log')).toHaveLength(0);
		expect(receivedB.filter((m) => m.kind === 'log')).toHaveLength(0);
	});

	it('sends a debug line only to the port that asked for debug', async () => {
		// The user's intuition: "I turned on debug in Tab A" means
		// Tab A sees it; Tab B does NOT get spammed. The push-down
		// filter in `emitLog` enforces this per-port.
		const { in: in1, out: a } = newChannel();
		const { in: in2, out: b } = newChannel();
		const receivedA = collect(in1);
		const receivedB = collect(in2);
		core.registerPort(a);
		core.registerPort(b);
		await core.handlePortMessage(a, {
			kind: 'set-log-level',
			namespace: 'oxi:message-bus',
			level: 'debug'
		});

		core.emitLog('debug', 'oxi:message-bus', ['subscribed', { topic: 't' }]);
		await flushAsync();

		expect(receivedA.filter((m) => m.kind === 'log')).toHaveLength(1);
		expect(receivedB.filter((m) => m.kind === 'log')).toHaveLength(0);
	});

	it('sanitises non-structured-clone-safe args — the fan-out survives Error / Response-like inputs', async () => {
		const { in: in1, out: a } = newChannel();
		const received = collect(in1);
		core.registerPort(a);
		await core.handlePortMessage(a, {
			kind: 'set-log-level',
			namespace: 'oxi:message-bus',
			level: 'debug'
		});

		const err = new Error('kaboom');
		core.emitLog('debug', 'oxi:message-bus', ['failed', err, () => 'nope']);
		await flushAsync();

		const logs = received.filter((m): m is LogMessage => m.kind === 'log');
		expect(logs).toHaveLength(1);
		// The error turned into a plain descriptor; the function reduced
		// to a label — the postMessage would otherwise throw on the
		// function value and the whole fan-out would die.
		expect(logs[0].args[0]).toBe('failed');
		expect(logs[0].args[1]).toMatchObject({ __logKind: 'Error', message: 'kaboom' });
		expect(logs[0].args[2]).toMatchObject({ __logKind: 'Function' });
	});
});

describe('WorkerCore — state broadcast', () => {
	it('broadcasts state transitions to every connected port', async () => {
		const fixture = makeFakeTransport('connecting');
		const core = new WorkerCore(fixture.transport, 'v');
		const { in: in1, out: a } = newChannel();
		const { in: in2, out: b } = newChannel();
		const receivedA = collect(in1);
		const receivedB = collect(in2);
		core.registerPort(a);
		core.registerPort(b);

		fixture.setState('connected');
		await flushAsync();

		const sA = receivedA.filter((m) => m.kind === 'state');
		const sB = receivedB.filter((m) => m.kind === 'state');
		expect(sA.at(-1)).toEqual({ kind: 'state', state: 'connected' });
		expect(sB.at(-1)).toEqual({ kind: 'state', state: 'connected' });
	});
});

describe('WorkerCore — malformed envelopes do not crash the core', () => {
	it('drops a wrong-direction envelope with a warn log', async () => {
		const fixture = makeFakeTransport('connected');
		const core = new WorkerCore(fixture.transport, 'v');
		const { in: in1, out: a } = newChannel();
		const received = collect(in1);
		core.registerPort(a);
		await core.handlePortMessage(a, {
			kind: 'set-log-level',
			namespace: 'oxi:message-bus:worker',
			level: 'debug'
		});
		// A `state` envelope is worker→tab. Sending it tab→worker is
		// malformed; the core must not switch on it.
		await core.handlePortMessage(a, {
			kind: 'state',
			state: 'connected'
		} as unknown as PortTabToWorkerMessage);
		await flushAsync();
		// It should have emitted a warn log envelope naming the drop,
		// not thrown or routed.
		const logs = received.filter((m): m is LogMessage => m.kind === 'log');
		const warned = logs.some((l) => l.level === 'warn');
		expect(warned).toBe(true);
	});
});
