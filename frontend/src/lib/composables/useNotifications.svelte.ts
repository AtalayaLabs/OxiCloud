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
 * frame needed from the client) and refetches on every push. The DB
 * is truth; the bus event is a cache-invalidation hint.
 *
 * # Delta catch-up + dedup
 *
 * Two paths can deliver the SAME row and must not double-count it:
 *
 * 1. **WS live push** — `notification_received` event → calls
 *    `refreshDelta(#lastReceivedAt)` which fetches
 *    `?after=<lastReceivedAt>&limit=100`, merges the result into the
 *    reactive list.
 * 2. **Reconnect catch-up** — after a grace-close (tab idle > 60 s)
 *    or a network drop, the WS reopens and `onReconnect` fires the
 *    same `refreshDelta(#lastReceivedAt)`. This backfills rows that
 *    landed while the socket was closed.
 *
 * The race: a NEW notification created after the reconnect but
 * before the delta fetch returns lands via BOTH paths — WS push
 * (delta fetch A) and reconnect (delta fetch B). Dedup lives in
 * `mergeById`: incoming rows keyed on `id` displace any existing
 * entry with the same id, so the row appears exactly once. Server
 * `read_at` always wins over local because incoming replaces.
 *
 * `#lastReceivedAt` is the newest `created_at` we've observed. It
 * feeds every delta fetch. Initial `refresh()` seeds it from the
 * newest returned row; subsequent merges update it to the newest of
 * the incoming set.
 */
import { messageBus } from '$lib/message-bus/client.svelte';
import { session } from '$lib/stores/session.svelte';
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import { ui } from '$lib/stores/ui.svelte';
import { t } from '$lib/i18n/index.svelte';
import {
	deleteNotification as apiDelete,
	getUnreadCount,
	listNotifications,
	markAllNotificationsRead,
	markNotificationRead
} from '$lib/api/endpoints/notifications';
import type { Notification, SharegrantedPayload } from '$lib/api/types';
import { NOTIFICATION_KIND } from '$lib/api/types';
import log from 'loglevel';

const bellLog = log.getLogger('oxi:notifications');

/**
 * Plain-string summary of a persistent notification — used for the
 * transient toast preview (fade-to-bell UX affordance) and for
 * accessible labels where a component can't render rich content.
 *
 * Rich Svelte rendering lives in `NotificationRow.svelte`. This is
 * the string-only equivalent for toasts and aria-labels.
 */
export function summaryFor(row: Notification): string {
	switch (row.kind) {
		case NOTIFICATION_KIND.SHARE_GRANTED: {
			const p = row.payload as unknown as SharegrantedPayload;
			const resource = p.resource_name ?? p.resource_type ?? 'a resource';
			return t(
				'notifications.persistent.share_granted',
				{ role: p.role ?? 'access', resource },
				`Someone shared "${resource}" with you (${p.role ?? 'access'}).`
			);
		}
		case NOTIFICATION_KIND.NEW_LOGIN_FROM_NEW_DEVICE:
			return t(
				'notifications.persistent.new_device_login',
				'A new device signed into your account.'
			);
		case NOTIFICATION_KIND.JOB_COMPLETED_FOR_YOU: {
			const name = String((row.payload as { name?: unknown }).name ?? 'a job');
			return t('notifications.persistent.job_completed', { name }, `Job "${name}" finished.`);
		}
		case NOTIFICATION_KIND.STORAGE_QUOTA_THRESHOLD:
			return t(
				'notifications.persistent.quota_threshold',
				'You are approaching your storage quota.'
			);
		default:
			return t(
				'notifications.persistent.generic',
				{ kind: row.kind },
				`Notification (${row.kind}).`
			);
	}
}

/**
 * Merge `incoming` rows into `existing`, deduplicating on `id`.
 * Where an id appears in both, the incoming (fresh-from-server)
 * copy wins — so a `read_at` flip visible in `incoming` correctly
 * overrides a stale local unread state. Result stays sorted
 * newest-first by `created_at`.
 *
 * Exported for the unit tests to exercise the race semantics
 * without spinning up a full store.
 */
export function mergeById(existing: Notification[], incoming: Notification[]): Notification[] {
	if (incoming.length === 0) return existing;
	// Local lookup set — pure function, no reactive state involved,
	// so `SvelteSet` would add allocations without buying anything.
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	const incomingIds = new Set(incoming.map((n) => n.id));
	const kept = existing.filter((n) => !incomingIds.has(n.id));
	// String compare of ISO-8601 UTC timestamps sorts identically
	// to Date compare — cheaper, no allocation per row.
	return [...incoming, ...kept].sort((a, b) => b.created_at.localeCompare(a.created_at));
}

