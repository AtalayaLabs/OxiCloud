<script lang="ts">
	import Modal from '$lib/components/Modal.svelte';
	import { dialogs } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';

	// Local input value for prompt dialogs; reset whenever a new prompt opens.
	let value = $state('');
	let values = $state<Record<string, string>>({});
	let lastId: object | null = null;
	let inputEl = $state<HTMLInputElement | null>(null);

	$effect(() => {
		const c = dialogs.current;
		if (c && c !== lastId) {
			lastId = c;
			value = c.kind === 'prompt' ? (c.opts.defaultValue ?? '') : '';
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
</script>

{#if dialogs.current}
	{@const c = dialogs.current}
	<Modal {open} title={c.opts.title} onclose={() => dialogs.cancel()}>
		{#if c.kind === 'prompt'}
			<form id="dialog-form" onsubmit={submit}>
				{#if c.opts.message}<p class="dlg-msg">{c.opts.message}</p>{/if}
				<input
					class="dlg-input"
					data-testid="dialog-host-prompt-input"
					type="text"
					bind:this={inputEl}
					bind:value
					placeholder={c.opts.placeholder ?? ''}
					autocomplete="off"
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
			<p class="dlg-error" role="alert">{dialogs.error}</p>
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

	.dlg-error {
		margin: var(--space-3) 0 0;
		color: var(--color-danger-text);
		font-size: var(--text-sm);
	}
</style>
