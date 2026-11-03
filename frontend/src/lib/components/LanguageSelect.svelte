<script lang="ts">
	import { LANGUAGES } from '$lib/i18n/index.svelte';

	let {
		value,
		onchange,
		automaticLabel,
		label,
		testid
	}: {
		value: string;
		onchange: (value: string) => void;
		automaticLabel?: string;
		label?: string;
		testid?: string;
	} = $props();
</script>

<select
	{value}
	aria-label={label}
	data-testid={testid}
	onchange={(e) => onchange(e.currentTarget.value)}
>
	{#if automaticLabel !== undefined}
		<option value="">{automaticLabel}</option>
	{/if}
	{#each LANGUAGES as lang (lang.code)}
		<option value={lang.code}>{lang.flag} {lang.name}</option>
	{/each}
</select>

<style>
	select {
		padding: 0.5rem 0.625rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: 1rem;
	}
</style>
