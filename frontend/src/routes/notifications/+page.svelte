<script lang="ts">
	import { onMount } from 'svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import NotificationRow from '$lib/components/NotificationRow.svelte';
	import TransientNotificationRow from '$lib/components/TransientNotificationRow.svelte';
	import { notifications } from '$lib/composables/useNotifications.svelte';
	import { ui, type Notification as TransientNotification } from '$lib/stores/ui.svelte';
	import type { Notification } from '$lib/api/types';
	import { t } from '$lib/i18n/index.svelte';

	type FeedRow =
		| { kind: 'transient'; id: string; at: number; item: TransientNotification }
		| { kind: 'persistent'; id: string; at: number; item: Notification };

	const feed = $derived.by<FeedRow[]>(() => {
		const rows: FeedRow[] = [
			...ui.notifications.map((n) => ({
				kind: 'transient' as const,
				id: `transient-${n.id}`,
				at: n.at,
				item: n
			})),
			...notifications.items.map((row) => ({
				kind: 'persistent' as const,
				id: `persistent-${row.id}`,
				at: Date.parse(row.created_at) || 0,
				item: row
			}))
		];
		return rows.sort((a, b) => b.at - a.at);
	});

	const unread = $derived(ui.unread + notifications.unread);

	function markAllRead() {
		ui.markNotificationsRead();
		void notifications.markAllRead();
	}

	onMount(() => ui.markNotificationsRead());
</script>

<svelte:head><title>{t('notifications.title', 'Notifications')} · OxiCloud</title></svelte:head>

<div class="notifications-page">
	<header class="notifications-page__header">
		<h1 class="page-title">{t('notifications.title', 'Notifications')}</h1>
		{#if unread > 0}
			<Button
				small
				icon="check-double"
				data-testid="notifications-mark-all-read-btn"
				onclick={markAllRead}
			>
				{t('notifications.mark_all_read', 'Mark all read')}
			</Button>
		{/if}
	</header>

	{#if feed.length === 0}
		<div class="empty-state">
			<Icon name="bell-slash" />
			<p>{t('notifications.empty', 'No notifications')}</p>
		</div>
	{:else}
		<div class="notifications-feed">
			{#each feed as row (row.id)}
				{#if row.kind === 'transient'}
					<TransientNotificationRow n={row.item} />
				{:else}
					<NotificationRow row={row.item} />
				{/if}
			{/each}
		</div>

		{#if notifications.hasMore}
			<div class="notifications-page__more">
				<Button
					small
					disabled={notifications.loadingOlder}
					data-testid="notifications-load-older-btn"
					onclick={() => void notifications.loadOlder()}
				>
					{notifications.loadingOlder
						? t('common.loading', 'Loading…')
						: t('common.load_more', 'Load more')}
				</Button>
			</div>
		{/if}
	{/if}
</div>

<style>
	.notifications-page {
		max-width: 52rem;
		margin: 0 auto;
	}

	.notifications-page__header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		margin-bottom: var(--space-4);
	}

	.notifications-page__header .page-title {
		margin: 0;
	}

	.notifications-feed {
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-2xl);
		overflow: hidden;
	}

	/* The bell clips long messages to one line; the full-history page has
	   room, so let a shared-folder name wrap instead of truncating it. */
	.notifications-feed :global(.notif-item-text) {
		white-space: normal;
		overflow-wrap: anywhere;
	}

	.notifications-page__more {
		display: flex;
		justify-content: center;
		margin-top: var(--space-4);
	}
</style>
