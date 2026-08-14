<script lang="ts">
	// Route-scoped styles: kept off the global critical path (Vite code-splits
	// this into the /login route chunk, loaded only when this page renders).
	import '$lib/styles/ported/auth.css';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import type { Pathname } from '$app/types';
	import { onMount } from 'svelte';
	import { ApiError } from '$lib/api/client';
	import {
		bindDpopIfPossible,
		exchangeOidcCode,
		fetchMe,
		getAuthStatus,
		getOidcProviders,
		login,
		register,
		sendMagicLink,
		setupAdmin,
		type OidcProviders
	} from '$lib/api/endpoints/auth';
	import { i18n, SUPPORTED_LOCALES, setLocale, t, type Locale } from '$lib/i18n/index.svelte';
	import { loginErrorMessage } from '$lib/auth/loginError';
	import { session } from '$lib/stores/session.svelte';
	import { hasSessionHint } from '$lib/api/csrf';

	type Mode = 'login' | 'register' | 'setup';
	let mode = $state<Mode>('login');
	// First-run admin setup is only offered after the status probe confirms it.
	let setupAvailable = $state(false);
	// Suppress the auth UI until the onMount probes (session/oidc/status) settle,
	// to avoid flashing the login form before a redirect or the setup wizard.
	let booting = $state(true);

	// Login
	let username = $state('');
	let password = $state('');
	let showPassword = $state(false);
	let capsOn = $state(false);
	let error = $state('');
	let busy = $state(false);

	// Register. Since PR 18 both `username` and `password` are optional on
	// the backend — email-only signup mints a welcome magic-link. Leaving
	// the password blank is a deliberate first-class UX path here.
	let regUsername = $state('');
	let regEmail = $state('');
	let regPassword = $state('');
	let regConfirm = $state('');
	let regError = $state('');
	let regShowPassword = $state(false);
	let regShowConfirm = $state(false);
	let regCapsOn = $state(false);
	// True when the user has chosen the passwordless-signup branch —
	// hides the confirm-password field and switches the submit label.
	const regEmailOnly = $derived(regPassword.length === 0);

	// Admin setup (first run)
	let setupEmail = $state('');
	let setupPassword = $state('');
	let setupConfirm = $state('');
	let setupShowPassword = $state(false);
	let setupShowConfirm = $state(false);
	let setupCapsOn = $state(false);
	let setupError = $state('');
	let setupSuccess = $state('');
	const setupMatchState = $derived(
		setupConfirm.length === 0 ? '' : setupPassword === setupConfirm ? 'ok' : 'bad'
	);

	// Magic-link submit status (rendered inline after a link is sent).
	let magicStatus = $state<{ text: string; ok: boolean } | null>(null);

	// OIDC + auth-method flags exposed by /api/auth/oidc/providers.
	let oidc = $state<OidcProviders>({ enabled: false });
	// Default `true` here: on older backends the field is absent, and the
	// legacy behaviour was always-on password login.
	const passwordLoginEnabled = $derived(oidc.password_login_enabled !== false);
	// Default `false`: only render magic-link UI when the backend
	// affirmatively enables it (SMTP wired + allowlist + non-OIDC deployment).
	const magicLinkLoginEnabled = $derived(oidc.magic_link_login_enabled === true);
	// Single-form UX: the identifier + password fields double as the
	// magic-link path. When the password is empty (and the server offers
	// magic-link), submit sends a link to the identifier instead of
	// attempting password login. This eliminates the duplicate
	// identifier input the old two-form layout carried.
	const submitAsMagicLink = $derived(
		magicLinkLoginEnabled && (password.length === 0 || !passwordLoginEnabled)
	);
	// The login failure remap for "email not verified". The server
	// auto-sends a verification magic-link on this branch (piggybacked
	// on the successful password proof — see login handler), so the
	// resend "affordance" is simply resubmitting the form. Kept as a
	// flag to let the UI render a specific hint.
	let emailNotVerified = $state<{ email: string } | null>(null);
	// One-shot "your session expired" banner. Triggered by the fetch
	// interceptor via `?source=session_expired`. Set to true only if
	// the query param is present on mount; the URL is stripped
	// immediately after so revisits / manual logouts don't re-show
	// the stale message.
	let sessionExpiredNotice = $state(false);
	// One-shot "logged out" success banner, distinct from the
	// session-expired one above. Triggered by AppShell::onLogout via
	// `?source=logged_out`. Consumed on mount (URL stripped) so the
	// notice never re-appears on reload.
	let loggedOutNotice = $state(false);
	// Also gates the existing-session probe below — after an explicit
	// logout we know the session is dead; probing would 401 → refresh
	// → 401 and clobber this landing with `?source=session_expired`
	// via the interceptor.
	let skipExistingSessionProbe = $state(false);
	// One-shot notice populated from ?login_error=<key> on mount.
	// Set by the OIDC callback's AutoLinkRefused redirect when the
	// IdP-returned email matches an existing local account but the
	// auto-link decision tree refused (verified=false, disabled by
	// config, or the local account is already linked to a different
	// identity). See docs/plan/oidc-account-linking.md § Auto-link.
	let loginErrorNotice = $state<string | null>(null);
	// Refs used by the mode-driven auto-focus effect. Bound with
	// `bind:this` on the first input of each mode's form so the effect
	// can focus the "primary" field each time the mode changes without
	// walking the DOM.
	let loginIdentifierInput = $state<HTMLInputElement | null>(null);
	let registerEmailInput = $state<HTMLInputElement | null>(null);
	let setupEmailInput = $state<HTMLInputElement | null>(null);
	// "Account created, follow the email link" banner. Set by the
	// register submit handler right before switching mode='login',
	// so the message stays on screen for the whole time the user is
	// looking at the login form (instead of vanishing on the register
	// form under a hard-to-read timeout). Cleared on the next
	// successful login OR when the user dismisses it.
	let postRegisterNotice = $state<string | null>(null);

	// The redirect target is an in-SPA destination (e.g. /files or a deep link a
	// guard bounced us from). It's user-supplied via the query string so its exact
	// value isn't a known route literal — cast to Pathname for resolve().
	const redirectTarget = $derived((page.url.searchParams.get('redirect') || '/files') as Pathname);
	const matchState = $derived(
		regConfirm.length === 0 ? '' : regPassword === regConfirm ? 'ok' : 'bad'
	);

	function csrfCookiePresent(): boolean {
		return document.cookie.split('; ').some((c) => c.startsWith('oxicloud_csrf='));
	}

	function onPwKey(e: KeyboardEvent) {
		capsOn = e.getModifierState?.('CapsLock') ?? false;
	}

	function onRegPwKey(e: KeyboardEvent) {
		regCapsOn = e.getModifierState?.('CapsLock') ?? false;
	}

	function onSetupPwKey(e: KeyboardEvent) {
		setupCapsOn = e.getModifierState?.('CapsLock') ?? false;
	}

	// Unified login submit. Two modes dispatched from ONE form:
	//   * password filled → POST /api/auth/login
	//   * password empty  → POST /api/auth/magic-link/send (backend
	//     accepts either a username or an email as identifier)
	// The `submitAsMagicLink` derived tracks which mode is active;
	// button label + hint text render off it.
	async function onLogin(e: SubmitEvent) {
		e.preventDefault();
		error = '';
		emailNotVerified = null;
		magicStatus = null;
		if (submitAsMagicLink) {
			await submitMagicLink();
			return;
		}
		busy = true;
		try {
			const data = await login(username, password);
			if (!csrfCookiePresent()) {
				error = t(
					'auth.cookie_rejected',
					'Login succeeded but the browser rejected the session cookie. If you are on HTTP, set OXICLOUD_COOKIE_SECURE=false or use HTTPS.'
				);
				return;
			}
			session.setUser(data.user);
			postRegisterNotice = null;
			// When the backend flags `force_password_change` the user's
			// current credential is an admin-set temporary password;
			// route them to the profile page (`?forcePasswordChange=1`
			// lights up an in-page banner) instead of the requested
			// destination. The redirect target is preserved as `next`
			// so the profile flow can bounce back after they pick a
			// real password.
			if (data.force_password_change) {
				const next = encodeURIComponent(redirectTarget);
				await goto(resolve(`/profile?forcePasswordChange=1&next=${next}`), {
					replaceState: true
				});
			} else {
				await goto(resolve(redirectTarget), { replaceState: true });
			}
		} catch (err) {
			if (err instanceof ApiError && err.errorType === 'EmailNotVerified') {
				// Server auto-sent a verification magic-link on the
				// piggyback-of-successful-password path (see the login
				// handler). Just tell the user; resubmitting the form
				// re-triggers the same auto-send.
				emailNotVerified = { email: username };
				error = t(
					'auth.email_not_verified',
					'Your email is not verified. We sent a verification link to your inbox — click it, then sign in again. If it did not arrive, submit the form again.'
				);
			} else if (err instanceof ApiError && err.errorType === 'PasswordLoginDisabled') {
				error = t(
					'auth.password_login_disabled',
					'Password login is disabled on this server. Leave the password blank to receive a sign-in link, or use SSO.'
				);
			} else {
				error = err instanceof Error ? err.message : t('auth.login_error', 'Error logging in');
			}
		} finally {
			busy = false;
		}
	}

	// Password-empty branch of the unified submit. Uses the same
	// `username` identifier the password form does — the backend
	// dispatches on `@` (username vs email). Anti-enum uniform 200.
	async function submitMagicLink() {
		if (!username) return;
		busy = true;
		try {
			const result = await sendMagicLink(username);
			magicStatus =
				result === 'sent'
					? {
							text: t(
								'auth.magic_sent',
								'If an account exists, a sign-in link has been sent. Check your inbox.'
							),
							ok: true
						}
					: {
							text: t(
								'auth.magic_unavailable',
								'Sign-in by email is not available on this server.'
							),
							ok: false
						};
		} catch {
			magicStatus = { text: t('auth.magic_error', 'Something went wrong. Try again.'), ok: false };
		} finally {
			busy = false;
		}
	}

	async function onRegister(e: SubmitEvent) {
		e.preventDefault();
		regError = '';
		if (regPassword !== regConfirm) {
			regError = t('auth.passwords_mismatch', 'Passwords do not match');
			return;
		}
		busy = true;
		try {
			// Username is optional since PR 18 — pass undefined when the
			// field is left blank so the backend keeps `username = None`
			// (the user can claim a handle later via profile settings).
			await register(regEmail, regPassword, regUsername.trim() || undefined);
			regUsername = regEmail = regPassword = regConfirm = '';
			// Move the success notice to the LOGIN screen so it's actually
			// readable — the register form is about to be replaced, so a
			// message shown here would flash and disappear.
			postRegisterNotice = t(
				'auth.account_success',
				'If the address is available, a confirmation email is on its way. Follow the link to finish.'
			);
			mode = 'login';
		} catch (err) {
			regError =
				err instanceof Error ? err.message : t('auth.register_error', 'Registration failed');
		} finally {
			busy = false;
		}
	}

	async function onSetup(e: SubmitEvent) {
		e.preventDefault();
		setupError = '';
		setupSuccess = '';
		if (setupPassword !== setupConfirm) {
			setupError = t('auth.passwords_mismatch', 'Passwords do not match');
			return;
		}
		busy = true;
		try {
			await setupAdmin(setupEmail, setupPassword);
			setupEmail = setupPassword = setupConfirm = '';
			// Admin now exists — fold the setup affordance away and return to login.
			setupAvailable = false;
			setupSuccess = t('auth.admin_success', 'Administrator created. You can now sign in.');
			setTimeout(() => {
				mode = 'login';
				setupSuccess = '';
			}, 2000);
		} catch (err) {
			setupError =
				err instanceof Error ? err.message : t('auth.admin_create_error', 'Setup failed');
		} finally {
			busy = false;
		}
	}

	// `?login_error=<key>` → localized copy lives in $lib/auth/loginError
	// (extracted so a Vitest can exercise the mapping in isolation).

	onMount(async () => {
		// 0) Consume the one-shot `?source=session_expired` flag, if any.
		//    Strip it from the URL so the banner never re-appears on
		//    reloads / manual logout redirects. Uses history.replaceState
		//    (no navigation, no scroll jump).
		const sourceParam = page.url.searchParams.get('source');
		if (sourceParam === 'session_expired' || sourceParam === 'logged_out') {
			if (sourceParam === 'session_expired') sessionExpiredNotice = true;
			else loggedOutNotice = true;
			// Either flag means we KNOW there's no live session — skip
			// the existing-session probe further down.
			skipExistingSessionProbe = true;
			const stripped = new URL(page.url);
			stripped.searchParams.delete('source');
			window.history.replaceState(
				window.history.state,
				'',
				stripped.pathname + stripped.search + stripped.hash
			);
		}

		// One-shot auto-link refusal notice. Reason key is a stable
		// snake_case identifier the OIDC callback emitted; map each to
		// localized copy and strip the param so a reload doesn't
		// re-surface the same notice.
		const loginErrorKey = page.url.searchParams.get('login_error');
		if (loginErrorKey) {
			loginErrorNotice = loginErrorMessage(loginErrorKey);
			const stripped = new URL(page.url);
			stripped.searchParams.delete('login_error');
			window.history.replaceState(
				window.history.state,
				'',
				stripped.pathname + stripped.search + stripped.hash
			);
		}

		// 1) OIDC code-exchange fallback: the IdP round-trip may land back here
		//    with ?oidc_code=. Exchange it for a session and redirect into the app.
		const oidcCode = page.url.searchParams.get('oidc_code');
		if (oidcCode) {
			const user = await exchangeOidcCode(oidcCode);
			if (user) {
				session.setUser(user);
				// OIDC callback creates the session UNBOUND (redirect flow can't
				// carry a JKT in the callback body). Post-redirect bind here
				// attaches the browser keypair — one-shot, idempotent (server
				// returns 409 if already bound). Awaited so subsequent requests
				// under `DPOP=required` land with a bound session, not an
				// unbound one that would 401 on the very next thumbnail. See
				// `docs/plan/dpop.md` Gate 3.
				await bindDpopIfPossible();
				await goto(resolve(redirectTarget), { replaceState: true });
				return;
			}
			// Exchange failed — fall through to the normal login UI.
		}

		// 2) Existing-session probe: if already authenticated, skip the form.
		//    Skipped when we KNOW the session is gone: explicit logout /
		//    interceptor-detected expiry (both set `skipExistingSessionProbe`),
		//    OR the CSRF hint cookie is absent (fresh browser, no cookies at
		//    all — a probe would just 401). Probing anyway would trip the
		//    apiFetch → 401 → refresh → 401 → sessionExpiredHandler chain
		//    that clobbers whatever notice we're about to paint.
		if (!skipExistingSessionProbe && hasSessionHint()) {
			try {
				const me = await fetchMe();
				if (me) {
					session.setUser(me);
					await goto(resolve(redirectTarget), { replaceState: true });
					return;
				}
			} catch {
				/* probe failed — show the login page */
			}
		}

		// 3) Bootstrap probe: a fresh install (no admin) must be set up first.
		const [providers, status] = await Promise.all([getOidcProviders(), getAuthStatus()]);
		oidc = providers;
		setupAvailable = !status.initialized;
		if (setupAvailable) mode = 'setup';

		// Auto-redirect to the IdP in standalone-OIDC posture is enforced
		// server-side via the `auto_redirect_if_standalone_oidc` auth policy
		// (see interfaces/web/mod.rs::oidc_standalone_login_redirect). Keeping
		// a client-side copy would make the policy toggle a no-op — the SPA
		// would auto-redirect regardless of what the admin configured.
		booting = false;
	});

	// Auto-focus the primary input for the current mode. Fires once the
	// booting probes settle AND on every mode swap. The `booting` guard
	// avoids stealing focus from something else during the loading
	// splash; the input-ref guard covers the render-order case where
	// the effect fires before the DOM has the target.
	//
	// `activeElement` guard: if the user (or Playwright's `.fill()`, or
	// browser autofill) already has focus in a form field, don't yank
	// it away. Concrete bug this prevents: boot probes are slow → user
	// types their email into the (initially unfocused) input → probes
	// finish → `booting` flips false → this effect fires and refocuses
	// the input, which resets the caret and can concatenate subsequent
	// keystrokes onto the wrong field if the user was mid-tab. Mode-
	// swap re-runs still refocus correctly because the old form's
	// inputs unmount first, resetting `activeElement` to `<body>`.
	$effect(() => {
		if (booting) return;
		const active = document.activeElement;
		if (
			active &&
			active !== document.body &&
			(active.tagName === 'INPUT' || active.tagName === 'TEXTAREA')
		) {
			return;
		}
		const target =
			mode === 'login'
				? loginIdentifierInput
				: mode === 'register'
					? registerEmailInput
					: setupEmailInput;
		target?.focus();
	});
