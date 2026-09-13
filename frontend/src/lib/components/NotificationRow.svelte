<!--
  Single-file renderer for one persistent notification (bell).

  Slice E's rendering path — reuses the AppShell bell's existing
  `.notif-item*` classes so it slots into the same dropdown as the
  transient toast rows. One switch on `row.kind`; a kind's block gets
  extracted to its own component only when it exceeds ~30 lines,
  needs local `$state`, or two kinds start sharing a sub-component.
  See `docs/plan/templated-messages.md § Rendering`.

  Click semantics:
   - Row body click        → navigate to the resource + mark-read
   - Anchor click (inner)  → same navigation, `stopPropagation` so
                             the outer click doesn't re-fire
   - Close button click    → delete the row, `stopPropagation`
-->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import type { Notification, SharegrantedPayload } from '$lib/api/types';
	import { NOTIFICATION_KIND } from '$lib/api/types';
	import { notifications } from '$lib/composables/useNotifications.svelte';

	/** SvelteKit's `resolve()` is typed with compile-time route keys.
	 *  The bell's `href` is a runtime-computed path from the resource's
	 *  storage path, not a compile-time key — same pattern as
	 *  `AppShell::navHref`. Cast at one wrapper site rather than
	 *  scattering `@ts-expect-error` per callsite. */
	function runtimeResolve(href: string): string {
		// @ts-expect-error runtime-known path, not a literal typed route key
		return resolve(href);
	}

	interface Props {
		row: Notification;
		/** Called after the row-body click's mark-read/navigate — the
		 *  bell panel wraps this to close its dropdown on activation. */
		onactivate?: () => void;
	}
	let { row, onactivate }: Props = $props();

	function formatPersistentTime(iso: string): string {
		try {
			return new Date(iso).toLocaleString();
		} catch {
			return '';
		}
	}

	function persistentIcon(kind: string): string {
		switch (kind) {
			case NOTIFICATION_KIND.SHARE_GRANTED:
				return 'user-plus';
			case NOTIFICATION_KIND.NEW_LOGIN_FROM_NEW_DEVICE:
				return 'shield-alt';
			case NOTIFICATION_KIND.JOB_COMPLETED_FOR_YOU:
				return 'check-circle';
			case NOTIFICATION_KIND.STORAGE_QUOTA_THRESHOLD:
				return 'database';
			default:
				return 'bell';
		}
	}

	/** Best-effort resource link by kind:
	 *
	 *  - `folder` → `/files/{id}` (SvelteKit's `/files/[...path]`
	 *    route resolves by UUID; the caller has Read on the shared
	 *    folder by construction of the grant).
	 *  - `file`   → `/shared-with-me?file={id}` — the `/files/{id}`
	 *    route requires a FOLDER id AND access to the parent folder,
	 *    neither of which is guaranteed for a file-scoped grant.
	 *    `/shared-with-me` is the guaranteed-accessible home for
	 *    shares (every recipient of a `share_granted` sees their
	 *    row here), and its `?file=` deep link opens the inline
	 *    FileViewer.
	 *  - `drive`  → `/files/{navigate_folder_id}` — the drive
	 *    itself has no browsable URL, but its root folder does.
	 *    Backend enrichment populates `navigate_folder_id` from
	 *    `Drive.root_folder_id`. If the lookup failed at ingest
	 *    (`navigate_folder_id` absent), the row falls back to
	 *    bold-text — better silent than a broken link.
	 *  - Other kinds (calendar, address_book, playlist) → `null`.
	 *    Not reachable via `/files/*`; the row renders the
	 *    resource name as bold text (not a link). Adding routing
	 *    for those = one branch here + backend enrichment for the
	 *    kind.
	 *
	 *  `resource_path` on the payload is kept as a display-time
	 *  snapshot (used by hover / a11y labels) but not the anchor
	 *  target; the anchor uses `resource_id` (or
	 *  `navigate_folder_id`) so the link survives future renames +
	 *  moves. See `docs/plan/templated-messages.md § File
	 *  notification routing`. */
	function resourceHref(payload: unknown): string | null {
		if (typeof payload !== 'object' || payload === null) return null;
		const p = payload as Record<string, unknown>;
		const type = p.resource_type;
		const id = p.resource_id;
		if (typeof id !== 'string' || id.length === 0) return null;
		if (type === 'folder') return `/files/${id}`;
		if (type === 'file') return `/shared-with-me?file=${encodeURIComponent(id)}`;
		if (type === 'drive') {
			const navId = p.navigate_folder_id;
			if (typeof navId === 'string' && navId.length > 0) {
				return `/files/${navId}`;
			}
			// Enrichment failed at ingest — no browsable target.
			// Bold-text fallback in the template.
			return null;
		}
		return null;
	}

	function onBodyClick(): void {
		const href = resourceHref(row.payload);
		void notifications.markRead(row.id);
		onactivate?.();
		// `href` is a runtime path from the resource's storage
		// path — not a compile-time route key. `runtimeResolve`
		// wraps `resolve()` at a single site (lint rule looks for
		// the literal call, so we disable at the callsite).
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		if (href) void goto(runtimeResolve(href));
	}

	function onBodyKeydown(e: KeyboardEvent): void {
		if (e.key === 'Enter' || e.key === ' ') {
			e.preventDefault();
			onBodyClick();
		}
	}

	function onAnchorClick(e: MouseEvent): void {
		// The <a> element handles navigation natively (letting the
		// browser go via <a href> preserves cmd-click / middle-click
		// semantics). Just mark-read + stop the outer click.
		e.stopPropagation();
		void notifications.markRead(row.id);
		onactivate?.();
	}

	function onCloseClick(e: MouseEvent): void {
		e.stopPropagation();
		void notifications.delete(row.id);
	}

	function onCloseKeydown(e: KeyboardEvent): void {
		if (e.key === 'Enter' || e.key === ' ') {
			e.preventDefault();
			e.stopPropagation();
			void notifications.delete(row.id);
		}
	}
