import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { session, ui } = vi.hoisted(() => ({
	session: { user: { id: '1', username: 'admin', role: 'admin' } },
	ui: { notify: vi.fn() }
}));
vi.mock('$lib/stores/session.svelte', () => ({ session }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/api/endpoints/admin', () => ({
	clearPluginLogs: vi.fn(),
	createExternalMount: vi.fn(),
	createUser: vi.fn(),
	deleteExternalMount: vi.fn(),
	deletePlugin: vi.fn(),
	deleteUser: vi.fn(),
	getDashboard: vi.fn(),
	listExternalMounts: vi.fn(),
	getMigration: vi.fn(),
	getOidcSettings: vi.fn(),
	getPluginLogs: vi.fn(),
	getPluginRetention: vi.fn(),
	getSmtpInfo: vi.fn(),
	getStorageSettings: vi.fn(),
	installPlugin: vi.fn(),
	listPlugins: vi.fn(),
	listUsers: vi.fn(),
	migrationAction: vi.fn(),
	resetUserPassword: vi.fn(),
	saveOidc: vi.fn(),
	savePluginRetention: vi.fn(),
	saveStorage: vi.fn(),
	sendSmtpTest: vi.fn(),
	setPluginEnabled: vi.fn(),
	setRegistrationEnabled: vi.fn(),
	setUserActive: vi.fn(),
	setUserQuota: vi.fn(),
	setUserRole: vi.fn(),
	testOidc: vi.fn(),
	testStorage: vi.fn(),
	verifyMigration: vi.fn()
}));

import * as admin from '$lib/api/endpoints/admin';
import AdminPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

const dashboard = {
	total_users: 3,
	active_users: 2,
	admin_users: 1,
	server_version: '1.0',
	total_used_bytes: 100,
	total_quota_bytes: 1000,
	storage_usage_percent: 10,
	auth_enabled: true,
	oidc_configured: false,
	quotas_enabled: true,
	registration_enabled: true,
	users_over_80_percent: 0,
	users_over_quota: 0
};

const user = {
	id: 'u1',
	username: 'bob',
	email: 'bob@x.test',
	role: 'user',
	active: true,
	is_active: true,
	storage_used_bytes: 10,
	storage_quota_bytes: 100,
	is_external: false
};

const mount = {
	mount_folder_id: 'mnt-1',
	name: 'Media',
	kind: 'local_fs',
	owner_id: 'admin',
	read_only: true,
	drive_id: 'd1',
	mount_path: 'Personal/Media',
	config: { path: '/srv/media', read_only: true }
};

beforeEach(() => {
	vi.clearAllMocks();
	m(admin.getDashboard).mockResolvedValue(dashboard);
	m(admin.listUsers).mockResolvedValue({ total: 1, users: [user] });
	m(admin.listPlugins).mockResolvedValue({ available: true, enabled: true, plugins: [] });
	m(admin.getOidcSettings).mockResolvedValue({
		enabled: false,
		issuer_url: '',
		client_id: '',
		scopes: null,
		auto_provision: false,
		admin_groups: null,
		disable_password_login: false,
		provider_name: null,
		callback_url: 'http://localhost/callback',
		client_secret_set: false,
		env_overrides: []
	});
	m(admin.getStorageSettings).mockResolvedValue({ backend: 'filesystem', env_overrides: [] });
	m(admin.getMigration).mockResolvedValue({
		status: 'idle',
		total_blobs: 0,
		migrated_blobs: 0,
		migrated_bytes: 0
	});
	m(admin.getSmtpInfo).mockResolvedValue({
		enabled: false,
		host: 'localhost',
		port: 25,
		tls: 'none',
		from: 'a@x.test',
		user_state: 'unset'
	});
	m(admin.listExternalMounts).mockResolvedValue([mount]);
});

it('loads the dashboard on mount', async () => {
	render(AdminPage);
	await waitFor(() => expect(admin.getDashboard).toHaveBeenCalled());
});

it('toggles registration from the dashboard', async () => {
	m(admin.setRegistrationEnabled).mockResolvedValue(undefined);
	render(AdminPage);
	const cb = await screen.findByTestId('admin-dashboard-registration-checkbox');
	await fireEvent.click(cb);
	await waitFor(() => expect(admin.setRegistrationEnabled).toHaveBeenCalled());
});

it('loads users when the users tab is opened and creates a user', async () => {
	m(admin.createUser).mockResolvedValue(undefined);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-users-tab'));
	await waitFor(() => expect(admin.listUsers).toHaveBeenCalled());
	await fireEvent.click(await screen.findByTestId('admin-users-create-btn'));
	await fireEvent.input(await screen.findByTestId('admin-create-user-username-input'), {
		target: { value: 'newbie' }
	});
	await fireEvent.input(screen.getByTestId('admin-create-user-password-input'), {
		target: { value: 'Password123!' }
	});
	await fireEvent.click(screen.getByTestId('admin-create-user-submit-btn'));
	await waitFor(() => expect(admin.createUser).toHaveBeenCalled());
});

