<script lang="ts">
	import Modal from '$lib/components/Modal.svelte';
	import { dialogs } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';

	// Local input value for prompt dialogs; reset whenever a new prompt opens.
	let value = $state('');
	let values = $state<Record<string, string>>({});
	let lastId: object | null = null;
	let inputEl = $state<HTMLInputElement | null>(null);
	// Track the last error message so we only re-focus the input on a
	// NEW error (not on every re-render while the error stays set).
	let lastErrorSeen: string | null = null;

	// `$effect.pre`, NOT `$effect` — and the difference is a bug, not a
	// preference. A plain `$effect` runs AFTER the DOM update, so a newly
	// opened dialog painted its inputs bound to the PREVIOUS dialog's
	// `values` object, and only then did this block replace `values`
	// wholesale and write back over the fields. Anything that typed inside
	// that window had its text overwritten or misplaced: the rename form's
	// description text landed in the NAME field, which shipped as a playlist
	// renamed to `<name><description>`.
	//
	// It is a wide window, not a rare interleaving — `music.spec.ts`'s
	// "playlist lifecycle" test failed 16 times in 20 runs under
	// `--repeat-each=20`, and once in a CI run of the full suite. A human
	// hits it by typing within a frame of the dialog opening, which is
	// unlikely but not impossible on a fast machine.
	//
	// `$effect.pre` runs BEFORE the DOM update, so the inputs' first paint
	// already carries the right values and nothing clobbers them afterwards.
	// That closes the window rather than narrowing it. The
	// `requestAnimationFrame` block below still defers past paint on
	// purpose: it needs `inputEl`, which does not exist until after.
	$effect.pre(() => {
		const c = dialogs.current;
		if (c && c !== lastId) {
			lastId = c;
			value = c.kind === 'prompt' ? (c.opts.defaultValue ?? '') : '';
			lastErrorSeen = null;
			values =
				c.kind === 'form'
					? Object.fromEntries(c.opts.fields.map((f) => [f.key, f.value ?? '']))
					: {};
			if (c.kind === 'prompt' && c.opts.selectOnOpen) {
				const select = c.opts.selectOnOpen;
				requestAnimationFrame(() => {
					const el = inputEl;
					if (!el) return;
					el.focus();
					if (select === 'name') {
						// Select the filename stem only — leave the extension
						// untouched so a rename replaces just the name.
						const dot = value.lastIndexOf('.');
						const end = dot > 0 ? dot : value.length;
						el.setSelectionRange(0, end);
					} else {
						el.select();
					}
				});
			}
		}
	});

	// Nice-to-have 2: on a FRESH error (dialog kept open after a failed
	// action), re-focus the input so the user can retype immediately
	// without reaching for the mouse. `busy = false` is set BEFORE
	// `error` on the store, so by the time this effect runs the input
	// is re-enabled and focusable. Guarded by `lastErrorSeen` so a
	// stable error doesn't steal focus on every re-render.
	$effect(() => {
		const err = dialogs.error;
		if (err && err !== lastErrorSeen && dialogs.current?.kind === 'prompt') {
			lastErrorSeen = err;
			requestAnimationFrame(() => inputEl?.focus());
		} else if (!err) {
			lastErrorSeen = null;
		}
	});

	const open = $derived(dialogs.current !== null);

	function submit(e?: SubmitEvent) {
		e?.preventDefault();
		const c = dialogs.current;
		if (!c || dialogs.busy) return;
		// `resolve` runs any async action and keeps the dialog open on failure.
		if (c.kind === 'prompt') void dialogs.resolve(value);
		else if (c.kind === 'form') void dialogs.resolve(values);
		else void dialogs.resolve(true);
	}

	// Nice-to-have 1: clear the inline error as soon as the user starts
	// editing. A stale "already exists" message hanging over the input
	// while the user types a fix is visual noise — the submit will
	// re-run the action anyway, so the error either returns (new value
	// still bad) or disappears (good value). Keeps the dialog quieter.
	function oninputChange() {
		if (dialogs.error) dialogs.error = null;
	}
</script>

{#if dialogs.current}
	{@const c = dialogs.current}
	<Modal {open} title={c.opts.title} onclose={() => dialogs.cancel()}>
		{#if c.kind === 'prompt'}
			<form id="dialog-form" onsubmit={submit}>
				{#if c.opts.message}<p class="dlg-msg">{c.opts.message}</p>{/if}
				<input
					class="dlg-input"
					class:form-input--invalid={!!dialogs.error}
					data-testid="dialog-host-prompt-input"
					type="text"
					bind:this={inputEl}
					bind:value
					oninput={oninputChange}
					placeholder={c.opts.placeholder ?? ''}
					autocomplete="off"
					aria-invalid={dialogs.error ? 'true' : undefined}
					aria-describedby={dialogs.error ? 'dialog-host-error' : undefined}
					disabled={dialogs.busy}
				/>
			</form>
		{:else if c.kind === 'form'}
			<form id="dialog-form" onsubmit={submit}>
				{#if c.opts.message}<p class="dlg-msg">{c.opts.message}</p>{/if}
				{#each c.opts.fields as field (field.key)}
					<label>
						<span>{field.label}</span>
						{#if field.multiline}
							<textarea
								rows="3"
								data-testid={`dialog-host-input-${field.key}`}
								bind:value={values[field.key]}
								placeholder={field.placeholder ?? ''}
								disabled={dialogs.busy}></textarea>
						{:else}
							<input
								type="text"
								data-testid={`dialog-host-input-${field.key}`}
								bind:value={values[field.key]}
								placeholder={field.placeholder ?? ''}
								autocomplete="off"
								disabled={dialogs.busy}
							/>
						{/if}
					</label>
				{/each}
			</form>
		{:else if c.opts.message}
			<p class="dlg-msg">{c.opts.message}</p>
		{/if}

		{#if dialogs.error}
			<p class="form-error" id="dialog-host-error" role="alert">{dialogs.error}</p>
		{/if}

		{#snippet footer()}
			<button
				class="btn btn-secondary"
				data-testid="dialog-host-cancel-btn"
				disabled={dialogs.busy}
				onclick={() => dialogs.cancel()}
			>
				{c.opts.cancelText ?? t('common.cancel', 'Cancel')}
			</button>
			{#if c.kind === 'confirm'}
				<button
					class="btn {c.opts.danger ? 'btn-danger' : 'btn-primary'}"
					data-testid="dialog-host-confirm-btn"
					disabled={dialogs.busy}
					onclick={() => dialogs.resolve(true)}
				>
					{dialogs.busy
						? t('common.loading', 'Loading…')
						: (c.opts.confirmText ?? t('common.ok', 'OK'))}
				</button>
			{:else}
				<button
					class="btn btn-primary"
					data-testid="dialog-host-submit-btn"
					type="submit"
					form="dialog-form"
					disabled={dialogs.busy}
				>
					{dialogs.busy
						? t('common.loading', 'Loading…')
						: (c.opts.confirmText ?? t('common.ok', 'OK'))}
				</button>
			{/if}
		{/snippet}
	</Modal>
{/if}

<style>
	.dlg-msg {
		margin: 0 0 var(--space-3);
		color: var(--color-text);
	}

	.dlg-input {
		width: 100%;
		padding: var(--space-2-5) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: var(--text-base);
	}

	/* `.form-error` and `.form-input--invalid` live in
	   `lib/styles/base/forms.css` — one place for every dialog's
	   error state. See `frontend/AGENTS.md § Dialog error UX`. */
</style>