</script>

<div
	class="notif-item notif-item--{row.kind}"
	role="button"
	tabindex="0"
	data-testid="notification-row"
	style:font-weight={row.read_at === null ? '500' : 'normal'}
	style:cursor="pointer"
	onclick={onBodyClick}
	onkeydown={onBodyKeydown}
>
	<span class="notif-item-icon">
		<Icon name={persistentIcon(row.kind)} />
	</span>
	<div class="notif-item-body">
		<div class="notif-item-text">
			{#if row.kind === NOTIFICATION_KIND.SHARE_GRANTED}
				{@const p = row.payload as unknown as SharegrantedPayload}
				{@const href = resourceHref(row.payload)}
				<UserVignette userId={p.granter_id} />
				<!-- eslint-disable-next-line svelte/no-useless-mustaches -->
				{' '}{t(
					'notifications.share_granted.verb',
					'shared'
				)}<!-- eslint-disable-next-line svelte/no-useless-mustaches -->
				{' '}
				{#if href}
					<!-- `runtimeResolve` wraps SvelteKit's `resolve()`
					     with the same `@ts-expect-error` pattern
					     AppShell uses — bell href is a runtime path,
					     not a compile-time route key. -->
					<!-- eslint-disable-next-line svelte/no-navigation-without-resolve -->
					<a href={runtimeResolve(href)} onclick={onAnchorClick}>
						{p.resource_name ?? p.resource_type}
					</a>
				{:else}
					<strong>{p.resource_name ?? p.resource_type}</strong>
				{/if}
				<!-- eslint-disable-next-line svelte/no-useless-mustaches -->
				{' '}({p.role})
			{:else}
				<!-- Generic fallback for kinds without their own template
				     yet (new_login_from_new_device, job_completed_for_you,
				     storage_quota_threshold — each waits for its
				     ingester to ship, see `docs/plan/templated-messages.md
				     § Deferred`). -->
				<span>
					{t('notifications.persistent.generic', { kind: row.kind }, `Notification (${row.kind}).`)}
				</span>
			{/if}
		</div>
		<div class="notif-item-time">{formatPersistentTime(row.created_at)}</div>
	</div>
	<button
		type="button"
		class="notif-item-close"
		aria-label={t('common.delete', 'Delete')}
		onclick={onCloseClick}
		onkeydown={onCloseKeydown}
	>
		<Icon name="times" />
	</button>
</div>

<style>
	/*
	 * Row layout classes (`.notif-item`, `.notif-item-icon`,
	 * `.notif-item-body`, `.notif-item-text`, `.notif-item-time`)
	 * are inherited from AppShell's bell dropdown. Only the close
	 * button needs its own styling — everything else is styled
	 * by the parent panel.
	 *
	 * `:global()` needed because Svelte scopes styles per
	 * component; the button's class is applied to a real element
	 * in this component but interacts with the parent's `.notif-item`
	 * hover state.
	 */
	.notif-item-close {
		background: transparent;
		border: 0;
		padding: 0.25rem;
		margin-left: auto;
		color: var(--color-text-muted);
		border-radius: 4px;
		cursor: pointer;
		align-self: flex-start;
		font: inherit;
	}

	.notif-item-close:hover,
	.notif-item-close:focus-visible {
		background: var(--color-hover);
		color: var(--color-text);
	}
</style>
