// Port-protocol envelope coverage — Gate 1 of the SharedWorker
// migration (`docs/plan/message-bus-via-shared-worker.md`).
//
// These are the Layer A tests the plan calls out: pure Vitest, no
// `MessagePort` or browser, pinning the envelope union + runtime
// typeguard shape. They light up now because the port-protocol module
// exists after Gate 1, even though the SharedWorker that will route
// envelopes against these shapes doesn't land until Gate 2.
//
// Each test pins one wire-shape invariant a careless edit to the
// discriminated union or the typeguard could silently collapse:
// adding a `kind` to the union without updating the typeguard
// forwards unparsed garbage; dropping a required field from the
// typeguard lets a malformed envelope through; adding a case
// without the matching kinds-set entry silently rejects every
// envelope of that kind.

import { describe, it, expect } from 'vitest';

import {
	parsePortMessage,
	type AckMessage,
	type BinaryMessage,
	type CallMessage,
	type EventMessage,
	type HelloAckMessage,
	type HelloMessage,
	type LogMessage,
	type NackMessage,
	type PingMessage,
	type PingPongMessage,
	type PortMessage,
	type RevokedMessage,
	type SendBinaryMessage,
	type SetLogLevelMessage,
	type StateMessage,
	type SubscribeMessage,
	type UnsubscribeMessage,
	type WriteDeniedMessage
} from './port-protocol';

describe('parsePortMessage — roundtrip for every envelope kind', () => {
	// One well-formed envelope per `kind`. If the typeguard refuses any
	// of these, something regressed in the per-kind field checks.
	const samples: PortMessage[] = [
		// Tab → Worker
		{ kind: 'hello', reqId: 1, wantedLogLevel: 'info' } satisfies HelloMessage,
		{ kind: 'subscribe', reqId: 2, topic: 'folder:abc' } satisfies SubscribeMessage,
		{ kind: 'unsubscribe', topic: 'folder:abc' } satisfies UnsubscribeMessage,
		{ kind: 'register-binary-handler', fileId: 'f-1' },
		{ kind: 'unregister-binary-handler', fileId: 'f-1' },
		{ kind: 'register-write-denied-handler', fileId: 'f-1' },
		{ kind: 'unregister-write-denied-handler', fileId: 'f-1' },
		{ kind: 'send-binary', bytes: new ArrayBuffer(4) } satisfies SendBinaryMessage,
		{ kind: 'call', reqId: 3, method: 'rt.collab_flush' } satisfies CallMessage,
		{
			kind: 'call',
			reqId: 4,
			method: 'rt.collab_flush',
			params: { file_id: 'f-1' }
		} satisfies CallMessage,
		{ kind: 'ping', reqId: 5 } satisfies PingMessage,
		{
			kind: 'set-log-level',
			namespace: 'oxi:message-bus',
			level: 'debug'
		} satisfies SetLogLevelMessage,
		// Worker → Tab
		{
			kind: 'hello-ack',
			reqId: 1,
			state: 'connected',
			workerVersion: 'v1'
		} satisfies HelloAckMessage,
		{ kind: 'state', state: 'connecting' } satisfies StateMessage,
		{ kind: 'ack', reqId: 3, result: { ok: true } } satisfies AckMessage,
		{
			kind: 'nack',
			reqId: 3,
			error: { code: -32000, message: 'ws_closed' }
		} satisfies NackMessage,
		{
			kind: 'event',
			topic: 'folder:abc',
			params: { topic: 'folder:abc', payload: {} } as unknown as EventMessage['params']
		} satisfies EventMessage,
		{
			kind: 'revoked',
			topic: 'folder:abc',
			params: { topic: 'folder:abc' } as unknown as RevokedMessage['params']
		} satisfies RevokedMessage,
		{ kind: 'binary', fileId: 'f-1', bytes: new ArrayBuffer(2) } satisfies BinaryMessage,
		{
			kind: 'write-denied',
			fileId: 'f-1',
			params: { file_id: 'f-1', reason: 'no_edit' } satisfies WriteDeniedMessage['params']
		} satisfies WriteDeniedMessage,
		{ kind: 'ping-pong', reqId: 5, latencyMs: 17 } satisfies PingPongMessage,
		{
			kind: 'log',
			level: 'debug',
			namespace: 'oxi:message-bus',
			args: ['subscribed', { topic: 'folder:abc' }],
			ts: 1_700_000_000_000
		} satisfies LogMessage
	];

	for (const sample of samples) {
		it(`accepts a well-formed ${sample.kind} envelope`, () => {
			// Pass the sample as-is: the typeguard tests the SHAPE of
			// what arrives on `postMessage`, not the clone semantics.
			// `structuredClone` was tried here but trips on jsdom's
			// `ArrayBuffer` realm boundary for `send-binary` / `binary`
			// envelopes — the cloned buffer isn't the test realm's
			// ArrayBuffer, so `instanceof ArrayBuffer` reads false
			// even though the real worker-to-tab path produces a true
			// ArrayBuffer. The realm-boundary coverage lives in Gate
			// 2's end-to-end worker test; here we just pin the shape
			// gate.
			const parsed = parsePortMessage(sample);
			expect(parsed).not.toBeNull();
			expect(parsed!.kind).toBe(sample.kind);
		});
	}

	it('rejects an envelope with a ConnectionState value outside the union', () => {
		// A `state` outside the union would otherwise be surfaced into
		// the bridge's `state` rune and paint an unrepresentable badge.
		expect(parsePortMessage({ kind: 'state', state: 'reconnecting' })).toBeNull();
	});
});

