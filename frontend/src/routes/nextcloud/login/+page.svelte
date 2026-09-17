<script lang="ts">
	import { base } from '$app/paths';
	// Route-scoped auth styles (this page uses the .auth-* classes).
	import '$lib/styles/ported/auth.css';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { getOidcProviders } from '$lib/api/endpoints/auth';
	import { t } from '$lib/i18n/index.svelte';

	// Nextcloud Login Flow v2. The form does a NATIVE POST to the backend flow
	// endpoint so the server drives the redirect handshake — do not intercept it.
	// The flow token is hex; reject anything else to prevent action injection.
	const token = $derived(page.url.searchParams.get('token') ?? '');
	const validToken = $derived(/^[0-9a-fA-F]+$/.test(token));
	const formAction = $derived(`/login/v2/flow/${token}`);

	let oidcEnabled = $state(false);
	let oidcProvider = $state('SSO');
	let passwordLoginEnabled = $state(true);

	onMount(async () => {
		const info = await getOidcProviders();
		if (!info.enabled) return;
		oidcEnabled = true;
		oidcProvider = info.provider_name || 'SSO';
		passwordLoginEnabled = info.password_login_enabled !== false;
	});
</script>

<svelte:head><title>{t('app.title', 'OxiCloud')}</title></svelte:head>

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

		<h1 class="auth-title">{t('nextcloud.grant_title', 'Grant access')}</h1>
		<p class="auth-subtitle">
			{t('nextcloud.grant_subtitle', 'A Nextcloud client is requesting access to your account.')}
		</p>

		{#if !validToken}
			<div class="auth-error" style="display: block" role="alert">
				{t('nextcloud.invalid_token', 'Invalid session token.')}
			</div>
		{:else}
			{#if passwordLoginEnabled}
				<form
					class="auth-form"
					data-testid="nextcloud-login-form"
					method="post"
					action={formAction}
				>
					<div class="auth-input-group">
						<label class="auth-label" for="nc-user">{t('auth.username', 'Username or email')}</label
						>
						<div class="auth-input-wrap auth-input-wrap--user">
							<input
								id="nc-user"
								class="auth-input"
								data-testid="nextcloud-login-user-input"
								name="user"
								type="text"
								autocomplete="username"
								required
							/>
						</div>
					</div>
					<div class="auth-input-group">
						<label class="auth-label" for="nc-password">{t('auth.password', 'Password')}</label>
						<div class="auth-input-wrap auth-input-wrap--lock">
							<input
								id="nc-password"
								class="auth-input"
								data-testid="nextcloud-login-password-input"
								name="password"
								type="password"
								autocomplete="current-password"
								required
							/>
						</div>
					</div>
					<button class="auth-button" data-testid="nextcloud-login-grant-btn" type="submit"
						>{t('nextcloud.grant', 'Grant access')}</button
					>
				</form>
			{/if}

			{#if oidcEnabled}
				{#if passwordLoginEnabled}
					<div class="auth-divider"><span>{t('auth.or', 'or')}</span></div>
				{/if}
				<!-- Backend Nextcloud Login Flow v2 OIDC handshake (not a SvelteKit route). -->
				<a
					class="auth-button auth-button-sso"
					data-testid="nextcloud-login-sso-link"
					href={`${base}/login/v2/flow/${token}/oidc`}
					rel="external"
				>
					{t('nextcloud.sign_in_with', { provider: oidcProvider }, 'Sign in with {{provider}}')}
				</a>
			{/if}
		{/if}
	</div>
</div>
