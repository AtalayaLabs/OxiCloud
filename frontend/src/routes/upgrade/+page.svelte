<script lang="ts">
	// Route-scoped auth styles: same visual language as /login so the
	// upgrade page reads as an account action, not a settings mini-form.
	import '$lib/styles/ported/auth.css';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { onMount } from 'svelte';
	import { ApiError } from '$lib/api/client';
	import { getOidcProviders, upgradeToInternal, type OidcProviders } from '$lib/api/endpoints/auth';
	import { t } from '$lib/i18n/index.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';
	import { session } from '$lib/stores/session.svelte';

	let password = $state('');
	let confirm = $state('');
	let busy = $state(false);
	let error = $state('');
	let successHint = $state('');
	// Provider info tells us whether magic-link login is available.
	// `passwordRequired = !magic_link_login_enabled` — if the deployment
	// doesn't offer magic-link login, an upgraded user with no password
	// would be stuck (no login path), so the form requires one.
	let providers = $state<OidcProviders>({ enabled: false });
	const passwordRequired = $derived(providers.magic_link_login_enabled !== true);
	const matchState = $derived(confirm.length === 0 ? '' : password === confirm ? 'ok' : 'bad');

	onMount(async () => {
		// Guard: only external users have a meaningful upgrade path. Non-
		// externals bounce straight to /files — no error message needed,
		// this URL isn't a discoverable page for internal users.
		if (session.user && session.user.is_external !== true) {
			await goto(resolve('/files'), { replaceState: true });
			return;
		}
		providers = await getOidcProviders();
	});

	async function onSubmit(e: SubmitEvent) {
		e.preventDefault();
		error = '';
		successHint = '';
		if (passwordRequired && password.length === 0) {
			error = t(
				'upgrade.password_required',
				'Password is required — this deployment does not offer email-link login.'
			);
			return;
		}
		if (password.length > 0 && password !== confirm) {
			error = t('auth.passwords_mismatch', 'Passwords do not match');
			return;
		}
		// Client-side length gate against the server's advertised
		// `AuthConfig::min_password_length` — same rule the register /
		// setup / change-password paths honour.
		const minLen = serverConfig.auth.min_password_length;
		if (password.length > 0 && password.length < minLen) {
			error = t(
				'auth.password_too_short',
				{ min: String(minLen) },
				'Password must be at least {{min}} characters long'
			);
			return;
		}
		busy = true;
		try {
			// Server accepts `password` as optional when magic-link login
			// is on. `undefined` means "no password" — user stays magic-
			// link-only after the upgrade.
			const updated = await upgradeToInternal(password.length > 0 ? password : undefined);
			session.setUser(updated);
			successHint = t(
				'upgrade.success',
				'Your account has been upgraded. Redirecting to your files…'
			);
			// Small pause so the success line is visible, then jump into
			// the app on the freshly-provisioned home drive.
			setTimeout(() => goto(resolve('/files'), { replaceState: true }), 800);
		} catch (err) {
			if (err instanceof ApiError && err.errorType === 'AlreadyInternal') {
				// Race: another tab or session already upgraded. Treat as
				// success and just navigate into the app.
				await goto(resolve('/files'), { replaceState: true });
				return;
			}
			if (err instanceof ApiError && err.errorType === 'ManagedByIdP') {
				error = t(
					'upgrade.oidc_user',
					'SSO/OIDC accounts are managed by your identity provider. Upgrade is not available.'
				);
			} else if (err instanceof ApiError && err.errorType === 'RegistrationDomainNotAllowed') {
				error = t(
					'upgrade.domain_not_allowed',
					'This deployment does not accept new accounts from your email domain. Contact the administrator to enable it.'
				);
			} else if (err instanceof ApiError && err.errorType === 'PasswordRequired') {
				error = t(
					'upgrade.password_required',
					'Password is required — this deployment does not offer email-link login.'
				);
			} else {
				error = err instanceof Error ? err.message : t('upgrade.error', 'Upgrade failed.');
			}
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>{t('upgrade.title', 'Upgrade to a full account')}</title>
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

		<h1 class="auth-title">{t('upgrade.title', 'Upgrade to a full account')}</h1>
		<p class="auth-subtitle">
			{t(
				'upgrade.lede',
				'Get your own storage and start uploading files. Your existing shares stay untouched.'
			)}
		</p>

		{#if successHint}
			<div class="auth-success" style="display: block" role="status">{successHint}</div>
		{/if}
		{#if error}
			<div class="auth-error" style="display: block" role="alert">{error}</div>
		{/if}

		<form class="auth-form" data-testid="upgrade-form" onsubmit={onSubmit} novalidate>
			<div class="auth-input-group">
				<label class="auth-label" for="upgrade-password">
					{passwordRequired
						? t('auth.password', 'Password')
						: t('auth.password_optional', 'Password (optional)')}
				</label>
				<div class="auth-input-wrap auth-input-wrap--lock">
					<input
						id="upgrade-password"
						class="auth-input"
						data-testid="upgrade-password-input"
						type="password"
						bind:value={password}
						autocomplete="new-password"
						required={passwordRequired}
						disabled={busy}
					/>
				</div>
			</div>

			{#if password.length > 0}
				<div class="auth-input-group">
					<label class="auth-label" for="upgrade-confirm">
						{t('auth.confirm_password', 'Confirm password')}
					</label>
					<div class="auth-input-wrap auth-input-wrap--lock">
						<input
							id="upgrade-confirm"
							class="auth-input"
							data-testid="upgrade-confirm-input"
							type="password"
							bind:value={confirm}
							autocomplete="new-password"
							required
							disabled={busy}
						/>
					</div>
					{#if matchState}
						<div
							class="auth-match show {matchState === 'ok' ? 'auth-match--ok' : 'auth-match--bad'}"
						>
							{matchState === 'ok'
								? t('auth.passwords_match', 'Passwords match')
								: t('auth.passwords_mismatch', "Passwords don't match")}
						</div>
					{/if}
				</div>
			{/if}

			<button
				class="auth-button"
				type="submit"
				data-testid="upgrade-submit-btn"
				disabled={busy}
				aria-busy={busy}
			>
				{busy ? t('upgrade.busy', 'Upgrading…') : t('upgrade.submit', 'Upgrade my account')}
			</button>
		</form>

		<div class="auth-toggle">
			<button
				class="auth-toggle-link"
				data-testid="upgrade-cancel-btn"
				type="button"
				onclick={() => goto(resolve('/shared-with-me'), { replaceState: true })}
			>
				{t('upgrade.cancel', 'Not now — back to shared with me')}
			</button>
		</div>
	</div>
</div>
