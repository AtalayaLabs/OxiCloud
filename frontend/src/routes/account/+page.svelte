<script lang="ts">
	import AboutDialog from '$lib/components/AboutDialog.svelte';
	import UserMenuContent from '$lib/components/UserMenuContent.svelte';
	import { t } from '$lib/i18n/index.svelte';

	let aboutOpen = $state(false);
</script>

<svelte:head><title>{t('nav.account', 'Account')} · OxiCloud</title></svelte:head>

<div class="account-page">
	<h1 class="page-title">{t('nav.account', 'Account')}</h1>
	<div class="account-card">
		<UserMenuContent onabout={() => (aboutOpen = true)} />
	</div>
</div>

<AboutDialog bind:open={aboutOpen} />

<style>
	.account-page {
		max-width: 34rem;
		margin: 0 auto;
	}

	.account-card {
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-3xl);
		overflow: hidden;
	}
</style>
