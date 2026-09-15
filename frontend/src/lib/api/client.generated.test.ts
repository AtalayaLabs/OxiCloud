import { beforeEach, expect, it, vi } from 'vitest';

const network = vi.hoisted(() => {
	const fetch = vi.fn<typeof globalThis.fetch>();
	globalThis.fetch = fetch;
	return fetch;
});
vi.mock('$lib/auth/dpop-proof', () => ({
	buildDpopProof: vi.fn(async () => 'proof'),
	updateNonceFromResponse: vi.fn(),
	isDpopNonceChallenge: (res: Response) =>
		res.headers.get('WWW-Authenticate')?.includes('use_dpop_nonce') ?? false
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({ 'X-CSRF-Token': 'csrf' }) }));
import { apiFetch, setLogoutInProgress } from './client';
import { client } from './generated/client.gen';
import { renamePerson } from './generated/sdk.gen';
import { subscribePluginLogs } from './endpoints/admin';
import { buildDpopProof } from '$lib/auth/dpop-proof';

beforeEach(() => {
	vi.restoreAllMocks();
	network.mockReset();
	setLogoutInProgress(false);
	client.setConfig({ baseUrl: location.origin });
});

it('routes existing endpoint calls through the generated client', async () => {
	const request = vi.spyOn(client, 'request');
	network.mockResolvedValue(Response.json([]));
	const response = await apiFetch('/api/people');
	expect(await response.json()).toEqual([]);
	expect(request).toHaveBeenCalledOnce();
});

it('preserves raw error bodies for endpoint-specific error handling', async () => {
	network.mockResolvedValue(new Response('upstream unavailable', { status: 503 }));
	const response = await apiFetch('/api/people');
	expect(response.status).toBe(503);
	expect(await response.text()).toBe('upstream unavailable');
});

it('replays generated mutations after refresh with the same body, method and headers', async () => {
	const attempts: Request[] = [];
	const bodies: string[] = [];
	network.mockImplementation(async (input, init) => {
		const request = new Request(
			typeof input === 'string' ? new URL(input, location.origin) : input,
			init
		);
		attempts.push(request);
		bodies.push(await request.text()); // Simulate fetch consuming the request body.
		return Response.json({}, { status: attempts.length === 1 ? 401 : 200 });
	});
	await renamePerson({ path: { id: 'person' }, body: { name: 'Alice' }, throwOnError: true });
	expect(attempts.map((r) => r.method)).toEqual(['PATCH', 'POST', 'PATCH']);
	expect(bodies).toEqual(['{"name":"Alice"}', '{}', '{"name":"Alice"}']);
	expect(attempts[0].headers.get('Content-Type')).toBe('application/json');
	expect(attempts[0].headers.get('X-CSRF-Token')).toBe('csrf');
	expect(buildDpopProof).toHaveBeenCalledWith(
		'PATCH',
		expect.stringContaining('/api/people/person')
	);
});

it('keeps multipart boundaries and binary responses intact', async () => {
	let sent: Request | undefined;
	network.mockImplementation(async (input, init) => {
		sent = new Request(input, init);
		return new Response(new Uint8Array([0, 255, 3]), { headers: { 'X-Next-Cursor': 'next' } });
	});
	const form = new FormData();
	form.set('folder_id', 'folder');
	const response = await apiFetch('/api/files/upload', { method: 'POST', body: form });
	expect(sent?.headers.get('Content-Type')).toMatch(/^multipart\/form-data; boundary=/);
	expect(await sent?.text()).toContain('folder');
	expect(response.headers.get('X-Next-Cursor')).toBe('next');
	expect(new Uint8Array(await response.arrayBuffer())).toEqual(new Uint8Array([0, 255, 3]));
});

it('does not leak CSRF or DPoP to another origin', async () => {
	network.mockResolvedValue(Response.json({}));
	await apiFetch('https://elsewhere.test/api/example', { method: 'POST', body: '{}' });
	const [input, init] = network.mock.calls[0];
	const request = new Request(input, init);
	expect(request.headers.has('X-CSRF-Token')).toBe(false);
	expect(request.headers.has('DPoP')).toBe(false);
});

it('routes live logs and lagged events through generated SSE and stops on close', async () => {
	let cancelled = false;
	network.mockResolvedValue(
		new Response(
			new ReadableStream({
				start(controller) {
					controller.enqueue(
						new TextEncoder().encode('data: {"msg":"hello"}\n\nevent: lagged\ndata: {}\n\n')
					);
				},
				cancel() {
					cancelled = true;
				}
			}),
			{ headers: { 'Content-Type': 'text/event-stream' } }
		)
	);
	const onEntry = vi.fn();
	const onLagged = vi.fn();
	const subscription = subscribePluginLogs('plugin', onEntry, onLagged);
	try {
		await vi.waitFor(() => expect(onLagged).toHaveBeenCalledOnce());
		expect(onEntry).toHaveBeenCalledWith({ msg: 'hello' });
		const [input, init] = network.mock.calls[0];
		const request = new Request(input, init);
		expect(request.url).toBe(`${location.origin}/api/admin/plugins/plugin/logs/stream`);
		expect(request.headers.get('DPoP')).toBe('proof');
	} finally {
		subscription.close();
	}
	await vi.waitFor(() => expect(cancelled).toBe(true));
});
