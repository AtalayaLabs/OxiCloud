import { it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { goto, pageState, session } = vi.hoisted(() => {
	// `setUser` mirrors the real SessionStore method: sets the user and
	// runs `ensureActiveUser` (localStorage cleanup on account switch).
	// Tests don't care about the cleanup; the mock just assigns.
	const store: { user: unknown; setUser: (u: unknown) => void } = {
		user: null,
		setUser(u) {
			store.user = u;
		}
	};
	return {
		goto: vi.fn(),
		pageState: { url: new URL('http://localhost/login') } as { url: URL },
		session: store
	};
});
vi.mock('$app/navigation', () => ({ goto }));
vi.mock('$app/state', () => ({ page: pageState }));
vi.mock('$lib/stores/session.svelte', () => ({ session }));
vi.mock('$lib/api/endpoints/auth', () => ({
	bindDpopIfPossible: vi.fn().mockResolvedValue(false),
	exchangeOidcCode: vi.fn(),
	fetchMe: vi.fn(),
	getOidcProviders: vi.fn(),
	getAuthStatus: vi.fn(),
	login: vi.fn(),
	register: vi.fn(),
	sendMagicLink: vi.fn(),
	setupAdmin: vi.fn()
}));

import * as auth from '$lib/api/endpoints/auth';
import LoginPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

beforeEach(() => {
	vi.clearAllMocks();
	pageState.url = new URL('http://localhost/login');
	session.user = null;
	m(auth.fetchMe).mockResolvedValue(null);
	// Default provider info: both password + magic-link enabled, OIDC off.
	// The unified login form's magic-link submit path is only reachable
	// when `magic_link_login_enabled === true` — without this pin the
	// "sends a magic link" test can't reach `sendMagicLink()`.
	m(auth.getOidcProviders).mockResolvedValue({
		enabled: false,
		password_login_enabled: true,
		magic_link_login_enabled: true
	});
	m(auth.getAuthStatus).mockResolvedValue({ initialized: true });
});

// jsdom's `Location` can't be spied on in place (its setters trigger
// "not implemented" navigation errors), so swap the whole object for a
// stub around each test that needs to observe `window.location.replace`.
const originalLocation = window.location;
let replaceSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
	replaceSpy = vi.fn();
	Object.defineProperty(window, 'location', {
		configurable: true,
		value: { ...originalLocation, replace: replaceSpy }
	});
});

afterEach(() => {
	Object.defineProperty(window, 'location', { configurable: true, value: originalLocation });
});

it('logs in and redirects', async () => {
	m(auth.login).mockResolvedValue({ user: { id: '1' } });
	render(LoginPage);
	await screen.findByTestId('login-form');
	await fireEvent.input(screen.getByTestId('login-username-input'), { target: { value: 'admin' } });
	await fireEvent.input(screen.getByTestId('login-password-input'), { target: { value: 'pw' } });
	await fireEvent.click(screen.getByTestId('login-submit-btn'));
	await waitFor(() => expect(auth.login).toHaveBeenCalled());
});

it('exchanges an oidc code on mount and redirects', async () => {
	pageState.url = new URL('http://localhost/login?oidc_code=abc');
	m(auth.exchangeOidcCode).mockResolvedValue({ id: '1' });
	render(LoginPage);
	await waitFor(() => expect(auth.exchangeOidcCode).toHaveBeenCalledWith('abc'));
	await waitFor(() => expect(goto).toHaveBeenCalled());
});

it('skips the form when already authenticated', async () => {
	m(auth.fetchMe).mockResolvedValue({ id: '1' });
	render(LoginPage);
	await waitFor(() => expect(goto).toHaveBeenCalled());
});

it('enters setup mode on a fresh install', async () => {
	m(auth.getAuthStatus).mockResolvedValue({ initialized: false });
	render(LoginPage);
	await screen.findByTestId('login-setup-form');
});

it('sends a magic link when the password field is left empty', async () => {
	// Unified form: the same identifier input drives both flows. Filling
	// the identifier and leaving password empty makes `submitAsMagicLink`
	// derived resolve to true — the single submit button then dispatches
	// to `sendMagicLink` instead of `login`.
	m(auth.sendMagicLink).mockResolvedValue('sent');
	render(LoginPage);
	await screen.findByTestId('login-form');
	await fireEvent.input(screen.getByTestId('login-username-input'), {
		target: { value: 'a@b.test' }
	});
	// Password intentionally NOT filled.
	await fireEvent.click(screen.getByTestId('login-submit-btn'));
	await waitFor(() => expect(auth.sendMagicLink).toHaveBeenCalledWith('a@b.test'));
	expect(auth.login).not.toHaveBeenCalled();
});

it('registers a new account', async () => {
	m(auth.register).mockResolvedValue(undefined);
	render(LoginPage);
	await screen.findByTestId('login-form');
	await fireEvent.click(screen.getByTestId('login-to-register-btn'));
	await fireEvent.input(screen.getByTestId('login-register-username-input'), {
		target: { value: 'u' }
	});
	await fireEvent.input(screen.getByTestId('login-register-email-input'), {
		target: { value: 'u@b.test' }
	});
	await fireEvent.input(screen.getByTestId('login-register-password-input'), {
		target: { value: 'TestPassword1!' }
	});
	await fireEvent.input(screen.getByTestId('login-register-confirm-input'), {
		target: { value: 'TestPassword1!' }
	});
	await fireEvent.click(screen.getByTestId('login-register-submit-btn'));
	await waitFor(() => expect(auth.register).toHaveBeenCalled());
});

