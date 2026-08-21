import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), apiJson: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({ 'x-csrf-token': 't' }) }));

import { apiFetch, apiJson } from '$lib/api/client';
import * as admin from './admin';

const okRes = (body: unknown = {}) =>
	({ ok: true, status: 200, json: async () => body }) as unknown as Response;
const errRes = (status = 400, body: unknown = { message: 'nope' }) =>
	({ ok: false, status, json: async () => body }) as unknown as Response;

const fetchMock = apiFetch as unknown as ReturnType<typeof vi.fn>;
const jsonMock = apiJson as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
	vi.clearAllMocks();
	fetchMock.mockResolvedValue(okRes());
	jsonMock.mockResolvedValue({});
});

describe('admin mutate-based endpoints', () => {
	it('resolve on success and call the expected URL/method', async () => {
		await admin.createUser({
			username: 'u',
			password: 'p',
			email: null,
			role: 'user',
			quota_bytes: 0
		});
		expect(fetchMock).toHaveBeenCalledWith(
			'/api/admin/users',
			expect.objectContaining({ method: 'POST' })
		);
		await admin.setUserRole('1', 'admin');
		await admin.setUserActive('1', false);
		await admin.setUserQuota('1', 100);
		await admin.resetUserPassword('1', 'newpw');
		await admin.deleteUser('1');
		await admin.setRegistrationEnabled(true);
		await admin.saveOidc({ enabled: true });
		await admin.saveStorage({ backend: 'local' });
		await admin.savePluginRetention('id', { retention_days: 30, max_bytes: 100 });
		await admin.clearPluginLogs('id');
		await admin.setPluginEnabled('id', true);
		await admin.deletePlugin('id');
		await admin.migrationAction('start');
		await admin.migrationAction('pause');
		expect(fetchMock).toHaveBeenCalled();
	});

	it('throw the server message on failure', async () => {
		fetchMock.mockResolvedValue(errRes(409, { message: 'conflict' }));
		await expect(admin.deleteUser('1')).rejects.toThrow('conflict');
	});

	it('throw a generic message when the error body has none', async () => {
		fetchMock.mockResolvedValue(errRes(500, {}));
		await expect(admin.setUserActive('1', true)).rejects.toThrow(/failed: 500/);
	});
});

describe('admin read endpoints', () => {
	it('call apiJson for the listing/settings reads', async () => {
		await admin.listUsers(25, 0);
		expect(jsonMock).toHaveBeenCalledWith('/api/admin/users?limit=25&offset=0', expect.anything());
		await admin.getDashboard();
		await admin.getSmtpInfo();
		await admin.getOidcSettings();
		await admin.getStorageSettings();
		await admin.getMigration();
		await admin.listPlugins();
		await admin.getPluginLogs('id', { limit: 50, offset: 0 });
		expect(jsonMock).toHaveBeenCalled();
	});

	it('getPluginRetention returns null when the request is not ok', async () => {
		fetchMock.mockResolvedValueOnce(errRes(404, {}));
		await expect(admin.getPluginRetention('id')).resolves.toBeNull();
		fetchMock.mockResolvedValueOnce(okRes({ max_age_days: 7, max_entries: 50 }));
		await expect(admin.getPluginRetention('id')).resolves.toMatchObject({ max_age_days: 7 });
	});
});

describe('admin test/probe endpoints', () => {
	it('sendSmtpTest maps 503 to an unconfigured message', async () => {
		fetchMock.mockResolvedValue({ status: 503, json: async () => ({}) } as unknown as Response);
		await expect(admin.sendSmtpTest('to@x.test')).resolves.toMatchObject({ success: false });
	});

	it('sendSmtpTest returns the parsed result otherwise', async () => {
		fetchMock.mockResolvedValue(okRes({ success: true }));
		await expect(admin.sendSmtpTest('to@x.test')).resolves.toMatchObject({ success: true });
	});

	it('testOidc / testStorage return parsed results', async () => {
		fetchMock.mockResolvedValue(okRes({ success: true }));
		await expect(admin.testOidc('https://idp')).resolves.toBeTruthy();
		fetchMock.mockResolvedValue(okRes({ connected: true }));
		await expect(admin.testStorage({ backend: 's3' })).resolves.toMatchObject({ connected: true });
	});

	// verifyMigration retired in slice 7 of docs/plan/storage-multi-entry.md.
	// Superseded by `POST /api/admin/jobs/blobs_consistency/trigger?storage=<name>`.

	it('installPlugin posts a FormData bundle', async () => {
		fetchMock.mockResolvedValue(okRes({ id: 'com.example.hello' }));
		const file = new File([new Uint8Array([1, 2, 3])], 'p.zip', { type: 'application/zip' });
		await admin.installPlugin(file);
		expect(fetchMock).toHaveBeenCalledWith(
			'/api/admin/plugins',
			expect.objectContaining({ method: 'POST' })
		);
	});
});