</script>

<svelte:head>
	<title>{t('app.title', 'OxiCloud')}</title>
</svelte:head>

<div class="auth-container">
	<div class="auth-panel">
		<div class="auth-logo">
			<div class="auth-logo-icon">
				<svg viewBox="95 67 320 320" aria-hidden="true">
					<path
						d="M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z"
					/>
				</svg>
			</div>
			<div class="auth-logo-text"><span class="brand-oxi">Oxi</span>Cloud</div>
		</div>

		<!-- Form paints immediately alongside the logo — the onMount
			     probes (OIDC code exchange, session probe, providers
			     lookup) run concurrently and either redirect the user
			     away or upgrade the visible affordances (OIDC button,
			     magic-link toggle) in place. Guarding the whole form
			     behind `booting` caused a "logo only, then form" flash
			     on first paint. -->
		{#if loginErrorNotice}
			<!-- Dedicated error view — hides the login form entirely
			     until the user dismisses. Lands the user on a focused
			     "this went wrong" screen instead of a form buried
			     under a red banner. Sibling of the callback redirect
			     that surfaced this notice in the first place.
			     Reuses `.auth-title` and `.auth-button` for theme
			     consistency with the normal login/register/setup views. -->
			<div class="auth-error-view" role="alert" data-testid="login-error-notice">
				<h1 class="auth-title">{t('auth.login_error_title', 'Sign-in failed')}</h1>
				<p class="auth-error-view__message">{loginErrorNotice}</p>
				<button
					type="button"
					class="auth-button"
					data-testid="login-error-back-btn"
					onclick={() => (loginErrorNotice = null)}
				>
					{t('auth.login_error_back_to_login', 'Back to login')}
				</button>
			</div>
		{:else}
			<h1 class="auth-title">
				{#if mode === 'login'}
					{t('auth.sign_in', 'Sign in')}
				{:else if mode === 'register'}
					{t('auth.register', 'Create account')}
				{:else}
					{t('auth.setup_title', 'Initial setup')}
				{/if}
			</h1>

			{#if sessionExpiredNotice}
				<div
					class="auth-error auth-error--dismissible"
					style="display: flex"
					role="alert"
					data-testid="login-session-expired-notice"
				>
					<span>{t('auth.session_expired', 'Your session expired. Please sign in again.')}</span>
					<button
						type="button"
						class="auth-notice-dismiss"
						aria-label={t('common.dismiss', 'Dismiss')}
						data-testid="login-session-expired-dismiss-btn"
						onclick={() => (sessionExpiredNotice = false)}>×</button
					>
				</div>
			{/if}

			{#if loggedOutNotice}
				<div
					class="auth-success auth-error--dismissible"
					style="display: flex"
					role="status"
					data-testid="login-logged-out-notice"
				>
					<span>{t('auth.logged_out', 'Successfully signed out.')}</span>
					<button
						type="button"
						class="auth-notice-dismiss"
						aria-label={t('common.dismiss', 'Dismiss')}
						data-testid="login-logged-out-dismiss-btn"
						onclick={() => (loggedOutNotice = false)}>×</button
					>
				</div>
			{/if}

			{#if postRegisterNotice && mode === 'login'}
				<div
					class="auth-success auth-error--dismissible"
					style="display: flex"
					role="status"
					data-testid="login-post-register-notice"
				>
					<span>{postRegisterNotice}</span>
					<button
						type="button"
						class="auth-notice-dismiss"
						aria-label={t('common.dismiss', 'Dismiss')}
						data-testid="login-post-register-dismiss-btn"
						onclick={() => (postRegisterNotice = null)}>×</button
					>
				</div>
			{/if}

			{#if mode === 'login'}
				<!-- Unified login form. One identifier + one (optional)
				     password field drive both flows:
				       * password filled       → POST /api/auth/login
				       * password empty        → POST /api/auth/magic-link/send
				       * password-only server  → password field is required, no hint
				       * magic-link-only server → password field hides entirely -->
				{#if passwordLoginEnabled || magicLinkLoginEnabled}
					{#if error}
						<div
							class={emailNotVerified ? 'auth-success' : 'auth-error'}
							style="display: block"
							role="alert"
						>
							{error}
						</div>
					{/if}
					{#if magicStatus}
						<div
							class={magicStatus.ok
								? 'auth-status auth-status-success'
								: 'auth-status auth-status-error'}
							role={magicStatus.ok ? 'status' : 'alert'}
						>
							{magicStatus.text}
						</div>
					{/if}
					<form class="auth-form" data-testid="login-form" onsubmit={onLogin} novalidate>
						<div class="auth-input-group">
							<label class="auth-label" for="login-username">
								{t('auth.login_identifier', 'Username or email')}
							</label>
							<div class="auth-input-wrap auth-input-wrap--user">
								<input
									id="login-username"
									class="auth-input"
									data-testid="login-username-input"
									type="text"
									bind:value={username}
									bind:this={loginIdentifierInput}
									autocomplete="username"
									placeholder={t(
										'auth.login_identifier_placeholder',
										'Enter your username or email'
									)}
									required
									disabled={busy}
								/>
							</div>
						</div>

						{#if passwordLoginEnabled}
							<div class="auth-input-group">
								<label class="auth-label" for="login-password">
									{#if magicLinkLoginEnabled}
										{t('auth.password_or_link_hint', 'Password (leave blank for a sign-in link)')}
									{:else}
										{t('auth.password', 'Password')}
									{/if}
								</label>
								<div class="auth-input-wrap auth-input-wrap--lock has-toggle">
									<input
										id="login-password"
										class="auth-input"
										data-testid="login-password-input"
										type={showPassword ? 'text' : 'password'}
										bind:value={password}
										onkeydown={onPwKey}
										onkeyup={onPwKey}
										autocomplete="current-password"
										required={!magicLinkLoginEnabled}
										disabled={busy}
									/>
									<button
										type="button"
										class="auth-pw-toggle"
										aria-pressed={showPassword}
										data-testid="login-password-toggle-btn"
										aria-label={t('auth.toggle_password', 'Show password')}
										onclick={() => (showPassword = !showPassword)}
									></button>
								</div>
								{#if capsOn}
									<div class="auth-caps-warning">{t('auth.caps_lock', 'Caps Lock is on')}</div>
								{/if}
							</div>
						{/if}

						<button
							class="auth-button"
							type="submit"
							data-testid="login-submit-btn"
							disabled={busy}
							aria-busy={busy}
						>
							{#if busy}
								{submitAsMagicLink
									? t('auth.sending', 'Sending…')
									: t('auth.signing_in', 'Signing in…')}
							{:else if submitAsMagicLink}
								{t('auth.magicLinkSubmit', 'Send sign-in link')}
							{:else}
								{t('auth.sign_in', 'Sign in')}
							{/if}
						</button>
					</form>
				{/if}

				{#if oidc.enabled}
					{#if passwordLoginEnabled}
						<div class="auth-divider"><span>{t('auth.or', 'or')}</span></div>
					{/if}
					<!-- Backend OIDC authorize endpoint (not a SvelteKit route). -->
					<a
						class="auth-button auth-button-oidc"
						data-testid="login-oidc-btn"
						href={oidc.authorize_endpoint}
						rel="external"
					>
						{t(
							'auth.sso_login_provider',
							{ provider: oidc.provider_name ?? 'SSO' },
							'Sign in with {{provider}}'
						)}
					</a>
				{/if}

				{#if passwordLoginEnabled}
					<div class="auth-toggle">
						{t('auth.no_account', 'No account?')}
						<button
							class="auth-toggle-link"
							data-testid="login-to-register-btn"
							onclick={() => (mode = 'register')}
						>
							{t('auth.register', 'Create one')}
						</button>
					</div>
				{/if}

				{#if setupAvailable}
					<div class="auth-toggle">
						{t('auth.admin_setup', 'First time?')}
						<button
							class="auth-toggle-link"
							data-testid="login-to-setup-btn"
							onclick={() => (mode = 'setup')}
						>
							{t('auth.setup', 'Set up administrator')}
						</button>
					</div>
				{/if}
			{:else if mode === 'register'}
				{#if regError}<div class="auth-error" style="display: block" role="alert">
						{regError}
					</div>{/if}
				<form class="auth-form" data-testid="login-register-form" onsubmit={onRegister} novalidate>
					<!-- Email is the only required identifier since PR 18 — the
					     backend accepts email-only signup and mints a welcome
					     magic-link. Username is optional at this stage; the user
					     can claim a handle later via profile settings. -->
					<div class="auth-input-group">
						<label class="auth-label" for="reg-email">{t('auth.email', 'Email')}</label>
						<input
							id="reg-email"
							class="auth-input"
							data-testid="login-register-email-input"
							type="email"
							bind:value={regEmail}
							bind:this={registerEmailInput}
							autocomplete="email"
							required
							disabled={busy}
						/>
					</div>
					<div class="auth-input-group">
						<label class="auth-label" for="reg-username">
							{t('auth.username_optional', 'Username (optional)')}
						</label>
						<input
							id="reg-username"
							class="auth-input"
							data-testid="login-register-username-input"
							bind:value={regUsername}
							autocomplete="username"
							disabled={busy}
						/>
					</div>
					<!-- Password fields hide entirely when policy forbids password
					     login — the whole form becomes email-only in that mode. -->
					{#if passwordLoginEnabled}
						<div class="auth-input-group">
							<label class="auth-label" for="reg-password">
								{t(
									'auth.password_optional',
									'Password (optional — leave blank for a sign-in link)'
								)}
							</label>
							<div class="auth-input-wrap auth-input-wrap--lock has-toggle">
								<input
									id="reg-password"
									class="auth-input"
									data-testid="login-register-password-input"
									type={regShowPassword ? 'text' : 'password'}
									bind:value={regPassword}
									onkeydown={onRegPwKey}
									onkeyup={onRegPwKey}
									autocomplete="new-password"
									disabled={busy}
								/>
								<button
									type="button"
									class="auth-pw-toggle"
									aria-pressed={regShowPassword}
									data-testid="login-register-password-toggle-btn"
									aria-label={t('auth.toggle_password', 'Show password')}
									onclick={() => (regShowPassword = !regShowPassword)}
								></button>
							</div>
							{#if regCapsOn}
								<div class="auth-caps-warning">{t('auth.caps_lock', 'Caps Lock is on')}</div>
							{/if}
						</div>
						{#if !regEmailOnly}
							<div class="auth-input-group">
								<label class="auth-label" for="reg-confirm"
									>{t('auth.confirm_password', 'Confirm password')}</label
								>
								<div class="auth-input-wrap auth-input-wrap--lock has-toggle">
									<input
										id="reg-confirm"
										class="auth-input"
										data-testid="login-register-confirm-input"
										type={regShowConfirm ? 'text' : 'password'}
										bind:value={regConfirm}
										onkeydown={onRegPwKey}
										onkeyup={onRegPwKey}
										autocomplete="new-password"
										required
										disabled={busy}
									/>
									<button
										type="button"
										class="auth-pw-toggle"
										aria-pressed={regShowConfirm}
										data-testid="login-register-confirm-toggle-btn"
										aria-label={t('auth.toggle_password', 'Show password')}
										onclick={() => (regShowConfirm = !regShowConfirm)}
									></button>
								</div>
								{#if matchState}
									<div
										class="auth-match show {matchState === 'ok'
											? 'auth-match--ok'
											: 'auth-match--bad'}"
									>
										{matchState === 'ok'
											? t('auth.passwords_match', 'Passwords match')
											: t('auth.passwords_mismatch', "Passwords don't match")}
									</div>
								{/if}
							</div>
						{/if}
					{/if}
					<button
						class="auth-button"
						type="submit"
						data-testid="login-register-submit-btn"
						disabled={busy}
						aria-busy={busy}
					>
						{!passwordLoginEnabled || regEmailOnly
							? t('auth.register_email_only', 'Send me a sign-in link')
							: t('auth.register', 'Create account')}
					</button>
				</form>
				<div class="auth-toggle">
					{t('auth.have_account', 'Already have an account?')}
					<button
						class="auth-toggle-link"
						data-testid="login-register-to-login-btn"
						onclick={() => (mode = 'login')}
					>
						{t('auth.sign_in', 'Sign in')}
					</button>
				</div>
			{:else}
				<div class="setup-steps">
					<div class="setup-step">
						<div class="step-number active">1</div>
						<div class="step-title active">{t('auth.setup_step1', 'Admin')}</div>
					</div>
					<div class="setup-step">
						<div class="step-number">2</div>
						<div class="step-title">{t('auth.setup_step2', 'System')}</div>
					</div>
					<div class="setup-step">
						<div class="step-number">3</div>
						<div class="step-title">{t('auth.setup_step3', 'Completed')}</div>
					</div>
				</div>

				{#if setupError}<div class="auth-error" style="display: block" role="alert">
						{setupError}
					</div>{/if}
				{#if setupSuccess}<div class="auth-success" style="display: block">{setupSuccess}</div>{/if}

				<form class="auth-form" data-testid="login-setup-form" onsubmit={onSetup} novalidate>
					<div class="auth-input-group">
						<label class="auth-label" for="setup-username">
							{t('auth.admin_username', 'Administrator username')}
						</label>
						<div class="auth-input-wrap auth-input-wrap--user">
							<input
								id="setup-username"
								class="auth-input"
								data-testid="login-setup-username-input"
								type="text"
								value="admin"
								readonly
							/>
						</div>
					</div>

					<div class="auth-input-group">
						<label class="auth-label" for="setup-email">
							{t('auth.admin_email', 'Administrator email')}
						</label>
						<div class="auth-input-wrap auth-input-wrap--mail">
							<input
								id="setup-email"
								class="auth-input"
								data-testid="login-setup-email-input"
								type="email"
								bind:value={setupEmail}
								bind:this={setupEmailInput}
								autocomplete="email"
								required
								disabled={busy}
							/>
						</div>
					</div>

					<div class="auth-input-group">
						<label class="auth-label" for="setup-password">
							{t('auth.admin_password', 'Administrator password')}
						</label>
						<div class="auth-input-wrap auth-input-wrap--lock has-toggle">
							<input
								id="setup-password"
								class="auth-input"
								data-testid="login-setup-password-input"
								type={setupShowPassword ? 'text' : 'password'}
								bind:value={setupPassword}
								onkeydown={onSetupPwKey}
								onkeyup={onSetupPwKey}
								autocomplete="new-password"
								minlength="8"
								required
								disabled={busy}
							/>
							<button
								type="button"
								class="auth-pw-toggle"
								aria-pressed={setupShowPassword}
								data-testid="login-setup-password-toggle-btn"
								aria-label={t('auth.toggle_password', 'Show password')}
								onclick={() => (setupShowPassword = !setupShowPassword)}
							></button>
						</div>
						{#if setupCapsOn}
							<div class="auth-caps-warning">{t('auth.caps_lock', 'Caps Lock is on')}</div>
						{/if}
					</div>

					<div class="auth-input-group">
						<label class="auth-label" for="setup-confirm">
							{t('auth.confirm_password', 'Confirm password')}
						</label>
						<div class="auth-input-wrap auth-input-wrap--lock has-toggle">
							<input
								id="setup-confirm"
								class="auth-input"
								data-testid="login-setup-confirm-input"
								type={setupShowConfirm ? 'text' : 'password'}
								bind:value={setupConfirm}
								onkeydown={onSetupPwKey}
								onkeyup={onSetupPwKey}
								autocomplete="new-password"
								required
								disabled={busy}
							/>
							<button
								type="button"
								class="auth-pw-toggle"
								aria-pressed={setupShowConfirm}
								data-testid="login-setup-confirm-toggle-btn"
								aria-label={t('auth.toggle_password', 'Show password')}
								onclick={() => (setupShowConfirm = !setupShowConfirm)}
							></button>
						</div>
						{#if setupMatchState}
							<div
								class="auth-match show {setupMatchState === 'ok'
									? 'auth-match--ok'
									: 'auth-match--bad'}"
							>
								{setupMatchState === 'ok'
									? t('auth.passwords_match', 'Passwords match')
									: t('auth.passwords_mismatch', "Passwords don't match")}
							</div>
						{/if}
					</div>

					<button
						class="auth-button"
						type="submit"
						data-testid="login-setup-submit-btn"
						disabled={busy}
						aria-busy={busy}
					>
						{t('auth.create_admin', 'Create administrator')}
					</button>
				</form>

				<div class="auth-toggle">
					{t('auth.back_to_login', 'Already configured?')}
					<button
						class="auth-toggle-link"
						data-testid="login-setup-to-login-btn"
						onclick={() => (mode = 'login')}
					>
						{t('auth.sign_in', 'Sign in')}
					</button>
				</div>
			{/if}
		{/if}

		<div class="auth-lang">
			<select
				aria-label={t('settings.language', 'Language')}
				data-testid="login-language-select"
				value={i18n.locale}
				onchange={(e) => setLocale(e.currentTarget.value as Locale)}
			>
				{#each SUPPORTED_LOCALES as loc (loc)}
					<option value={loc}>{loc}</option>
				{/each}
			</select>
		</div>
	</div>
</div>

<style>
	.auth-lang {
		margin-top: var(--space-5);
		text-align: center;
	}

	.auth-lang select {
		padding: var(--space-1) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text-muted);
	}

	.auth-error--dismissible {
		align-items: center;
		gap: var(--space-2);
		justify-content: space-between;
	}

	.auth-notice-dismiss {
		background: transparent;
		border: 0;
		color: inherit;
		cursor: pointer;
		font-size: var(--font-size-lg);
		line-height: 1;
		padding: 0 var(--space-1);
	}

	.auth-notice-dismiss:hover {
		opacity: 0.7;
	}

	/* Dedicated error view — centered in the auth-panel with the
	   message padded off the title/button. `.auth-button` (from
	   ported/auth.css) already carries the full-width primary
	   styling used by the login submit, so the back button lands
	   right on the existing theme. */
	.auth-error-view {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-4);
		text-align: center;
	}

	.auth-error-view__message {
		color: var(--color-text-secondary);
		margin: 0;
	}
</style>
