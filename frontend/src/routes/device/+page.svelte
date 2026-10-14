<script lang="ts">
	// Route-scoped auth styles (this page uses the .auth-* classes).
	import '$lib/styles/ported/auth.css';
	import { errorMessage } from '$lib/utils/errors';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { deviceVerifyAction, deviceVerifyInfo } from '$lib/api/generated/openapi/sdk.gen';
	import type { DeviceVerifyInfoDto } from '$lib/api/generated/openapi/types.gen';
	import { t } from '$lib/i18n/index.svelte';

	// Distinguishable failure modes the page renders differently.
	// Lives on the component rather than in a shared module because
	// this is the only place these distinctions matter — the former
	// `endpoints/device.ts` wrapper is retired as part of the Phase 4
	// migration to the generated SDK.
	type DeviceLookupErrorKind = 'unauthorized' | 'not-found' | 'failed';
	class DeviceLookupFailure extends Error {
		kind: DeviceLookupErrorKind;
		constructor(kind: DeviceLookupErrorKind) {
			super(kind);
			this.kind = kind;
			this.name = 'DeviceLookupFailure';
		}
	}

	/**
	 * Look up a device user-code via the generated SDK. The backend returns
	 * HTTP 200 with `{valid:false}` for unknown/expired codes (NOT a non-2xx),
	 * so the body must be inspected — a 2xx alone does not mean the code is
	 * good. A 401 means the caller isn't signed in and must authenticate
	 * before authorizing a device.
	 */
	async function lookupDeviceCode(c: string): Promise<DeviceVerifyInfoDto> {
		let data: DeviceVerifyInfoDto;
		try {
			data = await deviceVerifyInfo({ throwOnError: true, query: { code: c } });
		} catch (err) {
			const status = (err as { response?: { status?: number } })?.response?.status;
			throw new DeviceLookupFailure(status === 401 ? 'unauthorized' : 'failed');
		}
		if (!data.valid) throw new DeviceLookupFailure('not-found');
		return data;
	}

	type Step = 'code' | 'loading' | 'review' | 'approved' | 'denied' | 'error';

	// A complete user-code is 8 chars + a hyphen (e.g. ABCD-1234) → length 9.
	const FULL_CODE_LENGTH = 9;

	let code = $state(page.url.searchParams.get('code') ?? '');
	let step = $state<Step>('code');
	let info = $state<DeviceVerifyInfoDto | null>(null);
	let errorText = $state('');
	let busy = $state(false);

	let debounceTimer: ReturnType<typeof setTimeout> | undefined;
	let codeInput = $state<HTMLInputElement | null>(null);

	function failureMessage(err: unknown): string {
		if (err instanceof DeviceLookupFailure) {
			switch (err.kind) {
				case 'unauthorized':
					return t(
						'device.unauthorized',
						'You must be logged in to authorize a device. Please log in first.'
					);
				case 'not-found':
					return t('device.not_found', 'Code not found or expired. Please check and try again.');
				default:
					return t('device.lookup_failed', 'Failed to verify code. Please try again.');
			}
		}
		return errorMessage(err);
	}

	async function lookup(e?: SubmitEvent) {
		e?.preventDefault();
		if (!code) return;
		step = 'loading';
		errorText = '';
		try {
			info = await lookupDeviceCode(code);
			step = 'review';
		} catch (err) {
			errorText = failureMessage(err);
			step = 'error';
		}
	}

	/**
	 * Normalise the code field as the user types: uppercase, strip anything
	 * that isn't [A-Z0-9-], auto-insert the hyphen after the first 4 chars,
	 * then debounce a lookup once a full code is present.
	 */
	function onCodeInput(e: Event) {
		const target = e.currentTarget as HTMLInputElement;
		let val = target.value.toUpperCase().replace(/[^A-Z0-9-]/g, '');
		if (val.length === 4 && !val.includes('-')) val = `${val}-`;
		code = val;
		// Reflect the normalised value back into the input.
		target.value = val;
		if (step === 'error') {
			step = 'code';
			errorText = '';
		}

		clearTimeout(debounceTimer);
		if (val.length >= FULL_CODE_LENGTH) {
			debounceTimer = setTimeout(() => void lookup(), 300);
		}
	}

	async function decide(action: 'approve' | 'deny') {
		busy = true;
		try {
			await deviceVerifyAction({
				throwOnError: true,
				body: { user_code: code, action }
			});
			step = action === 'approve' ? 'approved' : 'denied';
		} catch (err) {
			errorText = errorMessage(err);
			step = 'error';
		} finally {
			busy = false;
		}
	}

	function backToCode() {
		step = 'code';
		errorText = '';
		// Re-focus so the user can correct the code immediately.
		queueMicrotask(() => codeInput?.focus());
	}

	onMount(() => {
		if (code) void lookup();
		else codeInput?.focus();
	});
