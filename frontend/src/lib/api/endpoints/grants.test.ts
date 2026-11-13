import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

import { apiFetch, apiJson } from '$lib/api/client';
import {
	createGrant,
	updateGrantRole,
	revokeGrant,
	notifyGrantRecipient,
	createLink,
	linkRestorable
} from './grants';
// `displayRole` + `expiryToIso` tests moved alongside their new homes
// (`$lib/utils/grantFormat.test.ts` and `$lib/utils/dateInput.test.ts`
// respectively) when the helpers were extracted. Imports here are
// no longer needed.

const fetchMock = apiFetch as unknown as ReturnType<typeof vi.fn>;
const jsonMock = apiJson as unknown as ReturnType<typeof vi.fn>;

describe('grant mutations', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
		jsonMock.mockResolvedValue({});
	});
	it('call the API for create/update/revoke/notify', async () => {
		await createGrant(
			{ type: 'user', id: 'u1' },
			{ type: 'folder', id: 'rid' },
			'viewer',
			null
		).catch(() => {});
		await updateGrantRole(
			{ type: 'user', id: 'u1' },
			{ type: 'folder', id: 'rid' },
			'editor',
			null
		).catch(() => {});
		await revokeGrant('g1').catch(() => {});
		await notifyGrantRecipient('g1').catch(() => {});
		expect(fetchMock).toHaveBeenCalled();
	});
});

describe('createLink', () => {
	beforeEach(() => vi.clearAllMocks());
	it('posts the target id and returns the link folder', async () => {
		fetchMock.mockResolvedValue({ ok: true, status: 201, json: async () => ({ id: 'l1' }) });
		const link = await createLink('t1');
		const [url, init] = fetchMock.mock.calls[0];
		expect(url).toBe('/api/links');
		expect(init.method).toBe('POST');
		expect(init.headers['Content-Type']).toBe('application/json');
		expect(JSON.parse(init.body)).toEqual({ target_id: 't1' });
		expect(link).toEqual({ id: 'l1' });
	});
	it('throws on failure', async () => {
		fetchMock.mockResolvedValue({
			ok: false,
			status: 409,
			json: async () => ({ error: 'no name' })
		});
		await expect(createLink('t1')).rejects.toThrow('no name');
	});
});

describe('linkRestorable', () => {
	it('offers the action for a declined or missing link only', () => {
		expect(linkRestorable(undefined)).toBe(false);
		expect(linkRestorable({ link_id: 'l1', link_name: 'Docs', declined: false })).toBe(false);
		expect(linkRestorable({ link_id: null, link_name: null, declined: true })).toBe(true);
		expect(linkRestorable({ link_id: null, link_name: null, declined: false })).toBe(true);
	});
});
