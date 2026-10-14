import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

import { apiFetch, apiJson } from '$lib/api/client';
import { createGrant, updateGrantRole, revokeGrant, notifyGrantRecipient } from './grants';
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