</script>

<svelte:head><title>{t('device.title', 'Device verification')} · OxiCloud</title></svelte:head>

<main class="device">
	<div class="device__card">
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
		<h1>{t('device.title', 'Device verification')}</h1>

		{#if step === 'code'}
			<form data-testid="device-code-form" onsubmit={lookup}>
				<label class="device__field">
					<span>{t('device.enter_code', 'Enter the code shown on your device')}</span>
					<input
						bind:this={codeInput}
						data-testid="device-code-input"
						value={code}
						oninput={onCodeInput}
						autocomplete="off"
						autocapitalize="characters"
						spellcheck="false"
						inputmode="text"
						maxlength={FULL_CODE_LENGTH}
					/>
				</label>
				<button type="submit" data-testid="device-continue-btn" disabled={!code}
					>{t('device.continue', 'Continue')}</button
				>
			</form>
		{:else if step === 'loading'}
			<p>{t('common.loading', 'Loading…')}</p>
		{:else if step === 'review'}
			<dl class="device__info">
				<dt>{t('device.client', 'Application')}</dt>
				<dd>{info?.client_name || t('device.unknown', 'Unknown')}</dd>
				<dt>{t('device.scopes', 'Access')}</dt>
				<dd>{info?.scopes || 'all'}</dd>
			</dl>
			<div class="device__actions">
				<button
					class="device__deny"
					data-testid="device-deny-btn"
					disabled={busy}
					onclick={() => decide('deny')}
				>
					{t('device.deny', 'Deny')}
				</button>
				<button
					class="device__approve"
					data-testid="device-approve-btn"
					disabled={busy}
					onclick={() => decide('approve')}
				>
					{t('device.approve', 'Approve')}
				</button>
			</div>
		{:else if step === 'approved'}
			<p class="device__ok">
				{t('device.approved', 'Device approved. You can return to your device.')}
			</p>
		{:else if step === 'denied'}
			<p>{t('device.denied', 'Device access denied.')}</p>
		{:else if step === 'error'}
			<p class="device__error" role="alert">{errorText}</p>
			<button data-testid="device-retry-btn" onclick={backToCode}
				>{t('common.retry', 'Try again')}</button
			>
		{/if}
	</div>
</main>

<style>
	.device {
		min-height: 100vh;
		display: grid;
		place-items: center;
		padding: 1rem;
		background: var(--color-bg-page);
	}

	.device__card {
		width: min(92vw, 24rem);
		display: flex;
		flex-direction: column;
		gap: 1rem;
		padding: 2rem;
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-lg);
	}

	.device__field {
		display: flex;
		flex-direction: column;
		gap: 0.375rem;
	}

	.device__field input {
		padding: 0.625rem 0.75rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: 1.25rem;
		letter-spacing: 0.1em;
		text-align: center;
	}

	.device__info {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 0.25rem 1rem;
		margin: 0;
	}

	.device__info dt {
		color: var(--color-text-muted);
	}

	.device__info dd {
		margin: 0;
		color: var(--color-text);
	}

	.device__actions {
		display: flex;
		justify-content: flex-end;
		gap: 0.5rem;
	}

	button {
		padding: 0.5rem 1rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.device__approve {
		background: var(--color-primary);
		color: var(--color-text-light);
		border-color: transparent;
	}

	.device__deny {
		color: var(--color-danger-text);
	}

	.device__ok {
		color: var(--color-success-text);
	}

	.device__error {
		color: var(--color-danger-text);
	}
</style>
