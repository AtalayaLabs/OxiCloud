import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { session, ui } = vi.hoisted(() => ({
	session: {
		loaded: true,
		load: vi.fn(),
		user: {
			id: '1',
			username: 'admin',
			email: 'a@x.test',
			given_name: 'A',
			family_name: 'B',
			role: 'admin',
			storage_used_bytes: 100,
			storage_quota_bytes: 1000,
			is_external: false,
			has_password: true
		}
	},
	ui: { notify: vi.fn() }
}));
vi.mock('$lib/stores/session.svelte', () => ({ session }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog: vi.fn() }));
vi.mock('$lib/utils/errors', () => ({ errorToast: vi.fn() }));
vi.mock('$lib/api/endpoints/auth', () => ({ getOidcProviders: vi.fn() }));
vi.mock('$lib/api/endpoints/profile', () => ({
	changePassword: vi.fn(),
	createAppPassword: vi.fn(),
	isAutoAppPassword: () => false,
	listAppPasswords: vi.fn(),
	revokeAppPassword: vi.fn(),
	updateAvatar: vi.fn(),
	updateProfile: vi.fn()
}));

import * as profile from '$lib/api/endpoints/profile';
import { getOidcProviders } from '$lib/api/endpoints/auth';
import { confirmDialog } from '$lib/stores/dialogs.svelte';
import ProfilePage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

beforeEach(() => {
	vi.clearAllMocks();
	// Reset the shared session each test (handlers may mutate session.user).
	session.loaded = true;
	session.user = {
		id: '1',
		username: 'admin',
		email: 'a@x.test',
		given_name: 'A',
		family_name: 'B',
		role: 'admin',
		storage_used_bytes: 100,
		storage_quota_bytes: 1000,
		is_external: false,
		has_password: true
	};
	m(profile.listAppPasswords).mockResolvedValue([]);
	m(profile.updateProfile).mockResolvedValue(undefined);
	m(getOidcProviders).mockResolvedValue({ password_login_enabled: true });
});

it('renders and saves the profile form', async () => {
	m(profile.updateProfile).mockResolvedValue(undefined);
	render(ProfilePage);
	await screen.findByTestId('profile-edit-form');
	await fireEvent.input(screen.getByTestId('profile-given-name-input'), {
		target: { value: 'New' }
	});
	await fireEvent.click(screen.getByTestId('profile-save-btn'));
	await waitFor(() => expect(profile.updateProfile).toHaveBeenCalled());
});

it('generates an app password', async () => {
	m(profile.createAppPassword).mockResolvedValue({ id: 'ap1', secret: 'xyz', label: 'tok' });
	render(ProfilePage);
	await screen.findByTestId('profile-app-pw-label-input');
	await fireEvent.input(screen.getByTestId('profile-app-pw-label-input'), {
		target: { value: 'tok' }
	});
	await fireEvent.click(screen.getByTestId('profile-app-pw-generate-btn'));
	await waitFor(() => expect(profile.createAppPassword).toHaveBeenCalledWith('tok'));
});

it('rejects a mismatched password change without calling the API', async () => {
	render(ProfilePage);
	await screen.findByTestId('profile-password-form');
	await fireEvent.input(screen.getByTestId('profile-current-password-input'), {
		target: { value: 'old' }
	});
	await fireEvent.input(screen.getByTestId('profile-new-password-input'), {
		target: { value: 'new1' }
	});
	await fireEvent.input(screen.getByTestId('profile-confirm-password-input'), {
		target: { value: 'new2' }
	});
	await fireEvent.click(screen.getByTestId('profile-update-password-btn'));
	expect(profile.changePassword).not.toHaveBeenCalled();
});

it('changes the password when the confirmation matches', async () => {
	m(profile.changePassword).mockResolvedValue(undefined);
	render(ProfilePage);
	await screen.findByTestId('profile-password-form');
	await fireEvent.input(screen.getByTestId('profile-current-password-input'), {
		target: { value: 'OldPassword1!' }
	});
	await fireEvent.input(screen.getByTestId('profile-new-password-input'), {
		target: { value: 'NewPassword1!' }
	});
	await fireEvent.input(screen.getByTestId('profile-confirm-password-input'), {
		target: { value: 'NewPassword1!' }
	});
	await fireEvent.click(screen.getByTestId('profile-update-password-btn'));
	await waitFor(() => expect(profile.changePassword).toHaveBeenCalled());
});

it('revokes an existing app password after confirmation', async () => {
	m(profile.listAppPasswords).mockResolvedValue([
		{ id: 'ap1', label: 'CLI token', created_at: '2024-01-01T00:00:00Z' }
	]);
	m(confirmDialog).mockResolvedValue(true);
	m(profile.revokeAppPassword).mockResolvedValue(undefined);
	render(ProfilePage);
	await fireEvent.click(await screen.findByTestId('profile-app-pw-revoke-ap1'));
	await waitFor(() => expect(profile.revokeAppPassword).toHaveBeenCalledWith('ap1'));
});
