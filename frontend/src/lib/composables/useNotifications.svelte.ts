/**
 * Global bell store — persistent notifications (Slice E).
 *
 * Owns the reactive state for `NotificationBell`. Module-scoped so
 * one instance drives every consumer in the SPA (badge in AppShell,
 * slide-out panel, admin dashboard hooks, …). Same lifetime as
 * `messageBus`: survives every intra-SPA navigation, dies only on
 * full reload / tab close.
 *
 * Message-bus contract: the FE subscribes to `user:{me}:notifications`
 * (auto-subscribed server-side on WS session open — no `rt.subscribe`
 * frame needed from the client) and refetches the row list whenever
 * a `notification_received` event arrives. The DB is truth; the bus
 * event just says "there's new data, refresh".
 */
import { messageBus } from '$lib/message-bus/client.svelte';
import { session } from '$lib/stores/session.svelte';
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import {
	deleteNotification as apiDelete,
	getUnreadCount,
	listNotifications,
	markAllNotificationsRead,
	markNotificationRead
} from '$lib/api/endpoints/notifications';
import type { Notification } from '$lib/api/types';
import log from 'loglevel';

const bellLog = log.getLogger('oxi:notifications');

class NotificationsStore {
	#items = $state<Notification[]>([]);
	#unread = $state<number>(0);
	#loading = $state<boolean>(false);
	#error = $state<string | null>(null);

	get items(): Notification[] {
		return this.#items;
	}
	get unread(): number {
		return this.#unread;
	}
	get loading(): boolean {
		return this.#loading;
	}
	get error(): string | null {
		return this.#error;
	}

	/**
	 * Fetch the newest page + refresh the badge count. Idempotent —
	 * safe to call on every bus push, on mount, on visibility return.
	 */
	async refresh(): Promise<void> {
		this.#loading = true;
		try {
			const res = await listNotifications({ limit: 50 });
			this.#items = res.items;
			this.#unread = res.unread_count;
			this.#error = null;
		} catch (e) {
			this.#error = e instanceof Error ? e.message : String(e);
			bellLog.warn('notifications refresh failed', e);
		} finally {
			this.#loading = false;
		}
	}

	/** Badge-only fast path — avoids fetching payloads. */
	async refreshBadge(): Promise<void> {
		try {
			this.#unread = await getUnreadCount();
		} catch (e) {
			bellLog.warn('badge refresh failed', e);
		}
	}

	async markRead(id: string): Promise<void> {
		// Optimistic update — flip locally, then confirm on the wire.
		// Same pattern the folder-view uses on rename: reactive-first,
		// server-eventually. A wire failure re-fetches from truth.
		const row = this.#items.find((n) => n.id === id);
		if (row && row.read_at === null) {
			row.read_at = new Date().toISOString();
			this.#unread = Math.max(0, this.#unread - 1);
		}
		try {
			await markNotificationRead(id);
		} catch (e) {
			bellLog.warn('markRead failed; reconciling', e);
			await this.refresh();
		}
	}

	async markAllRead(): Promise<void> {
		const now = new Date().toISOString();
		for (const row of this.#items) {
			if (row.read_at === null) row.read_at = now;
		}
		this.#unread = 0;
		try {
			await markAllNotificationsRead();
		} catch (e) {
			bellLog.warn('markAllRead failed; reconciling', e);
			await this.refresh();
		}
	}

	async delete(id: string): Promise<void> {
		const idx = this.#items.findIndex((n) => n.id === id);
		if (idx >= 0) {
			const [removed] = this.#items.splice(idx, 1);
			if (removed && removed.read_at === null) {
				this.#unread = Math.max(0, this.#unread - 1);
			}
		}
		try {
			await apiDelete(id);
		} catch (e) {
			bellLog.warn('delete failed; reconciling', e);
			await this.refresh();
		}
	}

	/** Reset — called on logout so a switch-user doesn't inherit the
	 *  previous session's rows. */
	reset(): void {
		this.#items = [];
		this.#unread = 0;
		this.#error = null;
	}
}

/** Module-scoped singleton — one bell state per SPA lifetime. */
export const notifications = new NotificationsStore();

/**
 * Wire the bell into a component's lifecycle. Fires an initial fetch
 * on mount, subscribes to `user:{me}:notifications` for live pushes,
 * refetches on reconnect (bus events lost during outage window).
 *
 * Call once from the app root (`+layout.svelte`) — this store is
 * global. Additional callers do NOT need to re-mount; they can just
 * read `notifications.items` / `notifications.unread`.
 */
export function useNotifications(): void {
	$effect(() => {
		const userId = session.user?.id;
		if (!userId) return; // not logged in — nothing to fetch
		// Initial hydrate from DB truth. Runs whether or not the bus
		// is enabled — the bell has to work in "polling only" mode
		// when OXICLOUD_MESSAGEBUS_ENABLE=false too.
		void notifications.refresh();
	});

	$effect(() => {
		if (!serverConfig.features.message_bus) return;
		const userId = session.user?.id;
		if (!userId) return;

		// The topic is auto-subscribed server-side on WS session open
		// (same pattern as `:authz`); this call refcounts up to the
		// existing sub, doesn't fire a second `rt.subscribe` frame.
		const release = messageBus.subscribe(
			`user:${userId}:notifications`,
			(params) => {
				if (params.event === 'notification_received') {
					// Bus event carries only the poke. Refetch the
					// list — cheap, gives us the new row with its
					// full payload from truth.
					void notifications.refresh();
				}
			},
			() => {
				// Server-evicted (session flipped) — clear local so
				// the badge stops showing stale count.
				notifications.reset();
			}
		);

		const releaseReconnect = messageBus.onReconnect(() => {
			// A push we missed during the outage window is only
			// recoverable by rereading the DB.
			void notifications.refresh();
		});

		return () => {
			release();
			releaseReconnect();
		};
	});
}
