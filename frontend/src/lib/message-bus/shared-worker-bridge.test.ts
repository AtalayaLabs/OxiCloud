// Behavioral pins for `SharedWorkerBridge`.
//
// This file hosts regressions a careless edit to the bridge's
// construction path could silently reintroduce. The anchoring one
// here is the 2026-10-24 infinite-redirect-loop: an eager
// `POST /api/rt/ticket` fired at construction on an unauthenticated
// page would 401 → apiFetch's refresh interceptor → 401 → session-
// expired handler → `window.location.href = /login?source=session_expired`
// → page reload → bridge reconstructs → POST → redirect → loop.
// Observed on a fresh Chrome profile hitting a dev instance.
//
// The pin is on APIS NOT CALLED at construction, not on the apiFetch
// return value — the "fix" is "nothing happens here", so the test is
// a negative assertion.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { allowConsole } from '$lib/testing/allowConsole';

// The ticket-fetch deadline is one of the behaviours pinned here, and the
// bridge warns when it expires — see the console gate in `vitest-setup.ts`.
allowConsole('warn', 'ticket-fetch deadline expiry, which warns by design');
import { MessageChannel as NodeMessageChannel } from 'node:worker_threads';

// Hoisted mocks — the bridge's module graph pulls in `apiJson` and
// `getCsrfHeaders` via static import. apiJson's type is widened to
// `(url, init) => Promise<unknown>` so tests can override with
// a specific-shape implementation (e.g. the hanging-fetch stub in
// the AbortController test) without TypeScript narrowing fighting
// each `mockImplementationOnce`.
const { apiJsonMock, csrfMock, onSessionRefreshedMock, sessionCleared } = vi.hoisted(() => ({
	apiJsonMock: vi.fn<(url: string, init?: RequestInit) => Promise<unknown>>(async () => ({
		ticket: 'uuid-1',
		expires_in_seconds: 30,
		subprotocol: 'oxi.ticket.uuid-1'
	})),
	csrfMock: vi.fn(() => ({ 'X-CSRF-Token': 'test-csrf' })),
	// The bridge subscribes at construction so the mock must hand back
	// a no-op unsubscribe. Returning `undefined` would throw in close().
	onSessionRefreshedMock: vi.fn<(fn: () => void) => () => void>(() => () => {}),
	// Same contract for the identity-changed hook, but the callback is
	// captured so a test can play "the user logged out".
	sessionCleared: { fire: null as null | (() => void) }
}));

vi.mock('$lib/api/client', () => ({
	apiJson: apiJsonMock,
	onSessionRefreshed: onSessionRefreshedMock
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: csrfMock }));
vi.mock('$app/paths', () => ({ base: '' }));
vi.mock('$lib/auth/session-broadcast', () => ({
	onSessionCleared: (fn: () => void) => {
		sessionCleared.fire = fn;
		return () => {
			sessionCleared.fire = null;
		};
	}
}));

import { SharedWorkerBridge } from './shared-worker-bridge.svelte';

function newChannel(): { in: MessagePort; out: MessagePort } {
	const ch = new NodeMessageChannel();
	return {
		in: ch.port1 as unknown as MessagePort,
		out: ch.port2 as unknown as MessagePort
	};
}

beforeEach(() => {
	apiJsonMock.mockClear();
	csrfMock.mockClear();
});

describe('SharedWorkerBridge — regression: no eager ticket POST', () => {
	it('does NOT call apiJson at construction (prevents anonymous-page auth loop)', async () => {
		// If an eager `#fetchAndProvideTicket(0)` were ever re-added to
		// the constructor, `apiJsonMock` would fire — because we mock
		// `apiJson` to resolve, this test would see the call land here
		// rather than the real production redirect-loop. The pin is on
		// the API-call shape, which is observable without the production
		// 401 → redirect pathway.
		const { out } = newChannel();
		const bridge = new SharedWorkerBridge(out);
		// Yield so any microtask-scheduled fetch would have fired by now.
		await new Promise((r) => setTimeout(r, 10));
		expect(apiJsonMock).not.toHaveBeenCalled();
		bridge.close();
	});

	it('posts `hello` on construction (positive control)', async () => {
		// Belt + braces: the above negative pin would also pass if the
		// bridge did nothing on construction, which would be a worse
		// regression. Confirm the hello envelope DOES fly, so a future
		// edit that silently kills the whole constructor still fails
		// the suite.
		const { in: inbox, out } = newChannel();
		const received: unknown[] = [];
		const nodeInbox = inbox as unknown as {
			on: (ev: 'message', cb: (msg: unknown) => void) => void;
			start?: () => void;
		};
		nodeInbox.on('message', (m) => received.push(m));
		nodeInbox.start?.();
		const bridge = new SharedWorkerBridge(out);
		await new Promise((r) => setTimeout(r, 10));
		const hellos = received.filter(
			(m): m is { kind: string } =>
				typeof m === 'object' && m !== null && (m as { kind?: string }).kind === 'hello'
		);
		expect(hellos).toHaveLength(1);
		bridge.close();
	});
});