describe('parsePortMessage — negative cases that would silently corrupt routing', () => {
	it('rejects non-object input (string / number / null / undefined / array)', () => {
		expect(parsePortMessage('hello')).toBeNull();
		expect(parsePortMessage(42)).toBeNull();
		expect(parsePortMessage(null)).toBeNull();
		expect(parsePortMessage(undefined)).toBeNull();
		expect(parsePortMessage([])).toBeNull();
	});

	it('rejects an unknown `kind` — covers typos AND future-version envelopes', () => {
		// A rogue message with a plausible-looking kind. Rejecting is the
		// forward-compatible behaviour: a tab speaking v2 to a worker
		// stuck on v1 should surface as "unknown envelope dropped" with
		// a visible log, never as a silent misroute into another case.
		expect(parsePortMessage({ kind: 'subscribe-v2', topic: 'folder:abc' })).toBeNull();
	});

	it('rejects a `subscribe` missing `topic` — a required field', () => {
		// Would otherwise land in the fan-out map with `undefined` as a
		// key, forwarding every subsequent event to no one AND
		// occupying a routing slot nothing can clean up.
		expect(parsePortMessage({ kind: 'subscribe', reqId: 1 })).toBeNull();
	});

	it('rejects a `call` whose `params` is a number — must be object or absent', () => {
		expect(
			parsePortMessage({ kind: 'call', reqId: 1, method: 'rt.collab_flush', params: 42 })
		).toBeNull();
	});

	it('accepts a `call` with `params` omitted entirely', () => {
		// Belt + braces: the previous negative test uses `params: 42` as
		// the failing case; this one proves the absence path still works.
		expect(parsePortMessage({ kind: 'call', reqId: 1, method: 'rt.collab_flush' })).not.toBeNull();
	});

	it('rejects a `nack` whose `error` is missing numeric `code`', () => {
		// The bridge dispatches on `error.code` (RtErrorCode union).
		// A string `code` would land in the switch default and leak the
		// pending-call reqId silently — mandate a number here.
		expect(
			parsePortMessage({ kind: 'nack', reqId: 1, error: { code: 'oops', message: 'x' } })
		).toBeNull();
	});

	it('rejects `binary` without `bytes` being an ArrayBuffer', () => {
		// `postMessage` auto-structured-clones a Uint8Array into the
		// underlying buffer, but the typeguard enforces the explicit
		// ArrayBuffer so transferable fast-path assumptions downstream
		// hold.
		expect(parsePortMessage({ kind: 'binary', fileId: 'f-1', bytes: 'not-bytes' })).toBeNull();
	});

	it('rejects a `log` envelope whose level is outside the LogLevel set', () => {
		expect(
			parsePortMessage({
				kind: 'log',
				level: 'FATAL',
				namespace: 'oxi:message-bus',
				args: [],
				ts: 0
			})
		).toBeNull();
	});

	it('rejects a `log` envelope whose args is not an Array', () => {
		expect(
			parsePortMessage({
				kind: 'log',
				level: 'debug',
				namespace: 'oxi:message-bus',
				args: 'not-an-array',
				ts: 0
			})
		).toBeNull();
	});

	it('rejects a `hello` whose wantedLogLevel is bogus', () => {
		expect(parsePortMessage({ kind: 'hello', reqId: 1, wantedLogLevel: 'HOT' })).toBeNull();
	});

	it('rejects a `hello-ack` missing workerVersion', () => {
		// The script-update story in the plan hinges on this field.
		// Dropping it would make a stale-worker-vs-fresh-page mismatch
		// silently indistinguishable from a fresh handshake.
		expect(parsePortMessage({ kind: 'hello-ack', reqId: 1, state: 'connected' })).toBeNull();
	});
});
