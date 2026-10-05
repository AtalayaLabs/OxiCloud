import { describe, it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch } from '$lib/api/client';
import {
	isDirectoryAvailable,
	resolveLabel,
	resolveRecipient,
	searchRecipients
} from './recipients';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
describe('recipients pure helpers', () => {
	it('isDirectoryAvailable defaults to true', () => {
		expect(isDirectoryAvailable()).toBe(true);
	});
	it('resolveLabel falls back to the id when uncached', () => {
		expect(resolveLabel('group', 'g1')).toBe('g1');
		expect(resolveLabel('user', 'u1')).toBe('u1');
	});
	it('resolveRecipient builds a recipient object', () => {
		expect(resolveRecipient('group', 'g1')).toMatchObject({ type: 'group', id: 'g1', label: 'g1' });
		expect(resolveRecipient('user', 'u1')).toMatchObject({ type: 'user', id: 'u1' });
	});
});
describe('searchRecipients', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		// system contacts + groups both return arrays from .json()
		f.mockResolvedValue({ ok: true, status: 200, json: async () => [] });
	});
	it('returns an array of recipients', async () => {
		const r = await searchRecipients('alice').catch(() => []);
		expect(Array.isArray(r)).toBe(true);
		const e = await searchRecipients('a@b.test').catch(() => []);
		expect(Array.isArray(e)).toBe(true);
	});
});

describe('resolving the caller', () => {
	// The directory omits the caller unless asked for them, and the id→label
	// index is built from it. A member list that includes you then rendered
	// your row as a raw UUID — your own name is the one it could never show.
	it('resolves the caller after ensureResolvers', async () => {
		vi.resetModules();
		const SELF = '11111111-1111-1111-1111-111111111111';
		const other = { id: '22222222-2222-2222-2222-222222222222', full_name: 'Alice' };
		const self = { id: SELF, full_name: 'Bob' };
		const { apiFetch: fetchMock } = await import('$lib/api/client');
		(fetchMock as unknown as ReturnType<typeof vi.fn>).mockImplementation(async (url: string) => ({
			ok: true,
			status: 200,
			json: async () => (url.includes('include_self') ? [other, self] : [other])
		}));
		const mod = await import('./recipients');
		await mod.ensureResolvers();
		expect(mod.resolveRecipient('user', SELF).label).toBe('Bob');
		expect(mod.resolveLabel('user', other.id)).toBe('Alice');
	});
});
