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
import { MessageChannel as NodeMessageChannel } from 'node:worker_threads';

// Hoisted mocks — the bridge's module graph pulls in `apiJson` and
// `getCsrfHeaders` via static import.
const { apiJsonMock, csrfMock } = vi.hoisted(() => ({
	apiJsonMock: vi.fn(async () => ({
		ticket: 'uuid-1',
		expires_in_seconds: 30,
		subprotocol: 'oxi.ticket.uuid-1'
	})),
	csrfMock: vi.fn(() => ({ 'X-CSRF-Token': 'test-csrf' }))
}));

vi.mock('$lib/api/client', () => ({ apiJson: apiJsonMock }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: csrfMock }));
vi.mock('$app/paths', () => ({ base: '' }));

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
});
