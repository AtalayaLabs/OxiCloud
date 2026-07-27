<script lang="ts">
	import { errorToast } from '$lib/utils/errors';
	import { listFolder, moveFolder } from '$lib/api/endpoints/folders';
	import { moveFile } from '$lib/api/endpoints/files';
	import { copyFiles, copyFolders } from '$lib/api/endpoints/batch';
	import type { Drive, DriveRole, FolderItem } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { drives as drivesStore, driveIcon } from '$lib/stores/drives.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	// A drive accepts new items only if the caller can Create on its
	// root. Drive-scope roles are the management-ladder subset —
	// Owner / Editor / Viewer — so writability collapses to the top two;
	// Viewer cannot. `contributor`/`commenter` don't appear at drive
	// scope (folder/file-scope semantics), so they're not in `DriveRole`.
	const WRITABLE_ROLES: readonly DriveRole[] = ['owner', 'editor'] as const;
	function isWritable(d: Drive): boolean {
		return d.caller_role != null && WRITABLE_ROLES.includes(d.caller_role);
	}

	// Default-personal first, then secondary personals, then shared; within
	// a group, alphabetical. Mirrors DrivePicker so the sidebar and this
	// dialog rank drives identically.
	function driveRank(d: Drive): number {
		if (d.default_for_user) return 0;
		return d.kind === 'personal' ? 1 : 2;
	}

	interface Target {
		id: string;
		name: string;
		kind: 'file' | 'folder';
	}

	interface Props {
		open: boolean;
		item: Target | null;
		/** Optional multi-item batch; takes precedence over `item`. */
		items?: Target[] | null;
		/** 'move' (default) relocates; 'copy' duplicates into the picked folder. */
		mode?: 'move' | 'copy';
		onmoved?: () => void;
	}

	let { open = $bindable(false), item, items = null, mode = 'move', onmoved }: Props = $props();

	const targets = $derived(items && items.length ? items : item ? [item] : []);
	const targetIds = $derived(new Set(targets.map((x) => x.id)));

	let crumbs = $state<Array<{ id: string; name: string }>>([]);
	let folders = $state<FolderItem[]>([]);
	let currentId = $state<string | null>(null);
	let selectedDriveId = $state<string | null>(null);
	let loading = $state(false);
	let working = $state(false);

	const writableDrives = $derived(
		[...drivesStore.drives].filter(isWritable).sort((a, b) => {
			const r = driveRank(a) - driveRank(b);
			return r !== 0 ? r : a.name.localeCompare(b.name);
		})
	);

	// The chip strip only earns its vertical space when there's a real
	// choice. One writable drive → identical to the single-drive UI.
	const showDriveSwitcher = $derived(writableDrives.length > 1);

	async function loadInto(id: string) {
		loading = true;
		try {
			currentId = id;
			folders = (await listFolder(id)).folders;
		} catch (e) {
			errorToast(e);
		} finally {
			loading = false;
		}
	}

	async function init() {
		await drivesStore.load();
		const home = drivesStore.findDefault();
		// Prefer the user's home drive when it's writable (covers the
		// common case: moving stuff around inside Personal). Otherwise
		// fall back to the first writable drive, sorted as above.
		const start = home && isWritable(home) ? home : writableDrives[0];
		if (!start) return;
		selectedDriveId = start.id;
		crumbs = [{ id: start.root_folder_id, name: start.name }];
		await loadInto(start.root_folder_id);
	}

	async function switchDrive(d: Drive) {
		if (d.id === selectedDriveId) return;
		selectedDriveId = d.id;
		crumbs = [{ id: d.root_folder_id, name: d.name }];
		await loadInto(d.root_folder_id);
	}

	function enter(f: FolderItem) {
		crumbs = [...crumbs, { id: f.id, name: f.name }];
		void loadInto(f.id);
	}

	function gotoCrumb(index: number) {
		crumbs = crumbs.slice(0, index + 1);
		void loadInto(crumbs[index].id);
	}

	/** Jump to the home (root) folder — the first crumb. */
	function goHome() {
		if (crumbs.length) gotoCrumb(0);
	}

	/** Step up one level to the parent folder (no-op at home). */
	function goParent() {
		if (crumbs.length > 1) gotoCrumb(crumbs.length - 2);
	}

	const atHome = $derived(crumbs.length <= 1);

	async function confirmMove() {
		if (!targets.length || !currentId) return;
		working = true;
		try {
			if (mode === 'copy') {
				const fileIds = targets.filter((x) => x.kind === 'file').map((x) => x.id);
				const folderIds = targets.filter((x) => x.kind === 'folder').map((x) => x.id);
				await copyFiles(fileIds, currentId);
				await copyFolders(folderIds, currentId);
				ui.notify(t('files.copied', 'Copied'), 'success');
			} else {
				for (const tgt of targets) {
					if (tgt.id === currentId) continue;
					if (tgt.kind === 'file') await moveFile(tgt.id, currentId);
					else await moveFolder(tgt.id, currentId);
				}
				ui.notify(t('files.moved', 'Moved'), 'success');
			}
			open = false;
			onmoved?.();
		} catch (e) {
			errorToast(e);
		} finally {
			working = false;
		}
	}

	// (Re)initialise the picker each time it opens.
	$effect(() => {
		if (open && targets.length) void init();
	});

	const moveTitle = $derived.by(() => {
		if (mode === 'copy') {
			return targets.length > 1
				? t('files.copy_n', { n: targets.length }, 'Copy {{n}} items')
				: t('files.copy_title', { name: targets[0]?.name ?? '' }, 'Copy “{{name}}”');
		}
		return targets.length > 1
			? t('files.move_n', { n: targets.length }, 'Move {{n}} items')
			: t('files.move_title', { name: targets[0]?.name ?? '' }, 'Move “{{name}}”');
	});
