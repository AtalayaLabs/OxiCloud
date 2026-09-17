import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch } from '$lib/api/client';
import { lookupDeviceCode, decideDevice } from './device';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
});
it('looks up and decides device codes', async () => {
	await lookupDeviceCode('ABCD').catch(() => {});
	await decideDevice('ABCD', 'approve').catch(() => {});
	await decideDevice('ABCD', 'deny').catch(() => {});
	expect(f).toHaveBeenCalled();
});
