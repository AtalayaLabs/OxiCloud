import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

import { apiFetch, apiJson } from '$lib/api/client';
import { getShareMeta, verifySharePassword } from './share';

const fetchMock = apiFetch as unknown as ReturnType<typeof vi.fn>;
const jsonMock = apiJson as unknown as ReturnType<typeof vi.fn>;

// The URL builders and `getShareContents` are gone with the six `/api/s/*`
// browsing endpoints they addressed. A visitor now browses through
// `folders` / `files` like any other caller, so those paths are covered by
// those modules' tests and by `tests/api/anonymous_share_session.hurl`.

describe('share API calls', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
		jsonMock.mockResolvedValue({});
	});
	it('hit the API for meta / verify', async () => {
		await getShareMeta('t').catch(() => {});
		await verifySharePassword('t', 'pw').catch(() => {});
		expect(fetchMock.mock.calls.length + jsonMock.mock.calls.length).toBeGreaterThan(0);
	});
});

describe('share status branches', () => {
	const resp = (over: Record<string, unknown>) => ({
		ok: false,
		status: 200,
		json: async () => ({}),
		...over
	});
	beforeEach(() => vi.clearAllMocks());

	it('returns ok meta on 200', async () => {
		fetchMock.mockResolvedValue(
			resp({ ok: true, json: async () => ({ item_type: 'folder', item_name: 'Docs' }) })
		);
		expect(await getShareMeta('t')).toEqual({
			status: 'ok',
			data: { item_type: 'folder', item_name: 'Docs' }
		});
	});

	it('maps 401+requiresPassword to a password prompt', async () => {
		fetchMock.mockResolvedValue(
			resp({ status: 401, json: async () => ({ requiresPassword: true }) })
		);
		expect(await getShareMeta('t')).toEqual({ status: 'password' });
	});

	it('maps meta 410 to expired and 404 to invalid', async () => {
		fetchMock.mockResolvedValueOnce(resp({ status: 410 }));
		expect(await getShareMeta('t')).toEqual({ status: 'expired' });
		fetchMock.mockResolvedValueOnce(resp({ status: 404 }));
		expect(await getShareMeta('t')).toEqual({ status: 'invalid' });
	});

	it('verifies a password: true on ok, false on 401', async () => {
		fetchMock.mockResolvedValueOnce(resp({ ok: true }));
		expect(await verifySharePassword('t', 'pw')).toBe(true);
		fetchMock.mockResolvedValueOnce(resp({ status: 401 }));
		expect(await verifySharePassword('t', 'bad')).toBe(false);
	});
});
