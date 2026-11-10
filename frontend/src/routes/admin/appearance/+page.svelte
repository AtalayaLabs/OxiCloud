<script lang="ts">
	// A separate route keeps appearance state out of the existing large admin page.
	import { onMount } from 'svelte';
	import { getAppearance, selectTheme, reloadThemes } from '$lib/api/endpoints/admin';
	import type { AppearanceDto } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import Button from '$lib/components/Button.svelte';
	import { useAsyncAction } from '$lib/composables/useAsyncAction.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { errorMessage } from '$lib/utils/errors';

	function themeName(id: string): string {
		return id === 'modern-pro' ? 'Modern Pro' : id;
	}

	let appearance = $state<AppearanceDto | null>(null);
	let selected = $state('');
	let loadError = $state<string | null>(null);
	let saved = $state(false);
	let refreshed = $state(false);
	const action = useAsyncAction();
	const forcedDefault = $derived(appearance?.warning === 'forced_default');
	async function load() {
		loadError = null;
		try {
			appearance = await getAppearance();
			selected = appearance.active_theme_id;
		} catch (error) {
			loadError = errorMessage(error);
		}
	}
	async function refresh() {
		saved = false;
		refreshed = false;
		await action.run(async () => {
			const next = await reloadThemes();
			appearance = next;
			if (!next.available_themes.includes(selected)) selected = next.active_theme_id;
			refreshed = true;
		});
	}

	onMount(() => {
		void load();
	});
	async function save(event: SubmitEvent) {
		event.preventDefault();
		saved = false;
		await action.run(async () => {
			appearance = await selectTheme(selected);
			saved = true;
		});
	}
</script>

<svelte:head><title>{t('admin.appearance.title', 'Appearance')} · OxiCloud</title></svelte:head>
<div class="appearance">
	<div class="page-sticky-header">
		<div class="actions-bar">
			<h1 class="page-title">
				{t('admin.title', 'Admin')} › {t('admin.appearance.title', 'Appearance')}
			</h1>
		</div>
	</div>
	{#if loadError}
		<p role="alert" class="form-error">{loadError}</p>
		<Button onclick={() => void load()}>{t('common.retry', 'Retry')}</Button>
	{:else if appearance}
		<form class="appearance__form" onsubmit={save}>
			<p>
				{t(
					'admin.appearance.description',
					'Choose the appearance for everyone on this instance. Light, dark and system mode remain individual preferences.'
				)}
			</p>
			<p>
				{t(
					'admin.appearance.active',
					{ theme: themeName(appearance.active_theme_id) },
					'Active theme: {{theme}}'
				)}
			</p>
			{#if forcedDefault}
				<p role="status">
					{t(
						'admin.appearance.recovery',
						'The operator has forced the built-in theme for recovery. The saved selection is preserved.'
					)}
				</p>
			{:else if appearance.warning}
				<p role="status">
					{t(
						'admin.appearance.unavailable',
						{ theme: appearance.selected_theme_id ?? '' },
						'The saved theme “{{theme}}” is unavailable. The built-in default is being used.'
					)}
				</p>
			{/if}
			<label for="appearance-theme">{t('admin.appearance.theme', 'Global theme')}</label>
			<div class="appearance__picker">
				<div class="appearance__select">
					<select
						class="appearance__control"
						id="appearance-theme"
						bind:value={selected}
						disabled={action.busy || forcedDefault}
						onchange={() => {
							action.clearError();
							saved = false;
						}}
						aria-invalid={action.error ? 'true' : undefined}
						aria-describedby={action.error ? 'appearance-error' : undefined}
					>
						{#each appearance.available_themes as id (id)}<option value={id}>{themeName(id)}</option
							>{/each}
					</select>
					<span class="appearance__chevron" aria-hidden="true"><Icon name="chevron-down" /></span>
				</div>
				<Button onclick={() => void refresh()} disabled={action.busy}
					>{t('admin.appearance.refresh', 'Refresh')}</Button
				>
			</div>
			{#if refreshed}<p role="status">
					{t(
						'admin.appearance.refreshed',
						'Theme files reloaded. Reload the page to apply CSS changes.'
					)}
				</p>{/if}
			{#if action.error}<p id="appearance-error" role="alert" class="form-error">
					{action.error}
				</p>{/if}
			<Button type="submit" variant="primary" disabled={action.busy || forcedDefault}
				>{t('common.save', 'Save')}</Button
			>
			{#if saved}<p role="status">
					{t('admin.appearance.saved', 'Saved. The theme will apply on the next full page load.')}
				</p>{/if}
			{#if appearance.theme_diagnostics.length}
				<p>
					{t(
						'admin.appearance.invalid',
						'Some installed themes were excluded. Check the server logs for details.'
					)}
				</p>
				<ul>
					{#each appearance.theme_diagnostics as diagnostic, index (diagnostic.theme_id ?? index)}<li
						>
							{diagnostic.theme_id ?? '—'}: {diagnostic.reason}
						</li>{/each}
				</ul>
			{/if}
		</form>
	{:else}<p role="status">{t('common.loading', 'Loading…')}</p>{/if}
</div>

<style>
	.appearance {
		padding: var(--space-4);
	}

	.appearance__form {
		display: grid;
		gap: var(--space-4);
		max-width: var(--measure-prose);
	}

	.appearance__picker {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.appearance__select {
		flex: 1;
		min-width: 0;
		position: relative;
	}

	.appearance__control {
		appearance: none;
		width: 100%;
		min-height: var(--density-control-h);
		padding: var(--space-3) var(--space-10) var(--space-3) var(--space-4);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-bg-surface);
		color: var(--color-text);
		font: inherit;
		cursor: pointer;
	}

	.appearance__control:hover {
		border-color: var(--color-border-medium);
	}

	.appearance__control:disabled {
		cursor: wait;
		opacity: 0.6;
	}

	.appearance__chevron {
		position: absolute;
		inset-inline-end: var(--space-4);
		top: 50%;
		transform: translateY(-50%);
		color: var(--color-text-muted);
		pointer-events: none;
	}
</style>
