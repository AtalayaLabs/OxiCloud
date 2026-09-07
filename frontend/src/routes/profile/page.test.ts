import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Test-double session store. Post the three-layer UserDto refactor
// (docs/plan/userdto-refactor.md), production `session.user` is a
// derived accessor over `session.me.full.user`. The stub here mirrors
// that shape: `me` carries the whole SelfUser tree, and `user` mirrors
// `me.full.user` so any legacy `session.user.foo` read on the tested
// page keeps working through the mock without reproducing the derived
// mechanism.
const buildSelfMe = () => ({
	full: {
		user: {
			id: '1',
			username: 'admin',
			email: 'a@x.test',
			given_name: 'A',
			family_name: 'B',
			role: 'admin',
			is_external: false
		},
		storage_used_bytes: 100,
		storage_quota_bytes: 1000,
		has_password: true
	}
});

const { session, ui } = vi.hoisted(() => {
	const me = {
		full: {
			user: {
				id: '1',
				username: 'admin',
				email: 'a@x.test',
				given_name: 'A',
				family_name: 'B',
				role: 'admin',
				is_external: false
			},
			storage_used_bytes: 100,
			storage_quota_bytes: 1000,
			has_password: true
		}
	};
	return {
		session: {
			loaded: true,
			load: vi.fn(),
			me,
			user: me.full.user
		},
		ui: { notify: vi.fn() }
	};
});
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
	// Reset the shared session each test (handlers may mutate session.me
	// on save / refresh). `me` is the SelfUser tree; `user` mirrors
	// `me.full.user` for legacy `session.user.foo` reads.
	session.loaded = true;
	const me = buildSelfMe();
	session.me = me;
	session.user = me.full.user;
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