it('loads OIDC settings when the OIDC tab is opened', async () => {
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-oidc-tab'));
	await waitFor(() => expect(admin.getOidcSettings).toHaveBeenCalled());
});

it('loads storage + migration when the storage tab is opened', async () => {
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-storage-tab'));
	await waitFor(() => expect(admin.getStorageSettings).toHaveBeenCalled());
	await waitFor(() => expect(admin.getMigration).toHaveBeenCalled());
});

it('loads SMTP info when the SMTP tab is opened', async () => {
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-smtp-tab'));
	await waitFor(() => expect(admin.getSmtpInfo).toHaveBeenCalled());
});

it('loads plugins when the plugins tab is opened', async () => {
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-plugins-tab'));
	await waitFor(() => expect(admin.listPlugins).toHaveBeenCalled());
});

it('loads external mounts when the mounts tab is opened and lists them', async () => {
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-mounts-tab'));
	await waitFor(() => expect(admin.listExternalMounts).toHaveBeenCalled());
	// The configured mount is rendered in the table.
	expect(await screen.findByText('Media')).toBeTruthy();
});

it('creates a mount from the mounts form', async () => {
	// A distinct mount (new id) so the keyed {#each} doesn't collide with
	// the one already loaded by listExternalMounts.
	m(admin.createExternalMount).mockResolvedValue({
		...mount,
		mount_folder_id: 'mnt-2',
		name: 'Photos',
		read_only: false,
		mount_path: 'Personal/Photos',
		config: { path: '/srv/photos', read_only: false }
	});
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-mounts-tab'));
	await fireEvent.input(await screen.findByTestId('mount-name'), {
		target: { value: 'Photos' }
	});
	await fireEvent.input(screen.getByTestId('mount-path'), {
		target: { value: '/srv/photos' }
	});
	await fireEvent.click(screen.getByTestId('mount-create'));
	await waitFor(() =>
		expect(admin.createExternalMount).toHaveBeenCalledWith(
			expect.objectContaining({ name: 'Photos', host_path: '/srv/photos' })
		)
	);
});

it('deletes a mount through the confirm modal', async () => {
	m(admin.deleteExternalMount).mockResolvedValue(undefined);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-mounts-tab'));
	await fireEvent.click(await screen.findByTestId('mount-delete'));
	// deleteMount() gates on the styled confirm modal.
	await fireEvent.click(await screen.findByTestId('admin-confirm-ok-btn'));
	await waitFor(() => expect(admin.deleteExternalMount).toHaveBeenCalledWith('mnt-1'));
});

it("toggles a user's role through the confirm modal", async () => {
	m(admin.setUserRole).mockResolvedValue(undefined);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-users-tab'));
	await fireEvent.click(await screen.findByTestId('admin-user-toggle-role-u1'));
	await fireEvent.click(await screen.findByTestId('admin-confirm-ok-btn'));
	await waitFor(() => expect(admin.setUserRole).toHaveBeenCalledWith('u1', 'admin'));
});

it('deactivates a user through the confirm modal', async () => {
	m(admin.setUserActive).mockResolvedValue(undefined);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-users-tab'));
	await fireEvent.click(await screen.findByTestId('admin-user-toggle-active-u1'));
	await fireEvent.click(await screen.findByTestId('admin-confirm-ok-btn'));
	await waitFor(() => expect(admin.setUserActive).toHaveBeenCalledWith('u1', false));
});

it('saves OIDC settings from the OIDC form', async () => {
	m(admin.saveOidc).mockResolvedValue(undefined);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-oidc-tab'));
	await fireEvent.input(await screen.findByTestId('admin-oidc-issuer-input'), {
		target: { value: 'https://idp.test' }
	});
	await fireEvent.submit(await screen.findByTestId('admin-oidc-form'));
	await waitFor(() => expect(admin.saveOidc).toHaveBeenCalled());
});

it('sends an SMTP test email', async () => {
	m(admin.sendSmtpTest).mockResolvedValue({ ok: true } as never);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-smtp-tab'));
	await fireEvent.input(await screen.findByTestId('admin-smtp-to-input'), {
		target: { value: 'to@x.test' }
	});
	await fireEvent.click(await screen.findByTestId('admin-smtp-send-btn'));
	await waitFor(() => expect(admin.sendSmtpTest).toHaveBeenCalledWith('to@x.test'));
});

it('saves storage settings and starts a migration', async () => {
	m(admin.saveStorage).mockResolvedValue(undefined);
	m(admin.migrationAction).mockResolvedValue(undefined);
	render(AdminPage);
	await fireEvent.click(await screen.findByTestId('admin-storage-tab'));
	await fireEvent.submit(await screen.findByTestId('admin-storage-form'));
	await waitFor(() => expect(admin.saveStorage).toHaveBeenCalled());
	await fireEvent.click(await screen.findByTestId('admin-migration-start-btn'));
	await waitFor(() => expect(admin.migrationAction).toHaveBeenCalledWith('start'));
});