describe('SharedWorkerBridge — on-demand ticket flow (post-regression wiring)', () => {
	it('fetches a ticket in response to a `need-ticket` envelope, not at construction', async () => {
		// The remaining ticket pathway: worker sends `need-ticket` →
		// bridge POSTS /api/rt/ticket → sends `provide-ticket` reply.
		const { out } = newChannel();
		const bridge = new SharedWorkerBridge(out);
		await new Promise((r) => setTimeout(r, 10));
		expect(apiJsonMock).not.toHaveBeenCalled();

		// Simulate the worker's need-ticket envelope — the bridge
		// receives via its `.onmessage` which this file plumbs by
		// writing to the OTHER end of the channel.
		const { in: workerSide, out: tabSide } = newChannel();
		const bridge2 = new SharedWorkerBridge(tabSide);
		// From the worker's perspective, fire need-ticket onto the port.
		(workerSide as unknown as { postMessage: (m: unknown) => void }).postMessage({
			kind: 'need-ticket',
			nonce: 7
		});
		await new Promise((r) => setTimeout(r, 10));

		expect(apiJsonMock).toHaveBeenCalled();
		const call = apiJsonMock.mock.calls[0] as unknown as [string, RequestInit];
		expect(call[0]).toBe('/api/rt/ticket');
		expect(call[1].method).toBe('POST');
		expect(call[1].headers).toMatchObject({ 'X-CSRF-Token': 'test-csrf' });
		bridge.close();
		bridge2.close();
	});

	it('passes an AbortSignal to apiJson so the fetch is cancellable', async () => {
		// Regression pin for the AbortController wiring. If a future
		// edit drops the `signal` option, the bridge loses the
		// client-side deadline and reverts to waiting for the
		// browser's ~75 s TCP-connect timeout on a hanging server —
		// each reconnect cycle costs 5 s instead of 2 s. The pin is
		// that `signal` is PRESENT and is an `AbortSignal`; the
		// deadline behaviour is covered by the follow-up test.
		apiJsonMock.mockClear();
		const { in: workerSide, out: tabSide } = newChannel();
		const bridge = new SharedWorkerBridge(tabSide);
		(workerSide as unknown as { postMessage: (m: unknown) => void }).postMessage({
			kind: 'need-ticket',
			nonce: 11
		});
		await new Promise((r) => setTimeout(r, 10));

		expect(apiJsonMock).toHaveBeenCalled();
		const call = apiJsonMock.mock.calls[0] as unknown as [string, RequestInit];
		expect(call[1].signal).toBeInstanceOf(AbortSignal);
		expect(call[1].signal?.aborted).toBe(false);
		bridge.close();
	});

	it('posts ticket-fetch-failed when the fetch hangs past the deadline', async () => {
		// Pin: on a dead-but-not-refusing server, apiJson hangs
		// (no response). The AbortController fires at
		// TICKET_FETCH_TIMEOUT_MS (2 s), fetch throws AbortError,
		// bridge catches, posts `ticket-fetch-failed`. The worker's
		// `transport.ticketFetchFailed(nonce)` then bumps the
		// consecutiveFailures counter immediately — the 5 s
		// need-ticket backstop never has to fire.
		//
		// Vitest's fake timers advance the clock without real-time
		// delay, so the 2 s abort window completes near-instantly.
		vi.useFakeTimers();
		apiJsonMock.mockClear();

		// Mock apiJson to NEVER resolve on its own — simulates the
		// hung-server case. The AbortController should trip the
		// signal from the caller side, which fetch would normally
		// honour by rejecting with AbortError. Our mock has to do
		// the rejection manually since it isn't the real fetch.
		apiJsonMock.mockImplementationOnce(
			(_url: string, init?: RequestInit) =>
				new Promise((_resolve, reject) => {
					init?.signal?.addEventListener('abort', () => {
						const err = new Error('aborted');
						(err as Error & { name: string }).name = 'AbortError';
						reject(err);
					});
				})
		);

		const { in: workerSide, out: tabSide } = newChannel();
		const received: Array<{ kind: string; [k: string]: unknown }> = [];
		const nodeWorker = workerSide as unknown as {
			on: (ev: 'message', cb: (msg: unknown) => void) => void;
			start?: () => void;
			postMessage: (m: unknown) => void;
		};
		nodeWorker.on('message', (m) => received.push(m as { kind: string }));
		nodeWorker.start?.();

		const bridge = new SharedWorkerBridge(tabSide);
		nodeWorker.postMessage({ kind: 'need-ticket', nonce: 42 });
		await vi.advanceTimersByTimeAsync(10);
		expect(apiJsonMock).toHaveBeenCalled();

		// Nothing fired yet — abort hasn't tripped.
		expect(received.some((m) => m.kind === 'ticket-fetch-failed')).toBe(false);

		// Jump to just after the 2 s deadline — abort trips, catch
		// branch runs, ticket-fetch-failed posts.
		await vi.advanceTimersByTimeAsync(2_100);

		const failed = received.find((m) => m.kind === 'ticket-fetch-failed') as
			| { kind: string; nonce: number }
			| undefined;
		expect(failed).toBeDefined();
		expect(failed?.nonce).toBe(42);

		bridge.close();
		vi.useRealTimers();
	});
});