class NotificationsStore {
	#items = $state<Notification[]>([]);
	#unread = $state<number>(0);
	#loading = $state<boolean>(false);
	#error = $state<string | null>(null);
	/** Newest `created_at` we've observed, ISO 8601. Feeds the
	 *  `?after=…` cursor on delta fetches. `null` until the first
	 *  successful `refresh()` seeds it. */
	#lastReceivedAt: string | null = null;

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
	 * Full refresh — replaces the local list with the newest page
	 * from the server. Used on initial mount + as fallback when a
	 * delta fetch fails or a mutation reconciliation runs.
	 */
	async refresh(): Promise<void> {
		this.#loading = true;
		try {
			const res = await listNotifications({ limit: 50 });
			this.#items = res.items;
			this.#unread = res.unread_count;
			this.#lastReceivedAt = res.items[0]?.created_at ?? this.#lastReceivedAt;
			this.#error = null;
		} catch (e) {
			this.#error = e instanceof Error ? e.message : String(e);
			bellLog.warn('notifications refresh failed', e);
		} finally {
			this.#loading = false;
		}
	}

	/**
	 * Delta fetch — pulls only rows strictly newer than
	 * `#lastReceivedAt` (or does nothing if we've never fetched yet;
	 * the caller should fall back to `refresh()` in that case).
	 * Merges via `mergeById` so a concurrent WS push and reconnect
	 * catch-up can't double-count a row that landed twice.
	 *
	 * Fresh rows — rows the local set didn't have before the merge —
	 * each fire a transient toast via `ui.notify(..., record: false)`
	 * so the user gets a peripheral awareness cue that fades to the
	 * bell (which keeps the row in its persistent history). The
	 * bell's ring animation plays via `ui.ringBell()` so a single
	 * bump signals "something new is in there".
	 *
	 * Silent no-op when the server returns 0 rows — we're already in
	 * sync. Updates `#lastReceivedAt` to the newest of the merged set.
	 */
	async refreshDelta(): Promise<void> {
		if (this.#lastReceivedAt === null) {
			// Never fetched — fall back to a full refresh so the
			// caller doesn't need to distinguish the two cases.
			return this.refresh();
		}
		try {
			// `limit: 100` sized to cover realistic bell traffic per
			// hour without paginating; a rare heavy sender who blows
			// past 100 in one gap still gets 100 newest and the DB
			// row count (unread badge) stays authoritative.
			const res = await listNotifications({
				after: this.#lastReceivedAt,
				limit: 100
			});
			if (res.items.length > 0) {
				// Snapshot the pre-merge id set so we can identify
				// which rows are genuinely fresh vs already-known
				// (an already-known row can come back on a delta
				// fetch if its read_at flipped on another device).
				const before = new Set(this.#items.map((n) => n.id));
				this.#items = mergeById(this.#items, res.items);
				// Newest of merged set — take the first item's
				// created_at since the result is sorted DESC.
				this.#lastReceivedAt = res.items[0].created_at;

				// Toast preview for every fresh row. Pass
				// `record: false` so it doesn't add a phantom
				// transient entry to `ui.notifications` that would
				// duplicate the persistent row already in
				// `notifications.items` (the bell dropdown shows
				// them side by side). `ui.ringBell()` bumps the
				// bell-ring animation once for the batch.
				const fresh = res.items.filter((r) => !before.has(r.id));
				if (fresh.length > 0) {
					for (const row of fresh) {
						ui.notify(summaryFor(row), 'info', 4000, false);
					}
					ui.ringBell();
				}
			}
			// unread_count is the authoritative live server count —
			// always update it even when the delta was empty (a row
			// could have been mark-read'd on another device).
			this.#unread = res.unread_count;
			this.#error = null;
		} catch (e) {
			this.#error = e instanceof Error ? e.message : String(e);
			bellLog.warn('notifications delta failed', e);
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
		this.#lastReceivedAt = null;
		this.#error = null;
	}
}

/** Module-scoped singleton — one bell state per SPA lifetime. */
export const notifications = new NotificationsStore();

/**
 * Wire the bell into a component's lifecycle. Fires an initial full
 * fetch on mount, subscribes to `user:{me}:notifications` for live
 * pushes, delta-fetches on reconnect (backfills rows missed during
 * grace-close / network gap).
 *
 * Call once from the app root (`AppShell`) — this store is global.
 * Additional callers do NOT need to re-mount; they can just read
 * `notifications.items` / `notifications.unread`.
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
					// Bus event carries only the poke. Delta-fetch
					// from `#lastReceivedAt` — cheap when the store
					// is caught up, brings the new row with its full
					// payload from truth. Dedup via `mergeById`
					// handles the race with an in-flight reconnect
					// catch-up returning the same row.
					void notifications.refreshDelta();
				}
			},
			() => {
				// Server-evicted (session flipped) — clear local so
				// the badge stops showing stale count.
				notifications.reset();
			}
		);

		const releaseReconnect = messageBus.onReconnect(() => {
			// Tab was hidden > 60 s, or network dropped. WS just
			// reopened — any bus events published during the gap
			// are lost. Backfill via the `?after=<lastReceivedAt>`
			// cursor. Server's `unread_count` in the response is
			// authoritative — a mark-read on another device while
			// we were dark shows up here.
			//
			// Race with a live rt.event that lands milliseconds
			// later: `mergeById` deduplicates on `id`, so the
			// same row from both paths appears exactly once.
			void notifications.refreshDelta();
		});

		return () => {
			release();
			releaseReconnect();
		};
	});
}
