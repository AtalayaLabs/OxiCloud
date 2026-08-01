<script lang="ts">
	/**
	 * Read-only banner — one component, two variants.
	 *
	 * ## `variant="drive"` (default) — drive-scoped freeze
	 *
	 * Rendered at the top of any page whose content lives in (or is scoped
	 * to) a drive whose `policies.read_only === true`. Members see the
	 * banner and understand why upload / rename / delete / share
	 * affordances elsewhere in the app fail with a generic error toast —
	 * the backend engine gate refuses every non-`Read` permission on
	 * resources in the drive.
	 *
	 * Only `Read` permissions pass; the banner does not need to gate any
	 * behavior itself. It's pure signage. Backed by
	 * `docs/plan/drive.md` §8 (`read_only`).
	 *
	 * Consumed by:
	 *   - `routes/config/drive/[uuid]/+page.svelte` — always shown when
	 *     the drive being configured is frozen.
	 *   - `routes/files/[...path]/+page.svelte` — shown when the current
	 *     folder's owning drive is frozen (parent looks up drive via
	 *     `drives.findByRootFolderId`/`findById`).
	 *
	 * ## `variant="maintenance"` — server-wide freeze
	 *
	 * Rendered inside `AppShell` above `{children}` when the
	 * `x-server-status` header (see `middleware::server_status`) says
	 * the whole server is in read-only mode — typically during a
	 * storage-backend migration. Optional `progress` lets the banner
	 * show target + percentage.
	 *
	 * Shape / accent is identical between both variants — the design
	 * system reads them as the same family. Only the copy differs.
	 */
	import { t } from '$lib/i18n/index.svelte';
	import Icon from '$lib/icons/Icon.svelte';

	interface Progress {
		target: string;
		migrated: number;
		total: number;
		percent: number;
	}

	interface Props {
		/**
		 * `"drive"` — a specific drive is frozen (default; back-compat
		 * with pre-migration call sites). `"maintenance"` — the whole
		 * server is in read-only mode.
		 */
		variant?: 'drive' | 'maintenance';
		/** Drive-name shown in the body (variant="drive" only). */
		driveName?: string;
		/** Migration progress (variant="maintenance" only). */
		progress?: Progress;
	}

	let { variant = 'drive', driveName, progress }: Props = $props();
</script>

<div
	class="read-only-banner"
	role="region"
	aria-label={variant === 'maintenance'
		? t('server_status.readonly_banner_aria', 'Server maintenance in progress')
		: t('drive.read_only_banner.aria', 'This drive is read-only')}
	data-testid={variant === 'maintenance' ? 'server-status-banner' : 'read-only-banner'}
>
	<div class="read-only-banner__icon" aria-hidden="true">
		<Icon name="lock" />
	</div>
	<div class="read-only-banner__body">
		<strong>
			{#if variant === 'maintenance'}
				{t('server_status.readonly_title', 'Server maintenance in progress')}
			{:else if driveName}
				{t(
					'drive.read_only_banner.title_named',
					{ name: driveName },
					'Drive "{{name}}" is read-only'
				)}
			{:else}
				{t('drive.read_only_banner.title', 'This drive is read-only')}
			{/if}
		</strong>
		<span>
			{#if variant === 'maintenance'}
				{#if progress}
					{t(
						'server_status.readonly_progress',
						{
							target: progress.target,
							migrated: progress.migrated,
							total: progress.total,
							percent: progress.percent
						},
						'Migrating storage to `{{target}}` — {{percent}}% ({{migrated}} / {{total}} blobs). Uploads, renames, deletes, and shares are refused; reads and downloads work as normal.'
					)}
				{:else}
					{t(
						'server_status.readonly_body',
						'Uploads, renames, deletes, and shares are refused temporarily. Reads and downloads work as normal.'
					)}
				{/if}
			{:else}
				{t(
					'drive.read_only_banner.body',
					'Uploads, edits, deletes, renames, sharing and membership changes are refused. Reads and downloads keep working. Contact an administrator to un-freeze the drive.'
				)}
			{/if}
		</span>
	</div>
</div>

<style>
	/* Shape matches the sibling upgrade-banner in
	   `routes/shared-with-me/+page.svelte` so the two banners read as
	   the same family; only the accent shifts to communicate "info /
	   frozen" rather than "action / upgrade." */
	.read-only-banner {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-3) var(--space-4);
		margin-bottom: var(--space-4);
		background: var(--color-surface-raised);
		border: 1px solid var(--color-border);
		border-left: 4px solid var(--color-accent);
		border-radius: var(--radius-md);
	}

	.read-only-banner__icon {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 2rem;
		height: 2rem;
		border-radius: var(--radius-md);
		background: var(--color-surface);
		color: var(--color-accent);
		font-size: var(--text-lg);
	}

	.read-only-banner__body {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
	}

	.read-only-banner__body strong {
		font-weight: var(--weight-semibold);
		color: var(--color-text);
	}

	.read-only-banner__body span {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	@media (width <= 600px) {
		.read-only-banner {
			align-items: flex-start;
		}
	}
</style>
