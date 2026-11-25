<!--
	@component TransferConflictDialog

	Single shared Move/Copy conflict-resolution modal consumed via
	`$lib/stores/transferConflict.svelte.ts` (`promptTransferConflicts`).
	Reused by `MoveDialog`, `/files` folder-row/breadcrumb drop, and `AppShell`
	sidebar drive drop so all four transfer paths share identical UX: recursive folder
	Merge / Keep both / Skip, inline nested collisions when Merge is selected,
	file Skip / Replace (CAS) / Keep both with read-only & mtime badges, and
	kind-mismatch Skip / Keep both.
-->
<script lang="ts">
	import { SvelteSet } from 'svelte/reactivity';
	import Button from '$lib/components/Button.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { transferConflictStore } from '$lib/stores/transferConflict.svelte';
	import type {
		ItemKind,
		TransferDecisionAction,
		TransferFileConflictDto,
		TransferKindConflictDto
	} from '$lib/api/endpoints/transfer';
	import {
		applyTransferBulkAction,
		buildTransferDecisions,
		canReplaceAnyFile,
		compareTransferMtime,
		countActiveTopLevelTransfers,
		countIdenticalNestedFiles,
		formatTransferKeepBothPreview,
		hasAnyFolderConflict,
		initialTransferResolutions,
		nestedConflictsForFolder,
		topLevelFileConflicts,
		topLevelKindConflicts,
		transferConflictKey
	} from '$lib/transfer/transferPlan';
	import { formatBytes } from '$lib/utils/format';

	const current = $derived(transferConflictStore.current);
	let lastPromptRef: object | null = null;
	let resolutions = $state<Record<string, TransferDecisionAction>>({});
	let expandedFolders = new SvelteSet<string>();

	// Initialize before first paint (`$effect.pre`) whenever a new conflict prompt becomes active.
	$effect.pre(() => {
		const active = current;
		if (active && active !== lastPromptRef) {
			lastPromptRef = active;
			resolutions = initialTransferResolutions(active.preflight);
			expandedFolders.clear();
			for (const dc of active.preflight.folder_conflicts) {
				const nested = nestedConflictsForFolder(active.preflight, dc.source_id);
				const totalNested = nested.files.length + nested.kinds.length;
				if (totalNested > 0 && totalNested <= 8) {
					expandedFolders.add(dc.source_id);
				}
			}
		} else if (!active) {
			lastPromptRef = null;
		}
	});

	const topFiles = $derived(current ? topLevelFileConflicts(current.preflight) : []);
	const topKinds = $derived(current ? topLevelKindConflicts(current.preflight) : []);
	const showFolderBulk = $derived(current ? hasAnyFolderConflict(current.preflight) : false);
	const showReplaceBulk = $derived(current ? canReplaceAnyFile(current.preflight) : false);
	const identicalCount = $derived(current ? countIdenticalNestedFiles(current.preflight) : 0);

	const totalConflicts = $derived(
		current ? current.preflight.folder_conflicts.length + topFiles.length + topKinds.length : 0
	);

	const activeCount = $derived(
		current
			? countActiveTopLevelTransfers(
					current.preflight,
					current.totalItems,
					resolutions,
					current.operation
				)
			: 0
	);

	const modalTitle = $derived.by(() => {
		if (!current) return '';
		const key =
			current.operation === 'copy'
				? 'files.transfer_conflict.title_copy'
				: 'files.transfer_conflict.title_move';
		const fallback =
			current.operation === 'copy' ? 'Copy conflicts ({{n}})' : 'Move conflicts ({{n}})';
		return t(key, { n: totalConflicts }, fallback);
	});

	const confirmLabel = $derived.by(() => {
		if (!current) return '';
		if (activeCount === 0) {
			return t('files.transfer_conflict.confirm_skip_all', 'Skip all conflicts');
		}
		if (current.operation === 'copy') {
			return t(
				'files.transfer_conflict.confirm_copy',
				{ n: activeCount },
				'Continue ({{n}} to copy)'
			);
		}
		return t(
			'files.transfer_conflict.confirm_move',
			{ n: activeCount },
			'Continue ({{n}} to move)'
		);
	});

	function formatDateTime(iso: string): string {
		const ms = Date.parse(iso);
		if (Number.isNaN(ms)) return iso;
		try {
			return new Intl.DateTimeFormat(undefined, {
				dateStyle: 'medium',
				timeStyle: 'short'
			}).format(new Date(ms));
		} catch {
			return iso;
		}
	}

	function setAction(kind: ItemKind, sourceId: string, action: TransferDecisionAction) {
		resolutions = {
			...resolutions,
			[transferConflictKey(kind, sourceId)]: action
		};
	}

	function toggleExpanded(folderId: string) {
		if (expandedFolders.has(folderId)) expandedFolders.delete(folderId);
		else expandedFolders.add(folderId);
	}

	function runBulk(
		bulk: 'skip_all' | 'keep_both_all' | 'replace_all' | 'replace_if_newer' | 'merge_all_folders'
	) {
		if (!current) return;
		resolutions = applyTransferBulkAction(current.preflight, resolutions, bulk);
	}

	function handleConfirm() {
		if (!current) return;
		const decisions = buildTransferDecisions(current.preflight, resolutions, current.operation);
		transferConflictStore.confirm(decisions);
	}

	function handleCancel() {
		transferConflictStore.cancel();
	}
