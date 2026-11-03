<script lang="ts">
	import { avatarColorIndex, userInitials } from '$lib/utils/avatar';
	import { session } from '$lib/stores/session.svelte';

	let { large = false }: { large?: boolean } = $props();

	const photo = $derived(session.user?.image ?? null);
	const color = $derived(avatarColorIndex(session.user?.id));
	const initials = $derived(userInitials(session.user?.username || session.user?.email));
</script>

{#if photo}
	<img class="avatar avatar--photo" class:avatar--lg={large} src={photo} alt="" />
{:else}
	<span class="avatar avatar--c{color}" class:avatar--lg={large}>{initials}</span>
{/if}

<style>
	.avatar {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 36px;
		height: 36px;
		border-radius: 50%;
		background: var(--color-accent-gradient, var(--color-accent));
		color: var(--color-on-accent);
		font-size: var(--text-sm);
		font-weight: var(--weight-bold);
		flex-shrink: 0;
		overflow: hidden;
	}

	.avatar--photo {
		object-fit: cover;
	}

	/* Colour buckets mirror userVignette's .uv-color-0..4 shared palette. */
	.avatar--c0 {
		background: var(--color-badge-indigo-bg);
		color: var(--color-badge-indigo-text);
	}

	.avatar--c1 {
		background: var(--color-badge-green-bg);
		color: var(--color-badge-green-text);
	}

	.avatar--c2 {
		background: var(--color-badge-orange-bg);
		color: var(--color-badge-orange-text);
	}

	.avatar--c3 {
		background: var(--color-badge-blue-bg);
		color: var(--color-badge-blue-text);
	}

	.avatar--c4 {
		background: var(--color-badge-amber-bg);
		color: var(--color-badge-amber-text);
	}

	.avatar--lg {
		width: 44px;
		height: 44px;
		font-size: var(--text-base);
	}
</style>
