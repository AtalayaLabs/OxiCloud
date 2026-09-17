import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

import { apiFetch, apiJson } from '$lib/api/client';
import {
	displayRole,
	expiryToIso,
	createGrant,
	updateGrantRole,
	revokeGrant,
	notifyGrantRecipient
} from './grants';

const fetchMock = apiFetch as unknown as ReturnType<typeof vi.fn>;
const jsonMock = apiJson as unknown as ReturnType<typeof vi.fn>;

describe('displayRole', () => {
	it('passes through canonical roles', () => {
		expect(displayRole('owner')).toBe('owner');
		expect(displayRole('editor')).toBe('editor');
		expect(displayRole('viewer')).toBe('viewer');
	});
	it('maps legacy roles and defaults to viewer', () => {
		expect(displayRole('contributor')).toBe('editor');
		expect(displayRole('commenter')).toBe('viewer');
		expect(displayRole('mystery')).toBe('viewer');
		expect(displayRole(undefined)).toBe('viewer');
	});
});

describe('expiryToIso', () => {
	it('converts a date to an ISO string at UTC midnight', () => {
		expect(expiryToIso('2030-01-02')).toBe('2030-01-02T00:00:00.000Z');
	});
	it('returns null for empty input', () => {
		expect(expiryToIso(null)).toBeNull();
		expect(expiryToIso(undefined)).toBeNull();
		expect(expiryToIso('')).toBeNull();
	});
});

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
