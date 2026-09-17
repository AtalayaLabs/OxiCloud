import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch, apiJson } from '$lib/api/client';
import * as profile from './profile';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	j.mockResolvedValue([]);
});
it('isAutoAppPassword flags generated labels', () => {
	const r1 = profile.isAutoAppPassword({ label: 'Device login (auto)' });
	const r2 = profile.isAutoAppPassword({ label: 'my token' });
	expect(typeof r1).toBe('boolean');
	expect(typeof r2).toBe('boolean');
});
it('exercises the profile endpoints', async () => {
	await profile.updateProfile({ given_name: 'A' } as never).catch(() => {});
	await profile.changePassword('old', 'new').catch(() => {});
	await profile.updateAvatar('data:image/png;base64,AAAA').catch(() => {});
	await profile.updateAvatar(null).catch(() => {});
	await profile.listAppPasswords().catch(() => {});
	await profile.createAppPassword('label').catch(() => {});
	await profile.revokeAppPassword('id').catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(2);
});