</script>

<Modal bind:open title={moveTitle}>
	<div data-testid="move-dialog">
		{#if showDriveSwitcher}
			<div
				class="mv-drives"
				role="tablist"
				aria-label={t('drive.picker', 'Drives')}
				data-testid="move-dialog-drives"
			>
				{#each writableDrives as d (d.id)}
					<button
						type="button"
						role="tab"
						aria-selected={d.id === selectedDriveId}
						class="mv-drive"
						class:mv-drive--active={d.id === selectedDriveId}
						data-testid={`move-dialog-drive-${d.id}`}
						onclick={() => switchDrive(d)}
					>
						<Icon name={driveIcon(d)} />
						<span>{d.name}</span>
					</button>
				{/each}
			</div>
		{/if}

		<div class="mv-nav">
			<button
				class="mv-nav-btn"
				data-testid="move-dialog-home-btn"
				title={t('breadcrumb.home', 'Home')}
				aria-label={t('breadcrumb.home', 'Home')}
				disabled={atHome}
				onclick={goHome}><Icon name="home" /></button
			>
			<button
				class="mv-nav-btn"
				data-testid="move-dialog-parent-btn"
				title={t('dialogs.go_to_parent', 'Go to parent')}
				aria-label={t('dialogs.go_to_parent', 'Go to parent')}
				disabled={atHome}
				onclick={goParent}><Icon name="level-up-alt" /></button
			>
			<nav class="mv-crumbs" aria-label="Breadcrumb">
				{#each crumbs as c, i (c.id)}
					{#if i > 0}<span class="mv-sep">/</span>{/if}
					<button
						class="mv-crumb"
						data-testid={`move-dialog-crumb-${c.id}`}
						onclick={() => gotoCrumb(i)}>{c.name}</button
					>
				{/each}
			</nav>
		</div>

		{#if loading}
			<p class="mv-status">{t('common.loading', 'Loading…')}</p>
		{:else if folders.length === 0}
			<p class="mv-status">{t('files.no_subfolders', 'No subfolders here.')}</p>
		{:else}
			<ul class="mv-list">
				{#each folders as f (f.id)}
					<li>
						<button
							class="mv-folder"
							data-testid={`move-dialog-folder-${f.id}`}
							disabled={targetIds.has(f.id)}
							onclick={() => enter(f)}
						>
							<Icon name="folder" /> <span>{f.name}</span>
							<Icon name="chevron-right" class="mv-enter" />
						</button>
					</li>
				{/each}
			</ul>
		{/if}
	</div>

	{#snippet footer()}
		<button
			class="btn btn-secondary"
			data-testid="move-dialog-cancel-btn"
			onclick={() => (open = false)}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<button
			class="btn btn-primary"
			data-testid="move-dialog-confirm-btn"
			disabled={working || !currentId}
			onclick={confirmMove}
		>
			{mode === 'copy' ? t('files.copy_here', 'Copy here') : t('files.move_here', 'Move here')}
		</button>
	{/snippet}
</Modal>

<style>
	/* Drive switcher: a chip strip across the top of the dialog. Hidden
	   when only one writable drive is in scope (single-drive UX). */
	.mv-drives {
		display: flex;
		flex-wrap: wrap;
		gap: 0.375rem;
		margin-bottom: var(--space-3);
		padding-bottom: var(--space-3);
		border-bottom: 1px solid var(--color-border);
	}

	.mv-drive {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		padding: 0.3rem 0.625rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font: inherit;
		font-size: 0.85rem;
		cursor: pointer;
		max-width: 14rem;
	}

	.mv-drive:hover:not(.mv-drive--active) {
		background: var(--color-bg-hover);
	}

	.mv-drive--active {
		background: var(--color-accent);
		color: var(--color-on-accent);
		border-color: var(--color-accent);
		cursor: default;
	}

	.mv-drive span {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.mv-nav {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		margin-bottom: var(--space-3);
	}

	.mv-nav-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 28px;
		height: 28px;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		cursor: pointer;
		flex: none;
	}

	.mv-nav-btn:hover:not(:disabled) {
		background: var(--color-bg-hover);
	}

	.mv-nav-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}

	.mv-crumbs {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.25rem;
		min-width: 0;
	}

	.mv-crumb {
		background: none;
		border: none;
		color: var(--color-accent-text, var(--color-primary));
		cursor: pointer;
		padding: 0.125rem 0.25rem;
	}

	.mv-sep {
		color: var(--color-text-muted);
	}

	.mv-list {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 50vh;
		overflow: auto;
	}

	.mv-folder {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		width: 100%;
		padding: 0.5rem 0.625rem;
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		border-radius: var(--radius-md);
		text-align: left;
	}

	.mv-folder:hover:not(:disabled) {
		background: var(--color-bg-hover);
	}

	.mv-folder:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}

	.mv-folder span {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	:global(.mv-enter) {
		color: var(--color-text-muted);
	}

	.mv-status {
		color: var(--color-text-muted);
		padding: 1rem 0;
		text-align: center;
	}
</style>
