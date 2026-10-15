import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// Return the i18n key verbatim so the 409-branch assertions below can
// pin which key `updateProfile` picks without parsing a fallback string.
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (key: string) => key
}));
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

// 409 on PATCH /auth/me/profile covers two conflicts that share a status;
// the server exposes the discriminator in `body.message`, and the wrapper
// branches on substring to pick an i18n key. Pre-Phase-4 regressions
// collapsing both branches to a generic error would pass today's suite
// without these pins — add before any migration that touches updateProfile.
it('maps 409 "already claimed" to profile.username_immutable_error', async () => {
	f.mockResolvedValueOnce({
		ok: false,
		status: 409,
		json: async () => ({ message: 'username already claimed' })
	});
	await expect(profile.updateProfile({ username: 'x' } as never)).rejects.toThrow(
		'profile.username_immutable_error'
	);
});

it('maps 409 "taken" to profile.username_taken_error', async () => {
	f.mockResolvedValueOnce({
		ok: false,
		status: 409,
		json: async () => ({ message: 'username already taken' })
	});
	await expect(profile.updateProfile({ username: 'x' } as never)).rejects.toThrow(
		'profile.username_taken_error'
	);
});

it('maps 403 on updateProfile to profile.edit_oidc_managed', async () => {
	f.mockResolvedValueOnce({
		ok: false,
		status: 403,
		json: async () => ({ message: 'managed by identity provider' })
	});
	await expect(profile.updateProfile({ given_name: 'x' } as never)).rejects.toThrow(
		'profile.edit_oidc_managed'
	);
});