it('shows an error message when login fails', async () => {
	m(auth.login).mockRejectedValue(new Error('bad credentials'));
	render(LoginPage);
	await screen.findByTestId('login-form');
	await fireEvent.input(screen.getByTestId('login-username-input'), { target: { value: 'admin' } });
	await fireEvent.input(screen.getByTestId('login-password-input'), { target: { value: 'wrong' } });
	await fireEvent.click(screen.getByTestId('login-submit-btn'));
	await waitFor(() => expect(screen.getByText('bad credentials')).toBeTruthy());
});

it('rejects a registration with mismatched passwords without calling the API', async () => {
	render(LoginPage);
	await screen.findByTestId('login-form');
	await fireEvent.click(screen.getByTestId('login-to-register-btn'));
	await fireEvent.input(screen.getByTestId('login-register-username-input'), {
		target: { value: 'u' }
	});
	await fireEvent.input(screen.getByTestId('login-register-password-input'), {
		target: { value: 'TestPassword1!' }
	});
	await fireEvent.input(screen.getByTestId('login-register-confirm-input'), {
		target: { value: 'Different1!' }
	});
	await fireEvent.click(screen.getByTestId('login-register-submit-btn'));
	expect(auth.register).not.toHaveBeenCalled();
});

it('creates the first administrator in setup mode', async () => {
	m(auth.getAuthStatus).mockResolvedValue({ initialized: false });
	m(auth.setupAdmin).mockResolvedValue(undefined);
	render(LoginPage);
	await screen.findByTestId('login-setup-form');
	await fireEvent.input(screen.getByTestId('login-setup-email-input'), {
		target: { value: 'admin@x.test' }
	});
	await fireEvent.input(screen.getByTestId('login-setup-password-input'), {
		target: { value: 'TestPassword1!' }
	});
	await fireEvent.input(screen.getByTestId('login-setup-confirm-input'), {
		target: { value: 'TestPassword1!' }
	});
	await fireEvent.click(screen.getByTestId('login-setup-submit-btn'));
	await waitFor(() =>
		expect(auth.setupAdmin).toHaveBeenCalledWith('admin@x.test', 'TestPassword1!')
	);
});

it('renders an SSO sign-in link when an OIDC provider is configured', async () => {
	m(auth.getOidcProviders).mockResolvedValue({
		enabled: true,
		authorize_endpoint: 'https://idp.test/auth',
		provider_name: 'Acme SSO',
		password_login_enabled: true
	});
	render(LoginPage);
	const sso = await screen.findByTestId('login-oidc-btn');
	expect(sso.getAttribute('href')).toBe('https://idp.test/auth');
});

// Auto-redirect on standalone OIDC is enforced server-side via the
// `auto_redirect_if_standalone_oidc` policy (see
// src/interfaces/web/mod.rs::oidc_standalone_login_redirect). The SPA no
// longer contains a client-side copy — a duplicate would override the admin's
// policy choice. We keep this test asserting the *negative* to lock in
// "SPA renders the click-to-continue button, no window.location.replace".
it('does not client-side auto-redirect when OIDC is the only login method', async () => {
	m(auth.getOidcProviders).mockResolvedValue({
		enabled: true,
		password_login_enabled: false,
		authorize_endpoint: '/api/auth/oidc/authorize'
	});
	render(LoginPage);
	// Give onMount time to finish its probes; the SSO button must appear
	// and `window.location.replace` must NOT have been called.
	await screen.findByTestId('login-oidc-btn');
	expect(replaceSpy).not.toHaveBeenCalled();
});

it('does not auto-redirect when password login is also enabled', async () => {
	m(auth.getOidcProviders).mockResolvedValue({
		enabled: true,
		password_login_enabled: true,
		authorize_endpoint: '/api/auth/oidc/authorize'
	});
	render(LoginPage);
	await screen.findByTestId('login-form');
	expect(replaceSpy).not.toHaveBeenCalled();
});

it('does not auto-redirect after the IdP already returned an error (loop guard)', async () => {
	pageState.url = new URL('http://localhost/login?error=access_denied');
	m(auth.getOidcProviders).mockResolvedValue({
		enabled: true,
		password_login_enabled: false,
		authorize_endpoint: '/api/auth/oidc/authorize'
	});
	render(LoginPage);
	await screen.findByTestId('login-form');
	expect(replaceSpy).not.toHaveBeenCalled();
});

it('does not auto-redirect during first-run setup', async () => {
	m(auth.getAuthStatus).mockResolvedValue({ initialized: false });
	m(auth.getOidcProviders).mockResolvedValue({
		enabled: true,
		password_login_enabled: false,
		authorize_endpoint: '/api/auth/oidc/authorize'
	});
	render(LoginPage);
	await screen.findByTestId('login-setup-form');
	expect(replaceSpy).not.toHaveBeenCalled();
});
