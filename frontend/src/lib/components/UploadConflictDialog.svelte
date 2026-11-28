<!--
@component UploadConflictDialog
Summary resolver shown when a file or folder upload hits name/content conflicts
with existing server items (`content` diffs, read-only targets, or file/folder
kind mismatches). Never touches remote bytes until the user confirms; identical
files are auto-skipped before this modal opens and summarized at the top.
-->
<script lang="ts">
	import Button from '$lib/components/Button.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { formatDate, iconNameFor } from '$lib/utils/display';
	import { formatBytes } from '$lib/utils/format';
	import {
		applyBulkAction,
		defaultResolutionFor,
		type BulkConflictAction,
		type ConflictResolution,
		type UploadConflictItem
	} from '$lib/upload/conflictPlan';

	interface Props {
		open: boolean;
		conflicts: ReadonlyArray<UploadConflictItem>;
		newCount: number;
		identicalCount: number;
		onresolve: (resolutions: Record<string, ConflictResolution>) => void;
		oncancel: () => void;
	}

	let {
		open = $bindable(false),
		conflicts,
		newCount,
		identicalCount,
		onresolve,
		oncancel
	}: Props = $props();

	let resolutions = $state<Record<string, ConflictResolution>>({});

	$effect(() => {
		if (!open) return;
		const next: Record<string, ConflictResolution> = {};
		for (const item of conflicts) {
			next[item.id] = defaultResolutionFor(item);
		}
		resolutions = next;
	});

	const canReplaceAny = $derived(conflicts.some((c) => c.canReplace));
	const canReplaceIfNewer = $derived(
		conflicts.some((c) => c.canReplace && c.comparison === 'local_newer')
	);

	const activeConflictUploads = $derived(
		conflicts.reduce((acc, c) => {
			const choice = resolutions[c.id] ?? 'skip';
			return choice === 'skip' ? acc : acc + 1;
		}, 0)
	);

	const totalToUpload = $derived(newCount + activeConflictUploads);

	function applyBulk(action: BulkConflictAction) {
		resolutions = applyBulkAction(conflicts, action);
	}

	function setRowResolution(id: string, value: ConflictResolution) {
		resolutions = { ...resolutions, [id]: value };
	}

	function handleClose() {
		open = false;
		oncancel();
	}

	function handleConfirm() {
		const snapshot: Record<string, ConflictResolution> = {};
		for (const item of conflicts) {
			snapshot[item.id] = resolutions[item.id] ?? 'skip';
		}
		open = false;
		onresolve(snapshot);
	}
</script>

<Modal
	bind:open
	size="lg"
	title={t('files.upload_conflict.title', { n: conflicts.length }, 'Upload conflicts ({{n}})')}
	onclose={oncancel}
