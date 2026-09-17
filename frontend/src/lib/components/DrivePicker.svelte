<script lang="ts">
	import { appPath } from '$lib/utils/appPath';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { onMount } from 'svelte';

	import type { Drive } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { drives as drivesStore, driveIcon } from '$lib/stores/drives.svelte';
	import { formatBytes } from '$lib/utils/format';

	interface Props {
		onnavigate?: () => void;
	}
	let { onnavigate }: Props = $props();

	// URL of `/files/<first>/<second>/...` — the first segment identifies the
	// drive root the user navigated through. We use it to highlight the active
	// drive in the picker. Deep-linking to a descendant folder of a non-default
	// drive bypasses this highlight (the URL's leading segment is the deep
	// folder id, not the drive root); that's acceptable — D2 can refine this
	// by resolving `folder.drive_id` server-side when the gap matters.
	const firstFilesSegment = $derived.by(() => {
		const m = /^\/files\/([^/]+)/.exec(appPath(page.url.pathname));
		return m ? m[1] : null;
	});

	// Sorting: default-personal drive first, then secondary personals, then
	// shared. Within each group, by name. Picker UX puts "home" at the top so
	// the common case is one click.
	const sortedDrives = $derived(
		[...drivesStore.drives].sort((a, b) => {
			const rank = (d: Drive) => (d.default_for_user ? 0 : d.kind === 'personal' ? 1 : 2);
			const r = rank(a) - rank(b);
			return r !== 0 ? r : a.name.localeCompare(b.name);
		})
	);

	function isActive(d: Drive): boolean {
		return firstFilesSegment === d.root_folder_id;
	}

	function pctUsed(d: Drive): number | null {
		if (!d.quota_bytes || d.quota_bytes <= 0) return null;
		return Math.min(100, (d.used_bytes / d.quota_bytes) * 100);
	}

	async function open(d: Drive) {
		onnavigate?.();
		// Remember which drive root the user picked so a later click on the
		// sidebar "Files" link (which goes to bare `/files`) returns here
		// instead of always bouncing to the default drive.
		try {
			localStorage.setItem('oxi-last-drive-root', d.root_folder_id);
		} catch {
			/* private mode / quota — silently fall back to default */
		}
		await goto(resolve(`/files/${d.root_folder_id}`));
	}

	onMount(() => {
		void drivesStore.load();
	});
</script>

<!-- Only show the drive switcher when there's an actual choice to make. With a
     single drive (the default personal one) the picker just repeats "Personal"
     under the Files nav row, so hide it; it reappears the moment a second drive
     (e.g. a shared one created from the admin Drives tab) exists. -->
{#if drivesStore.loaded && drivesStore.drives.length > 1}
	<ul class="drive-picker" aria-label={t('drive.picker', 'Drives')}>
		{#each sortedDrives as d (d.id)}
			<li class="drive-picker__row" class:drive-picker__row--active={isActive(d)}>
				<button
					type="button"
					class="drive-picker__item"
					onclick={() => open(d)}
					title={pctUsed(d) !== null
						? `${d.name} — ${formatBytes(d.used_bytes)} / ${formatBytes(d.quota_bytes ?? 0)}`
						: `${d.name} — ${formatBytes(d.used_bytes)}`}
				>
					<Icon name={driveIcon(d)} />
					<span class="drive-picker__name">{d.name}</span>
				</button>
				<a
					href={resolve(`/config/drive/${d.id}`)}
					class="drive-picker__settings"
					title={t('drive.settings_aria', 'Drive settings')}
					aria-label={t('drive.settings_aria', 'Drive settings')}
					onclick={() => onnavigate?.()}
				>
					<Icon name="cog" />
				</a>
				{#if pctUsed(d) !== null}
					<div
						class="drive-picker__bar"
						role="progressbar"
						aria-valuenow={Math.round(pctUsed(d) ?? 0)}
						aria-valuemin="0"
						aria-valuemax="100"
						aria-label={t('drive.usage_aria', 'Drive usage')}
					>
						<div class="drive-picker__bar-fill" style:width="{pctUsed(d)}%"></div>
					</div>
				{/if}
			</li>
		{/each}
	</ul>
{/if}

<style>
	/* Rendered as nested children under the "Files" nav item — no own border or
	   title; visual nesting via left padding aligned to the parent icon. */
	.drive-picker {
		list-style: none;
		padding: 0;
		margin: 0 0 0.25rem;
		display: flex;
		flex-direction: column;
	}

	/* Each row is a small grid: drive button + gear icon on the top line,
	   optional usage bar spanning both columns below. */
	.drive-picker__row {
		display: grid;
		grid-template-columns: 1fr auto;
		align-items: center;
	}

	.drive-picker__item {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		padding: 0.3rem 0.5rem 0.3rem 2rem;
		background: transparent;
		border: none;
		color: var(--color-sidebar-text);
		font: inherit;
		font-size: 0.85rem;
		text-align: left;
		cursor: pointer;
	}

	.drive-picker__item:hover {
		background: var(--color-sidebar-hover-bg);
		color: var(--color-sidebar-text-hover);
	}

	/* Active drive: just a text-color shift. The parent "Files" row already
	   carries the orange-tinted active bg — anything more on the child
	   crowds the sidebar. Typography alone reads as "you are here" since
	   only one drive can be active at a time. */
	.drive-picker__row--active .drive-picker__item {
		color: var(--color-sidebar-text-active);
		font-weight: var(--weight-semibold);
	}

	.drive-picker__settings {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		padding: 0.25rem 0.75rem;
		color: var(--color-sidebar-text);
		opacity: 0.6;
		text-decoration: none;
	}

	.drive-picker__settings:hover {
		opacity: 1;
		color: var(--color-sidebar-text-hover);
	}

	.drive-picker__name {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Mini usage bar tucked under the row, indented to align with the name,
	   spans both grid columns so the gear icon sits above its right edge. */
	.drive-picker__bar {
		grid-column: 1 / -1;
		height: 3px;
		background: var(--color-sidebar-storage-bar);
		border-radius: 1.5px;
		margin: 0 1rem 0.25rem 2rem;
		overflow: hidden;
	}

	.drive-picker__bar-fill {
		height: 100%;
		background: var(--color-accent);
		transition: width 200ms ease;
	}
</style>
