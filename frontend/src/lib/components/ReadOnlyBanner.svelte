<script lang="ts">
	/**
	 * Read-only banner — one component, three variants.
	 *
	 * ## `variant="drive"` (default) — drive-scoped freeze
	 *
	 * Rendered at the top of any page whose content lives in (or is scoped
	 * to) a drive whose `policies.read_only === true`.
	 *
	 * ## `variant="maintenance"` — server-wide freeze
	 *
	 * Rendered inside `AppShell` above `{children}` when the
	 * `x-server-status` header says the whole server is in read-only
	 * mode — typically during a `storage_migration` cutover.
	 *
	 * ## `variant="rotating"` — background key rotation
	 *
	 * K4 storage-key-rotation. `storage_rotate` walks blobs in place;
	 * writes/reads continue normally throughout. Copy makes it clear
	 * this is a background maintenance banner, not a freeze — the app
	 * is fully usable.
	 *
	 * Shape / accent is identical across variants — the design system
	 * reads them as the same family. Only the copy differs.
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
		 * server is in read-only mode. `"rotating"` — a background key
		 * rotation is running; writes continue.
		 */
		variant?: 'drive' | 'maintenance' | 'rotating';
		/** Drive-name shown in the body (variant="drive" only). */
		driveName?: string;
		/** Migration/rotation progress (variant="maintenance" | "rotating" only). */
		progress?: Progress;
	}

	let { variant = 'drive', driveName, progress }: Props = $props();
</script>

<div
	class="read-only-banner"
	class:read-only-banner--rotating={variant === 'rotating'}
	role="region"
	aria-label={variant === 'maintenance'
		? t('server_status.readonly_banner_aria', 'Server maintenance in progress')
		: variant === 'rotating'
			? t('server_status.rotating_banner_aria', 'Storage key rotation in progress')
			: t('drive.read_only_banner.aria', 'This drive is read-only')}
	data-testid={variant === 'maintenance'
		? 'server-status-banner'
		: variant === 'rotating'
			? 'server-status-rotating-banner'
			: 'read-only-banner'}
>
	<div class="read-only-banner__icon" aria-hidden="true">
		<Icon name={variant === 'rotating' ? 'key' : 'lock'} />
	</div>
	<div class="read-only-banner__body">
		<strong>
			{#if variant === 'maintenance'}
				{t('server_status.readonly_title', 'Server maintenance in progress')}
			{:else if variant === 'rotating'}
				{t('server_status.rotating_title', 'Storage key rotation in progress')}
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
			{:else if variant === 'rotating'}
				{#if progress}
					{t(
						'server_status.rotating_progress',
						{
							target: progress.target,
							migrated: progress.migrated,
							total: progress.total,
							percent: progress.percent
						},
						'Rotating encryption on `{{target}}` — {{percent}}% ({{migrated}} / {{total}} blobs). All operations continue normally; this is a background maintenance task.'
					)}
				{:else}
					{t(
						'server_status.rotating_body',
						'A background key rotation is normalising storage. All operations continue normally.'
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

	/* K4 rotating variant — same shape, info accent (softer than the
	   default), signalling "background task, no user-facing freeze".
	   Uses `--color-info` when the palette defines it, falls back to
	   `--color-accent` otherwise. */
	.read-only-banner--rotating {
		border-left-color: var(--color-info, var(--color-accent));
	}

	.read-only-banner--rotating .read-only-banner__icon {
		color: var(--color-info, var(--color-accent));
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
