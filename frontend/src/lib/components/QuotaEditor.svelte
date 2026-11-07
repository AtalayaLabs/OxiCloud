<script lang="ts">
	// Shared quota edit modal for the two admin surfaces that mutate a
	// storage cap:
	//
	//   * User envelope — `PUT /api/admin/users/{id}/quota` (0 on the
	//     wire = unlimited, per the backend's `check_storage_quota`
	//     `quota <= 0` short-circuit).
	//   * Shared drive  — `PATCH /api/drives/{id}/quota` (`null` on
	//     the wire = unlimited; the service also normalises 0/negative
	//     to unlimited defensively).
	//
	// The wire encodings differ; the UX should not. This component
	// reuses the pre-refactor user-modal layout (single input + unit
	// dropdown + "0 for unlimited" hint) so the two surfaces share
	// their i18n keys (`admin.quota_for`, `admin.quota_label`,
	// `admin.quota_unlimited_hint`, `common.cancel`, `common.save`).
	// The save callback receives an explicit `unlimited` boolean and
	// a positive `bytes` count — the caller encodes for its own
	// endpoint (0 for users, null for drives) so the magic value
	// never spreads into the UI or shared component.

	import { t } from '$lib/i18n/index.svelte';
	import Modal from '$lib/components/Modal.svelte';

	interface Props {
		open: boolean;
		/** Modal title (e.g. "Edit quota"). */
		title: string;
		/**
		 * Human-readable label of the subject whose quota is being
		 * edited — a username, or a drive name. Rendered as
		 * "Quota for **{subjectName}**".
		 */
		subjectName: string;
		/**
		 * Current cap in bytes. `null` or `0` both render as `0`
		 * in the input (which the "0 = unlimited" hint labels as
		 * unlimited) — matches both endpoint conventions.
		 */
		initialBytes: number | null;
		/** Disables inputs + swaps the primary button to "Saving…". */
		busy?: boolean;
		error?: string | null;
		onclose: () => void;
		/**
		 * Called on Save. `unlimited: true` (input was `0` or
		 * negative) → the caller should send whatever "unlimited"
		 * means to its endpoint (0 for users, `null` for drives).
		 * `unlimited: false` → `bytes` is the caller's positive
		 * integer to persist verbatim.
		 */
		onsave: (result: { unlimited: boolean; bytes: number }) => void;
		/** data-testid prefix so both call-sites get stable selectors. */
		testIdPrefix?: string;
	}

	const QUOTA_UNITS = [
		{ value: 1024 ** 2, label: 'MB' },
		{ value: 1024 ** 3, label: 'GB' },
		{ value: 1024 ** 4, label: 'TB' }
	] as const;

	let {
		open,
		title,
		subjectName,
		initialBytes,
		busy = false,
		error = null,
		onclose,
		onsave,
		testIdPrefix = 'quota'
	}: Props = $props();

	// Local draft state — the parent owns `initialBytes` and can pass
	// a new value on every open; `$effect` resets the draft whenever
	// `open` transitions to true so a re-open shows fresh state
	// instead of the last edit.
	let value = $state(0);
	let unit = $state<number>(1024 ** 3);
	let wasOpen = $state(false);

	$effect(() => {
		if (open && !wasOpen) {
			const bytes = initialBytes ?? 0;
			// Pick the largest unit that yields a value >= 1 so the
			// number stays readable; fall back to GB for the
			// unlimited case so the form is filled with a sane default.
			if (bytes >= 1024 ** 4) {
				unit = 1024 ** 4;
			} else if (bytes >= 1024 ** 3 || bytes === 0) {
				unit = 1024 ** 3;
			} else {
				unit = 1024 ** 2;
			}
			value = bytes > 0 ? Math.round((bytes / unit) * 10) / 10 : 0;
		}
		wasOpen = open;
	});

	function submit(e: Event) {
		e.preventDefault();
		if (value <= 0) {
			onsave({ unlimited: true, bytes: 0 });
		} else {
			onsave({ unlimited: false, bytes: Math.round(value * unit) });
		}
	}
</script>

<Modal {open} {title} {onclose}>
	<form id="quota-editor-form" class="form" data-testid={`${testIdPrefix}-form`} onsubmit={submit}>
		<p class="muted">
			{t('admin.quota_for', 'Quota for')} <strong>{subjectName}</strong>
		</p>
		<label>
			<span>{t('admin.quota_label', 'Quota')}</span>
			<div class="quota-input">
				<input
					type="number"
					data-testid={`${testIdPrefix}-value-input`}
					min="0"
					step="0.1"
					bind:value
					disabled={busy}
				/>
				<select bind:value={unit} data-testid={`${testIdPrefix}-unit-select`} disabled={busy}>
					{#each QUOTA_UNITS as u (u.label)}
						<option value={u.value}>{u.label}</option>
					{/each}
				</select>
			</div>
			<span class="muted">{t('admin.quota_unlimited_hint', 'Set to 0 for unlimited')}</span>
		</label>
		{#if error}
			<p class="status--error">{error}</p>
		{/if}
	</form>
	{#snippet footer()}
		<button
			class="btn btn-secondary"
			type="button"
			data-testid={`${testIdPrefix}-cancel-btn`}
			onclick={onclose}
			disabled={busy}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<button
			class="btn btn-primary"
			type="submit"
			form="quota-editor-form"
			data-testid={`${testIdPrefix}-save-btn`}
			disabled={busy}
		>
			{busy ? t('common.saving', 'Saving…') : t('common.save', 'Save')}
		</button>
	{/snippet}
</Modal>

<style>
	/* Svelte 5 scopes styles per component, and CSS classes used in
	   this file's markup carry this component's hash. The local
	   `.form` / `.quota-input` / `.muted` / `.status--error` rules on
	   `/admin/+page.svelte` don't reach across; mirror them here so
	   the QuotaEditor visually matches every other modal in the admin
	   panel. Buttons use the global `.btn` classes. */
	.form {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.form label {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		font-size: 0.875rem;
	}

	.form input,
	.form select {
		padding: 0.5rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.quota-input {
		display: flex;
		gap: var(--space-2);
	}

	.quota-input input,
	.quota-input select {
		flex: 1 1 0;
		min-width: 0;
	}

	.muted {
		color: var(--color-text-muted);
		font-size: 0.8125rem;
	}

	.status--error {
		color: var(--color-error-text);
	}
</style>