describe('SharedWorkerBridge — subscribe refresh + session change', () => {
	/** Worker side of a channel, with inbound envelopes collected. */
	function wire(): {
		bridge: SharedWorkerBridge;
		post: (m: unknown) => void;
		received: Array<{ kind: string; [k: string]: unknown }>;
	} {
		const { in: workerSide, out: tabSide } = newChannel();
		const received: Array<{ kind: string; [k: string]: unknown }> = [];
		const nodeWorker = workerSide as unknown as {
			on: (ev: 'message', cb: (msg: unknown) => void) => void;
			start?: () => void;
			postMessage: (m: unknown) => void;
		};
		nodeWorker.on('message', (m) => received.push(m as { kind: string }));
		nodeWorker.start?.();
		return {
			bridge: new SharedWorkerBridge(tabSide),
			post: (m) => nodeWorker.postMessage(m),
			received
		};
	}

	const settle = () => new Promise((r) => setTimeout(r, 10));

	// The tab-side half of the dropped-replay fix: the worker forwards a
	// reconnect ack as `subscribe-refresh`, and the bridge must treat it like
	// any other ack — cache it and fan it out — or the consumer still never
	// hears that its permissions changed.
	it('fans a subscribe-refresh out to the topic’s ack handlers', async () => {
		const { bridge, post, received } = wire();
		const acks: unknown[] = [];
		bridge.subscribe(
			'collab:f1',
			() => {},
			undefined,
			(ack) => acks.push(ack)
		);
		await settle();
		// Answer the bridge's own subscribe first, so the entry exists.
		const sub = received.find((m) => m.kind === 'subscribe') as unknown as { reqId: number };
		post({ kind: 'ack', reqId: sub.reqId, result: { subscribed: 'collab:f1' } });
		await settle();
		expect(acks).toHaveLength(1);

		post({
			kind: 'subscribe-refresh',
			topic: 'collab:f1',
			ack: { subscribed: 'collab:f1', capabilities: { can_write: true } }
		});
		await settle();

		expect(acks).toHaveLength(2);
		expect(acks[1]).toEqual({ subscribed: 'collab:f1', capabilities: { can_write: true } });
		bridge.close();
	});

	// Logging out has to reach the worker: it outlives the session, and its
	// caches were all computed for the person who just left. Without this the
	// next user in that window inherits the previous one's subscriptions.
	it('tells the worker when the identity changes, and forgets its own topics', async () => {
		const { bridge, post, received } = wire();
		bridge.subscribe('collab:f1', () => {});
		await settle();
		const first = received.find((m) => m.kind === 'subscribe') as unknown as { reqId: number };
		post({ kind: 'ack', reqId: first.reqId, result: { subscribed: 'collab:f1' } });
		await settle();

		expect(sessionCleared.fire).toBeTypeOf('function');
		sessionCleared.fire?.();
		await settle();

		expect(received.some((m) => m.kind === 'session-changed')).toBe(true);

		// Local state went too: subscribing again must hit the worker rather
		// than being answered from the bridge's own `#subs` cache.
		received.length = 0;
		bridge.subscribe('collab:f1', () => {});
		await settle();
		expect(received.some((m) => m.kind === 'subscribe')).toBe(true);
		bridge.close();
	});
});
