<script lang="ts">
	import { onMount } from 'svelte';
	import Button from '$lib/components/Button.svelte';
	import GestureList from '$lib/components/GestureList.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import NotificationRow from '$lib/components/NotificationRow.svelte';
	import TransientNotificationRow from '$lib/components/TransientNotificationRow.svelte';
	import { markEverythingRead, notifications } from '$lib/composables/useNotifications.svelte';
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

	/**
	 * Drop one row, by whichever route its kind needs: a transient toast only
	 * ever lived in the UI store, a persistent one has to be deleted server
	 * side or it comes back with the next fetch.
	 */
	function dismiss(entry: FeedRow) {
		if (entry.kind === 'transient') ui.dismissNotification(entry.item.id);
		else void notifications.delete(entry.item.id);
	}

	/**
	 * Mark one row read — the right-hand swipe, and its hover button.
	 *
	 * NOT a toggle yet, and the missing half is server-side: the API exposes
	 * `POST /notifications/{id}/read` and nothing that clears `read_at`
	 * (`routes.rs`). So "mark unread" has no call to make, and a gesture that
	 * silently did nothing on half its rows would be worse than one that only
	 * goes one way. The swipe is idempotent either way: a row already read
	 * stays read.
	 */
	function markRead(entry: FeedRow) {
		if (entry.kind === 'transient') ui.markNotificationRead(entry.item.id);
		else void notifications.markRead(entry.item.id);
	}

	// Shared with the bell dropdown, which offers the same action: both stores
	// feed the badge, so marking one of them is a half-read state nobody can
	// clear from the UI.
	const markAllRead = markEverythingRead;

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
		<!--
			`GestureList` without `reorderCapable`: the feed is sorted by time,
			so a grip here would offer a move that cannot be kept. What it gains
			is the dismissal — a left swipe on touch, the same action as a button
			on hover — shared with the player queue and the playlist rather than
			hand-rolled a third time.

			Dismissing takes the ROW, not an index, because the two kinds are not
			dismissed the same way: a toast is dropped from the UI store, a
			persistent one is deleted through the API.
		-->
		<div class="notifications-feed">
			<GestureList
				items={feed}
				key={(row) => row.id}
				swipeLeft={{
					// The same eraser as the player queue and the playlist: one icon
					// for "take this out of the list", wherever the list is.
					icon: 'eraser',
					label: t('common.dismiss', 'Dismiss'),
					tone: 'danger',
					testId: (entry) => `notifications-dismiss-${entry.id}`,
					run: dismiss
				}}
				swipeRight={{
					// Blue and additive, against the red of the dismissal — the two
					// directions of this list are opposites, and red against blue is
					// the pair that survives colour blindness.
					icon: 'check-double',
					label: t('notifications.mark_read', 'Mark read'),
					tone: 'add',
					testId: (entry) => `notifications-read-${entry.id}`,
					run: markRead
				}}
				listLabel={t('notifications.title', 'Notifications')}
			>
				{#snippet row(entry)}
					{#if entry.kind === 'transient'}
						<TransientNotificationRow n={entry.item} />
					{:else}
						<!-- No row-level cross here: the list's own action is the one
						     route to deletion on this page. -->
						<NotificationRow row={entry.item} showClose={false} />
					{/if}
				{/snippet}
			</GestureList>
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
