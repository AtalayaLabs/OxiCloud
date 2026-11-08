/**
 * Transient UI state — toasts plus the persistent notification feed shown in the
 * top-bar bell. `notify()` raises a transient toast and records a notification
 * entry (so uploads, errors and successes accumulate in the bell). Component-local
 * state is preferred; only state that must cross component boundaries lives here.
 */
export type ToastKind = 'info' | 'success' | 'error' | 'warning';

export interface ToastAction {
	/** Button label — should be short (≤ 20 chars). */
	label: string;
	/** Invoked when the button is clicked; the toast auto-dismisses after. */
	onClick: () => void;
}

export interface Toast {
	id: number;
	message: string;
	kind: ToastKind;
	/** Optional inline action (e.g. "Go to Files" on a wrong-drop-zone toast). */
	action?: ToastAction;
}

export interface Notification {
	id: number;
	message: string;
	kind: ToastKind;
	at: number;
	read: boolean;
	/** 0–100 while an operation is in progress; undefined for plain notifications. */
	progress?: number;
	/** Optional icon-registry name override (defaults derived from kind). */
	icon?: string;
	/** Current per-file label (e.g. the filename being uploaded). */
	currentFile?: string;
	/** Files finished so far in the batch (for the "N / M files" counter). */
	completed?: number;
	/** Total files in the batch (for the "N / M files" counter). */
	total?: number;
	/** Aborts the in-flight operation (upload batch); undefined for plain rows. */
	onCancel?: () => void;
}

/** Announce a message to the matching ARIA live region (errors are assertive). */
function announce(message: string, assertive = false): void {
	const msg = message.trim();
	if (!msg || typeof document === 'undefined' || !document.body) return;
	const id = assertive ? 'a11y-live-assertive' : 'a11y-live-polite';
	let region = document.getElementById(id);
	if (!region) {
		region = document.createElement('div');
		region.id = id;
		region.className = 'sr-only';
		region.setAttribute('aria-live', assertive ? 'assertive' : 'polite');
		region.setAttribute('aria-atomic', 'true');
		region.setAttribute('role', assertive ? 'alert' : 'status');
		document.body.appendChild(region);
	}
	// Clear first, then set next frame so repeats register as a change.
	region.textContent = '';
	const target = region;
	if (typeof requestAnimationFrame !== 'undefined') {
		requestAnimationFrame(() => (target.textContent = msg));
	} else {
		target.textContent = msg;
	}
}

class UiStore {
	toasts = $state<Toast[]>([]);
	notifications = $state<Notification[]>([]);
	#seq = 0;

	/**
	 * Bumped to request the bell panel auto-open (e.g. on upload start) and to
	 * trigger the bell "ring" animation. AppShell watches this token.
	 */
	bellPing = $state(0);

	unread = $derived(this.notifications.filter((n) => !n.read).length);

	/** Unread count clamped for the badge — caps at "99+" like the original. */
	unreadBadge = $derived(this.unread > 99 ? '99+' : String(this.unread));

	/**
	 * Raise a toast and record a notification. `at` is stamped from the clock at
	 * call time; pass `record: false` for purely transient messages.
	 *
	 * The optional `opts.action` renders an inline button in the toast (e.g.
	 * "Go to Files" on a wrong-drop-zone warning); the callback fires on
	 * click and the toast auto-dismisses right after so a caller doesn't have
	 * to manage the id.
	 */
	notify(
		message: string,
		kind: ToastKind = 'info',
		timeoutMs = 4000,
		record = true,
		opts: { action?: ToastAction } = {}
	): number {
		const id = ++this.#seq;
		this.toasts = [...this.toasts, { id, message, kind, action: opts.action }];
		if (record) {
			this.notifications = [
				{ id, message, kind, at: Date.now(), read: false },
				...this.notifications
			];
		}
		announce(message, kind === 'error');
		if (timeoutMs > 0 && typeof setTimeout !== 'undefined') {
			setTimeout(() => this.dismiss(id), timeoutMs);
		}
		return id;
	}

	dismiss(id: number): void {
		this.toasts = this.toasts.filter((t) => t.id !== id);
	}

	/** Request the bell panel to open and play its ring animation. */
	ringBell(): void {
		this.bellPing++;
	}

	/**
	 * Begin a progress notification (e.g. an upload). Pass `total` to show the
	 * "N / M files" counter. Opens the bell, rings it, and announces the start.
	 */
	startProgress(
		message: string,
		icon = 'cloud-upload-alt',
		total?: number,
		onCancel?: () => void
	): number {
		const id = ++this.#seq;
		this.notifications = [
			{
				id,
				message,
				kind: 'info',
				at: Date.now(),
				read: false,
				progress: 0,
				icon,
				onCancel,
				...(total !== undefined ? { total, completed: 0 } : {})
			},
			...this.notifications
		];
		this.ringBell();
		announce(message);
		return id;
	}

	/** Update the percentage (0–100) of an in-flight progress notification. */
	updateProgress(
		id: number,
		progress: number,
		message?: string,
		extra?: { currentFile?: string; completed?: number }
	): void {
		this.notifications = this.notifications.map((n) =>
			n.id === id
				? {
						...n,
						progress,
						...(message ? { message } : {}),
						...(extra?.currentFile !== undefined ? { currentFile: extra.currentFile } : {}),
						...(extra?.completed !== undefined ? { completed: extra.completed } : {})
					}
				: n
		);
	}

	/** Resolve a progress notification into a final success/error entry. */
	finishProgress(id: number, message: string, kind: ToastKind = 'success'): void {
		this.notifications = this.notifications.map((n) =>
			n.id === id
				? {
						...n,
						message,
						kind,
						progress: undefined,
						currentFile: undefined,
						onCancel: undefined,
						at: Date.now()
					}
				: n
		);
		this.toasts = [...this.toasts, { id: ++this.#seq, message, kind }];
		announce(message, kind === 'error');
		const tid = this.#seq;
		if (typeof setTimeout !== 'undefined') setTimeout(() => this.dismiss(tid), 4000);
	}

	markNotificationsRead(): void {
		this.notifications = this.notifications.map((n) => (n.read ? n : { ...n, read: true }));
	}

	/**
	 * Drop one row from the notification history.
	 *
	 * Not `dismiss`, which takes a TOAST off the screen: a toast and its
	 * history entry are two records with two id spaces, and dismissing the
	 * toast leaves the entry in the list — which is exactly what happened when
	 * `/notifications` first wired its swipe to `dismiss`.
	 */
	dismissNotification(id: number): void {
		this.notifications = this.notifications.filter((n) => n.id !== id);
	}

	clearNotifications(): void {
		this.notifications = [];
	}
}

export const ui = new UiStore();