>
	<div class="upload-conflict" data-testid="upload-conflict-dialog">
		<p class="upload-conflict__intro">
			{t(
				'files.upload_conflict.intro',
				'Some items already exist in this folder with different content.'
			)}
		</p>

		<div class="upload-conflict__summary" data-testid="upload-conflict-summary">
			{#if newCount > 0}
				<span class="upload-conflict__pill upload-conflict__pill--new">
					<Icon name="cloud-upload-alt" />
					{t('files.upload_conflict.summary_new', { n: newCount }, '{{n}} new')}
				</span>
			{/if}
			{#if identicalCount > 0}
				<span class="upload-conflict__pill upload-conflict__pill--identical">
					<Icon name="check" />
					{t(
						'files.upload_conflict.summary_identical',
						{ n: identicalCount },
						'{{n}} unchanged (auto-skipped)'
					)}
				</span>
			{/if}
			<span class="upload-conflict__pill upload-conflict__pill--conflict">
				<Icon name="exclamation-triangle" />
				{t('files.upload_conflict.summary_conflicts', { n: conflicts.length }, '{{n}} conflict(s)')}
			</span>
		</div>

		<div class="upload-conflict__bulk" role="toolbar" aria-label="Bulk conflict actions">
			<button
				type="button"
				class="upload-conflict__bulk-btn"
				data-testid="upload-conflict-bulk-skip"
				onclick={() => applyBulk('skip_all')}
			>
				<Icon name="forward" />
				<span>{t('files.upload_conflict.skip_all', 'Skip all')}</span>
			</button>
			<button
				type="button"
				class="upload-conflict__bulk-btn"
				data-testid="upload-conflict-bulk-newer"
				disabled={!canReplaceIfNewer}
				onclick={() => applyBulk('replace_if_newer')}
			>
				<Icon name="clock" />
				<span>{t('files.upload_conflict.replace_if_newer', 'Replace if newer')}</span>
			</button>
			<button
				type="button"
				class="upload-conflict__bulk-btn"
				data-testid="upload-conflict-bulk-replace"
				disabled={!canReplaceAny}
				onclick={() => applyBulk('replace_all')}
			>
				<Icon name="exchange-alt" />
				<span>{t('files.upload_conflict.replace_all', 'Replace all')}</span>
			</button>
			<button
				type="button"
				class="upload-conflict__bulk-btn"
				data-testid="upload-conflict-bulk-keep-both"
				onclick={() => applyBulk('keep_both_all')}
			>
				<Icon name="copy" />
				<span>{t('files.upload_conflict.keep_both_all', 'Keep both')}</span>
			</button>
		</div>

		<ul class="upload-conflict__list">
			{#each conflicts as item (item.id)}
				{@const chosen = resolutions[item.id] ?? 'skip'}
				{@const icon =
					item.kind === 'kind_mismatch' && item.blockingKind === 'folder'
						? 'folder'
						: iconNameFor(item.relativePath, item.file.type)}
				<li
					class="upload-conflict__row"
					class:upload-conflict__row--readonly={!item.canReplace}
					data-testid="upload-conflict-row"
				>
					<div class="upload-conflict__row-main">
						<div class="upload-conflict__row-title">
							<Icon name={icon} class="upload-conflict__row-icon" />
							<span class="upload-conflict__row-path" title={item.relativePath}>
								{item.relativePath}
							</span>
							{#if item.kind === 'kind_mismatch'}
								<span class="upload-conflict__badge upload-conflict__badge--error">
									<Icon name="ban" />
									{t('files.upload_conflict.badge_kind_mismatch', 'Folder/file conflict')}
								</span>
							{:else if !item.canReplace}
								<span class="upload-conflict__badge upload-conflict__badge--muted">
									<Icon name="lock" />
									{t('files.upload_conflict.badge_readonly', 'Read-only')}
								</span>
							{:else if item.comparison === 'local_newer'}
								<span class="upload-conflict__badge upload-conflict__badge--accent">
									{t('files.upload_conflict.badge_local_newer', 'Local newer')}
								</span>
							{:else if item.comparison === 'remote_newer'}
								<span class="upload-conflict__badge upload-conflict__badge--warn">
									{t('files.upload_conflict.badge_remote_newer', 'Server newer')}
								</span>
							{:else if item.comparison === 'same'}
								<span class="upload-conflict__badge upload-conflict__badge--muted">
									{t('files.upload_conflict.badge_same_mtime', 'Same modified time')}
								</span>
							{/if}
						</div>

						{#if item.kind === 'kind_mismatch'}
							<p class="upload-conflict__row-meta">
								{t(
									'files.upload_conflict.kind_mismatch_hint',
									{ path: item.blockingPath ?? item.relativePath },
									'Cannot overwrite "{{path}}" because the server already has a different item type there. Choose Skip or Keep both.'
								)}
							</p>
						{:else if item.remote}
							<p class="upload-conflict__row-meta">
								<span>
									{#if item.localModifiedMs}
										{t(
											'files.upload_conflict.local_label',
											{
												size: formatBytes(item.localSize),
												date: formatDate(item.localModifiedMs)
											},
											'This device: {{size}} · {{date}}'
										)}
									{:else}
										{t(
											'files.upload_conflict.local_label_nodate',
											{ size: formatBytes(item.localSize) },
											'This device: {{size}}'
										)}
									{/if}
								</span>
								<span aria-hidden="true">·</span>
								<span>
									{t(
										'files.upload_conflict.remote_label',
										{
											size: formatBytes(item.remote.size),
											date: formatDate(item.remote.modifiedAt)
										},
										'Server: {{size}} · {{date}}'
									)}
								</span>
							</p>
						{/if}

						{#if chosen === 'keep_both'}
							<p class="upload-conflict__row-hint">
								<Icon name="info-circle" />
								<span>
									{t(
										'files.upload_conflict.keep_both_hint',
										{ name: item.keepBothPreviewName },
										'Saved alongside as e.g. {{name}}'
									)}
								</span>
							</p>
						{/if}
					</div>

					<div class="upload-conflict__choices" role="radiogroup" aria-label={item.relativePath}>
						<button
							type="button"
							role="radio"
							aria-checked={chosen === 'skip'}
							class="upload-conflict__choice"
							class:upload-conflict__choice--active={chosen === 'skip'}
							data-testid="upload-conflict-choice-skip"
							onclick={() => setRowResolution(item.id, 'skip')}
						>
							{t('files.upload_conflict.action_skip', 'Skip')}
						</button>
						<button
							type="button"
							role="radio"
							aria-checked={chosen === 'replace'}
							class="upload-conflict__choice"
							class:upload-conflict__choice--active={chosen === 'replace'}
							disabled={!item.canReplace}
							data-testid="upload-conflict-choice-replace"
							onclick={() => setRowResolution(item.id, 'replace')}
						>
							{t('files.upload_conflict.action_replace', 'Replace')}
						</button>
						<button
							type="button"
							role="radio"
							aria-checked={chosen === 'keep_both'}
							class="upload-conflict__choice"
							class:upload-conflict__choice--active={chosen === 'keep_both'}
							data-testid="upload-conflict-choice-keep-both"
							onclick={() => setRowResolution(item.id, 'keep_both')}
						>
							{t('files.upload_conflict.action_keep_both', 'Keep both')}
						</button>
					</div>
				</li>
			{/each}
		</ul>
	</div>

	{#snippet footer()}
		<Button variant="secondary" data-testid="upload-conflict-cancel" onclick={handleClose}>
			{t('common.cancel', 'Cancel')}
		</Button>
		<Button variant="primary" data-testid="upload-conflict-confirm" onclick={handleConfirm}>
			{#if totalToUpload > 0}
				{t(
					'files.upload_conflict.confirm_continue',
					{ n: totalToUpload },
					'Continue ({{n}} to upload)'
				)}
			{:else}
				{t('files.upload_conflict.confirm_skip_all', 'Skip all conflicts')}
			{/if}
		</Button>
	{/snippet}
</Modal>

<style>
	.upload-conflict {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.upload-conflict__intro {
		margin: 0;
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		line-height: 1.5;
	}

	.upload-conflict__summary {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
	}

	.upload-conflict__pill {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: 2px var(--space-2);
		border-radius: var(--radius-full);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
	}

	.upload-conflict__pill--new {
		background: var(--color-accent-tint);
		color: var(--color-accent-text);
	}

	.upload-conflict__pill--identical {
		background: var(--color-success-bg);
		color: var(--color-success-text);
	}

	.upload-conflict__pill--conflict {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.upload-conflict__bulk {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
	}

	.upload-conflict__bulk-btn {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: var(--space-1) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-subtle);
		color: var(--color-text);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		cursor: pointer;
	}

	.upload-conflict__bulk-btn:hover:not(:disabled) {
		background: var(--color-bg-hover);
		border-color: var(--color-border-medium);
	}

	.upload-conflict__bulk-btn:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}

	.upload-conflict__list {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 48vh;
		overflow-y: auto;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm);
	}

	.upload-conflict__row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		padding: var(--space-3);
		border-bottom: 1px solid var(--color-border-subtle);
	}

	.upload-conflict__row:last-child {
		border-bottom: none;
	}

	.upload-conflict__row--readonly {
		background: var(--color-bg-subtle);
	}

	.upload-conflict__row-main {
		flex: 1 1 16rem;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}

	.upload-conflict__row-title {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		min-width: 0;
	}

	.upload-conflict__row-path {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: var(--text-sm);
		font-weight: var(--weight-medium);
		color: var(--color-text-heading);
	}

	.upload-conflict__row-meta {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin: 0;
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}

	.upload-conflict__row-hint {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		margin: 0;
		font-size: var(--text-xs);
		color: var(--color-accent-text);
	}

	.upload-conflict__badge {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: 1px var(--space-2);
		border-radius: var(--radius-full);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
	}

	.upload-conflict__badge--accent {
		background: var(--color-accent-tint);
		color: var(--color-accent-text);
	}

	.upload-conflict__badge--warn {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.upload-conflict__badge--error {
		background: var(--color-error-bg);
		color: var(--color-error-text);
	}

	.upload-conflict__badge--muted {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.upload-conflict__choices {
		display: inline-flex;
		flex-shrink: 0;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		overflow: hidden;
		background: var(--color-bg-surface);
	}

	.upload-conflict__choice {
		padding: var(--space-1) var(--space-2);
		border: none;
		border-right: 1px solid var(--color-border-subtle);
		background: transparent;
		color: var(--color-text-secondary);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		cursor: pointer;
	}

	.upload-conflict__choice:last-child {
		border-right: none;
	}

	.upload-conflict__choice:hover:not(:disabled) {
		background: var(--color-bg-hover);
	}

	.upload-conflict__choice--active {
		background: var(--color-accent-tint);
		color: var(--color-accent-text);
	}

	.upload-conflict__choice:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
</style>