</script>

{#snippet fileConflictRow(fc: TransferFileConflictDto, nested: boolean)}
	{@const key = transferConflictKey('file', fc.source_id)}
	{@const choice = resolutions[key] ?? 'skip'}
	{@const mtimeCmp = compareTransferMtime(fc)}
	{@const allowReplace = fc.can_update_target}
	<li
		class="transfer-conflict__row"
		class:transfer-conflict__row--nested={nested}
		data-testid={`transfer-conflict-file-${fc.source_id}`}
	>
		<div class="transfer-conflict__row-main">
			<div class="transfer-conflict__path-line">
				<span class="transfer-conflict__type-icon" aria-hidden="true">
					<Icon name="file" />
				</span>
				<span class="transfer-conflict__path" title={fc.relative_path}>
					{nested ? fc.source_name : fc.relative_path}
				</span>
				{#if fc.identical}
					<span class="transfer-conflict__badge transfer-conflict__badge--success">
						{t('files.transfer_conflict.badge_identical', 'Identical content')}
					</span>
				{/if}
				{#if mtimeCmp === 'local_newer'}
					<span class="transfer-conflict__badge transfer-conflict__badge--info">
						{t('files.transfer_conflict.badge_source_newer', 'Source newer')}
					</span>
				{:else if mtimeCmp === 'remote_newer'}
					<span class="transfer-conflict__badge transfer-conflict__badge--warn">
						{t('files.transfer_conflict.badge_target_newer', 'Destination newer')}
					</span>
				{:else if mtimeCmp === 'same'}
					<span class="transfer-conflict__badge transfer-conflict__badge--muted">
						{t('files.transfer_conflict.badge_same_mtime', 'Same modified time')}
					</span>
				{/if}
				{#if !fc.can_update_target}
					<span class="transfer-conflict__badge transfer-conflict__badge--error">
						<Icon name="lock" />
						{t('files.transfer_conflict.badge_readonly', 'Read-only')}
					</span>
				{/if}
			</div>

			<div class="transfer-conflict__meta">
				<span>
					{t(
						'files.transfer_conflict.source_label',
						{
							size: formatBytes(fc.source_size),
							date: formatDateTime(fc.source_modified_at)
						},
						'Moving/copying: {{size}} · {{date}}'
					)}
				</span>
				<span>
					{t(
						'files.transfer_conflict.target_label',
						{
							size: formatBytes(fc.target_size),
							date: formatDateTime(fc.target_modified_at)
						},
						'Destination: {{size}} · {{date}}'
					)}
				</span>
			</div>

			{#if choice === 'keep_both'}
				<div class="transfer-conflict__hint">
					{t(
						'files.transfer_conflict.keep_both_hint',
						{ name: formatTransferKeepBothPreview(fc.source_name, 'file') },
						'Kept alongside as {{name}}'
					)}
				</div>
			{/if}
		</div>

		<div class="transfer-conflict__actions" role="radiogroup" aria-label={fc.relative_path}>
			<button
				type="button"
				role="radio"
				aria-checked={choice === 'skip'}
				class="transfer-conflict__choice"
				class:transfer-conflict__choice--active={choice === 'skip'}
				onclick={() => setAction('file', fc.source_id, 'skip')}
			>
				{t('files.transfer_conflict.action_skip', 'Skip')}
			</button>
			<button
				type="button"
				role="radio"
				aria-checked={choice === 'replace'}
				disabled={!allowReplace}
				class="transfer-conflict__choice"
				class:transfer-conflict__choice--active={choice === 'replace'}
				onclick={() => setAction('file', fc.source_id, 'replace')}
			>
				{t('files.transfer_conflict.action_replace', 'Replace')}
			</button>
			<button
				type="button"
				role="radio"
				aria-checked={choice === 'keep_both'}
				class="transfer-conflict__choice"
				class:transfer-conflict__choice--active={choice === 'keep_both'}
				onclick={() => setAction('file', fc.source_id, 'keep_both')}
			>
				{t('files.transfer_conflict.action_keep_both', 'Keep both')}
			</button>
		</div>
	</li>
{/snippet}

{#snippet kindConflictRow(kc: TransferKindConflictDto, nested: boolean)}
	{@const key = transferConflictKey(kc.source_kind, kc.source_id)}
	{@const choice = resolutions[key] ?? 'skip'}
	<li
		class="transfer-conflict__row"
		class:transfer-conflict__row--nested={nested}
		data-testid={`transfer-conflict-kind-${kc.source_id}`}
	>
		<div class="transfer-conflict__row-main">
			<div class="transfer-conflict__path-line">
				<span class="transfer-conflict__type-icon" aria-hidden="true">
					<Icon name={kc.source_kind === 'folder' ? 'folder' : 'file'} />
				</span>
				<span class="transfer-conflict__path" title={kc.relative_path}>
					{nested ? kc.source_name : kc.relative_path}
				</span>
				<span class="transfer-conflict__badge transfer-conflict__badge--error">
					{t('files.transfer_conflict.badge_kind_mismatch', 'Folder/file conflict')}
				</span>
			</div>
			<div class="transfer-conflict__hint">
				{t(
					'files.transfer_conflict.kind_mismatch_hint',
					{ path: kc.relative_path },
					'Cannot overwrite "{{path}}" because the destination contains a different item type. Choose Skip or Keep both.'
				)}
			</div>
			{#if choice === 'keep_both'}
				<div class="transfer-conflict__hint">
					{t(
						'files.transfer_conflict.keep_both_hint',
						{ name: formatTransferKeepBothPreview(kc.source_name, kc.source_kind) },
						'Kept alongside as {{name}}'
					)}
				</div>
			{/if}
		</div>

		<div class="transfer-conflict__actions" role="radiogroup" aria-label={kc.relative_path}>
			<button
				type="button"
				role="radio"
				aria-checked={choice === 'skip'}
				class="transfer-conflict__choice"
				class:transfer-conflict__choice--active={choice === 'skip'}
				onclick={() => setAction(kc.source_kind, kc.source_id, 'skip')}
			>
				{t('files.transfer_conflict.action_skip', 'Skip')}
			</button>
			<button
				type="button"
				role="radio"
				aria-checked={choice === 'keep_both'}
				class="transfer-conflict__choice"
				class:transfer-conflict__choice--active={choice === 'keep_both'}
				onclick={() => setAction(kc.source_kind, kc.source_id, 'keep_both')}
			>
				{t('files.transfer_conflict.action_keep_both', 'Keep both')}
			</button>
		</div>
	</li>
{/snippet}

{#if current}
	<Modal open={true} size="lg" title={modalTitle} onclose={handleCancel}>
		<div class="transfer-conflict" data-testid="transfer-conflict-dialog">
			<p class="transfer-conflict__intro">
				{#if current.targetName}
					{t(
						current.operation === 'copy'
							? 'files.transfer_conflict.intro_copy'
							: 'files.transfer_conflict.intro_move',
						{ target: current.targetName },
						'Some items already exist in "{{target}}". Choose how to handle each conflict.'
					)}
				{:else}
					{t(
						'files.transfer_conflict.intro_generic',
						'Some items already exist in the destination folder. Choose how to handle each conflict.'
					)}
				{/if}
			</p>

			<div class="transfer-conflict__summary">
				{#if current.preflight.folder_conflicts.length > 0}
					<span class="transfer-conflict__pill transfer-conflict__pill--warn">
						{t(
							'files.transfer_conflict.summary_folders',
							{ n: current.preflight.folder_conflicts.length },
							'{{n}} folder conflict(s)'
						)}
					</span>
				{/if}
				{#if current.preflight.file_conflicts.length > 0}
					<span class="transfer-conflict__pill transfer-conflict__pill--info">
						{t(
							'files.transfer_conflict.summary_files',
							{ n: current.preflight.file_conflicts.length },
							'{{n}} file conflict(s)'
						)}
					</span>
				{/if}
				{#if current.preflight.kind_conflicts.length > 0}
					<span class="transfer-conflict__pill transfer-conflict__pill--error">
						{t(
							'files.transfer_conflict.summary_kind',
							{ n: current.preflight.kind_conflicts.length },
							'{{n}} type conflict(s)'
						)}
					</span>
				{/if}
				{#if identicalCount > 0}
					<span class="transfer-conflict__pill transfer-conflict__pill--muted">
						{t(
							'files.transfer_conflict.summary_identical',
							{ n: identicalCount },
							'{{n}} identical file(s)'
						)}
					</span>
				{/if}
			</div>

			<div class="transfer-conflict__bulk">
				<Button small onclick={() => runBulk('skip_all')}>
					{t('files.transfer_conflict.bulk_skip_all', 'Skip all')}
				</Button>
				{#if showFolderBulk}
					<Button small onclick={() => runBulk('merge_all_folders')}>
						{t('files.transfer_conflict.bulk_merge_folders', 'Merge all folders')}
					</Button>
				{/if}
				{#if showReplaceBulk}
					<Button small onclick={() => runBulk('replace_if_newer')}>
						{t('files.transfer_conflict.bulk_replace_if_newer', 'Replace if newer')}
					</Button>
					<Button small onclick={() => runBulk('replace_all')}>
						{t('files.transfer_conflict.bulk_replace_all', 'Replace all')}
					</Button>
				{/if}
				<Button small onclick={() => runBulk('keep_both_all')}>
					{t('files.transfer_conflict.bulk_keep_both', 'Keep both')}
				</Button>
			</div>

			<ul class="transfer-conflict__list">
				{#each current.preflight.folder_conflicts as dc (dc.source_id)}
					{@const key = transferConflictKey('folder', dc.source_id)}
					{@const choice = resolutions[key] ?? 'merge'}
					{@const nested = nestedConflictsForFolder(current.preflight, dc.source_id)}
					{@const nestedTotal = nested.files.length + nested.kinds.length}
					{@const isExpanded = expandedFolders.has(dc.source_id)}
					<li
						class="transfer-conflict__folder-group"
						data-testid={`transfer-conflict-folder-${dc.source_id}`}
					>
						<div class="transfer-conflict__row">
							<div class="transfer-conflict__row-main">
								<div class="transfer-conflict__path-line">
									<span class="transfer-conflict__type-icon" aria-hidden="true">
										<Icon name="folder" />
									</span>
									<span class="transfer-conflict__path" title={dc.relative_path}>
										{dc.source_name}
									</span>
									{#if dc.source_has_shares && current.operation === 'move'}
										<span class="transfer-conflict__badge transfer-conflict__badge--warn">
											<Icon name="exclamation-triangle" />
											{t('files.transfer_conflict.badge_shared', 'Has shares')}
										</span>
									{/if}
								</div>

								<div class="transfer-conflict__meta">
									<span>
										{t(
											'files.transfer_conflict.folder_meta',
											{
												sourceCount: dc.source_file_count,
												targetCount: dc.target_file_count
											},
											'Source: {{sourceCount}} file(s) · Destination: {{targetCount}} file(s)'
										)}
									</span>
									{#if dc.nested_conflict_count > 0 && choice === 'merge'}
										<span>
											{t(
												'files.transfer_conflict.folder_nested_summary',
												{ n: dc.nested_conflict_count },
												'{{n}} nested item conflict(s) when merging'
											)}
										</span>
									{/if}
								</div>

								{#if dc.source_has_shares && current.operation === 'move' && choice === 'merge'}
									<div class="transfer-conflict__warning-note" role="note">
										{t(
											'files.transfer_conflict.folder_shared_move_warning',
											'This source folder has active shares; merging moves contents out and removes the empty folder.'
										)}
									</div>
								{/if}

								{#if choice === 'keep_both'}
									<div class="transfer-conflict__hint">
										{t(
											'files.transfer_conflict.keep_both_hint',
											{
												name: formatTransferKeepBothPreview(dc.source_name, 'folder')
											},
											'Kept alongside as {{name}}'
										)}
									</div>
								{/if}

								{#if choice === 'merge' && nestedTotal > 0}
									<button
										type="button"
										class="transfer-conflict__nested-toggle"
										aria-expanded={isExpanded}
										onclick={() => toggleExpanded(dc.source_id)}
									>
										<Icon name={isExpanded ? 'chevron-down' : 'chevron-right'} />
										{#if isExpanded}
											{t(
												'files.transfer_conflict.folder_nested_toggle_hide',
												{ n: nestedTotal },
												'Hide nested conflicts ({{n}})'
											)}
										{:else}
											{t(
												'files.transfer_conflict.folder_nested_toggle_show',
												{ n: nestedTotal },
												'Show nested conflicts ({{n}})'
											)}
										{/if}
									</button>
								{/if}
							</div>

							<div class="transfer-conflict__actions" role="radiogroup" aria-label={dc.source_name}>
								<button
									type="button"
									role="radio"
									aria-checked={choice === 'merge'}
									class="transfer-conflict__choice"
									class:transfer-conflict__choice--active={choice === 'merge'}
									onclick={() => setAction('folder', dc.source_id, 'merge')}
								>
									{t('files.transfer_conflict.action_merge', 'Merge')}
								</button>
								<button
									type="button"
									role="radio"
									aria-checked={choice === 'keep_both'}
									class="transfer-conflict__choice"
									class:transfer-conflict__choice--active={choice === 'keep_both'}
									onclick={() => setAction('folder', dc.source_id, 'keep_both')}
								>
									{t('files.transfer_conflict.action_keep_both', 'Keep both')}
								</button>
								<button
									type="button"
									role="radio"
									aria-checked={choice === 'skip'}
									class="transfer-conflict__choice"
									class:transfer-conflict__choice--active={choice === 'skip'}
									onclick={() => setAction('folder', dc.source_id, 'skip')}
								>
									{t('files.transfer_conflict.action_skip', 'Skip')}
								</button>
							</div>
						</div>

						{#if choice === 'merge' && nestedTotal > 0 && isExpanded}
							<ul class="transfer-conflict__nested">
								{#each nested.files as nfc (nfc.source_id)}
									{@render fileConflictRow(nfc, true)}
								{/each}
								{#each nested.kinds as nkc (nkc.source_id)}
									{@render kindConflictRow(nkc, true)}
								{/each}
							</ul>
						{/if}
					</li>
				{/each}

				{#each topFiles as fc (fc.source_id)}
					{@render fileConflictRow(fc, false)}
				{/each}

				{#each topKinds as kc (kc.source_id)}
					{@render kindConflictRow(kc, false)}
				{/each}
			</ul>
		</div>

		{#snippet footer()}
			<Button variant="secondary" onclick={handleCancel}>
				{t('common.cancel', 'Cancel')}
			</Button>
			<Button variant="primary" onclick={handleConfirm}>
				{confirmLabel}
			</Button>
		{/snippet}
	</Modal>
{/if}

<style>
	.transfer-conflict {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.transfer-conflict__intro {
		margin: 0;
		font-size: var(--text-sm);
		color: var(--color-text-secondary);
	}

	.transfer-conflict__summary {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
	}

	.transfer-conflict__pill {
		display: inline-flex;
		align-items: center;
		padding: var(--space-1) var(--space-2-5);
		border-radius: var(--radius-full);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		border: 1px solid var(--color-border);
	}

	.transfer-conflict__pill--warn {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
		border-color: var(--color-warning-border);
	}

	.transfer-conflict__pill--info {
		background: var(--color-info-bg);
		color: var(--color-info-text);
		border-color: var(--color-info-border);
	}

	.transfer-conflict__pill--error {
		background: var(--color-error-bg);
		color: var(--color-error-text);
		border-color: var(--color-error-text);
	}

	.transfer-conflict__pill--muted {
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
	}

	.transfer-conflict__bulk {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		padding-bottom: var(--space-2);
		border-bottom: 1px solid var(--color-border);
	}

	.transfer-conflict__list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		max-height: 50vh;
		overflow: auto;
	}

	.transfer-conflict__folder-group {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		padding: var(--space-3);
		background: var(--color-bg-surface);
	}

	.transfer-conflict__folder-group > .transfer-conflict__row {
		border: none;
		padding: 0;
		background: transparent;
	}

	.transfer-conflict__row {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		padding: var(--space-3);
		background: var(--color-bg-surface);
	}

	.transfer-conflict__row--nested {
		background: var(--color-bg-subtle);
		padding: var(--space-2-5);
	}

	.transfer-conflict__nested {
		list-style: none;
		margin: 0;
		padding: 0 0 0 var(--space-4);
		border-left: 2px solid var(--color-border-medium);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.transfer-conflict__row-main {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
		flex: 1;
	}

	.transfer-conflict__path-line {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}

	.transfer-conflict__type-icon {
		color: var(--color-text-muted);
		flex: none;
	}

	.transfer-conflict__path {
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		overflow-wrap: anywhere;
	}

	.transfer-conflict__badge {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: var(--space-0-5) var(--space-2);
		border-radius: var(--radius-full);
		font-size: var(--text-2xs);
		font-weight: var(--weight-medium);
	}

	.transfer-conflict__badge--info {
		background: var(--color-info-bg);
		color: var(--color-info-text);
	}

	.transfer-conflict__badge--warn {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.transfer-conflict__badge--success {
		background: var(--color-success-bg);
		color: var(--color-success-text);
	}

	.transfer-conflict__badge--error {
		background: var(--color-error-bg);
		color: var(--color-error-text);
	}

	.transfer-conflict__badge--muted {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.transfer-conflict__meta {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1) var(--space-3);
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}

	.transfer-conflict__hint {
		font-size: var(--text-xs);
		color: var(--color-text-secondary);
	}

	.transfer-conflict__warning-note {
		font-size: var(--text-xs);
		color: var(--color-warning-text);
		background: var(--color-warning-bg);
		padding: var(--space-1-5) var(--space-2);
		border-radius: var(--radius-md);
	}

	.transfer-conflict__nested-toggle {
		align-self: flex-start;
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		background: none;
		border: none;
		padding: var(--space-1) 0;
		font: inherit;
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		color: var(--color-accent-text);
		cursor: pointer;
	}

	.transfer-conflict__actions {
		display: inline-flex;
		flex-wrap: wrap;
		gap: var(--space-1);
		flex: none;
	}

	.transfer-conflict__choice {
		padding: var(--space-1) var(--space-2-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font: inherit;
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		cursor: pointer;
	}

	.transfer-conflict__choice:hover:not(:disabled) {
		background: var(--color-bg-hover);
		border-color: var(--color-border-medium);
	}

	.transfer-conflict__choice--active {
		background: var(--color-accent);
		color: var(--color-on-accent);
		border-color: var(--color-accent);
	}

	.transfer-conflict__choice:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}
</style>
