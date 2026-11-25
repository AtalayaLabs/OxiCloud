/**
 * Unified Move / Copy orchestrator across `MoveDialog`, folder/breadcrumb drop, and
 * sidebar drive drop.
 *
 * Calls `transferPreflight` before mutating anything:
 * - External mounts (`supported: false`) fall back to sequential REST move/copy endpoints.
 * - Descendant cycles reject cleanly before opening any conflict modal.
 * - Same-parent copies (`same_parent_*`) auto-duplicate silently as `name (2)` without prompting.
 * - Actual collisions (files, folders, kind mismatches) open the single shared
 *   `<TransferConflictDialog>` via `promptTransferConflicts`.
 */
import { ApiError } from '$lib/api/client';
import { copyFiles, copyFolders } from '$lib/api/endpoints/batch';
import { moveFile } from '$lib/api/endpoints/files';
import { moveFolder } from '$lib/api/endpoints/folders';
import {
	executeTransfer,
	transferPreflight,
	type TransferExecuteResponse,
	type TransferItemDecision,
	type TransferOperation
} from '$lib/api/endpoints/transfer';
import type { ItemType } from '$lib/api/types';
import { t } from '$lib/i18n/index.svelte';
import { promptTransferConflicts } from '$lib/stores/transferConflict.svelte';
import { ui } from '$lib/stores/ui.svelte';
import { buildTransferDecisions, hasUserFacingConflicts } from '$lib/transfer/transferPlan';
import { errorMessage } from '$lib/utils/errors';

export interface RunTransferItem {
	id: string;
	kind: ItemType;
}

export interface RunTransferOptions {
	operation: TransferOperation;
	targetFolderId: string;
	targetName?: string;
	items: RunTransferItem[];
	/**
	 * Optional hook invoked right before opening `<TransferConflictDialog>`.
	 * Used by `MoveDialog` to close the destination picker before the conflict
	 * resolver mounts so two modals never stack.
	 */
	onBeforePrompt?: () => void;
}

export type RunTransferResult =
	| { status: 'noop' }
	| { status: 'cancelled' }
	| { status: 'completed'; fallback: true }
	| { status: 'completed'; fallback: false; response: TransferExecuteResponse };

function summarizeResponse(operation: TransferOperation, resp: TransferExecuteResponse) {
	const succeeded =
		resp.moved_count +
		resp.copied_count +
		resp.replaced_count +
		resp.renamed_count +
		resp.merged_count;

	if (resp.failed_count > 0) {
		const firstFailed = resp.outcomes.find((o) => o.status === 'failed');
		const msg = firstFailed
			? errorMessage(
					new ApiError(
						400,
						'',
						'/api/folders/transfer',
						firstFailed.error_type ?? undefined,
						firstFailed.error ?? undefined
					)
				)
			: t('errors.generic', 'Operation failed');
		ui.notify(
			t(
				'files.transfer_conflict.toast_partial',
				{ ok: succeeded, failed: resp.failed_count, error: msg },
				'{{ok}} completed, {{failed}} failed: {{error}}'
			),
			'error'
		);
		return;
	}

	if (succeeded === 0 && resp.skipped_count > 0) {
		ui.notify(
			t(
				'files.transfer_conflict.toast_all_skipped',
				{ n: resp.skipped_count },
				'No changes made ({{n}} skipped)'
			),
			'info'
		);
		return;
	}

	const onlyPureMove =
		operation === 'move' &&
		resp.moved_count > 0 &&
		resp.copied_count === 0 &&
		resp.replaced_count === 0 &&
		resp.renamed_count === 0 &&
		resp.merged_count === 0 &&
		resp.skipped_count === 0;
	if (onlyPureMove) {
		ui.notify(t('files.moved', 'Moved'), 'success');
		return;
	}

	const onlyPureCopy =
		operation === 'copy' &&
		resp.copied_count > 0 &&
		resp.moved_count === 0 &&
		resp.replaced_count === 0 &&
		resp.renamed_count === 0 &&
		resp.merged_count === 0 &&
		resp.skipped_count === 0;
	if (onlyPureCopy) {
		ui.notify(t('files.copied', 'Copied'), 'success');
		return;
	}

	const parts: string[] = [];
	if (resp.moved_count > 0) {
		parts.push(
			t('files.transfer_conflict.summary_part_moved', { n: resp.moved_count }, '{{n}} moved')
		);
	}
	if (resp.copied_count > 0) {
		parts.push(
			t('files.transfer_conflict.summary_part_copied', { n: resp.copied_count }, '{{n}} copied')
		);
	}
	if (resp.replaced_count > 0) {
		parts.push(
			t(
				'files.transfer_conflict.summary_part_replaced',
				{ n: resp.replaced_count },
				'{{n}} replaced'
			)
		);
	}
	if (resp.renamed_count > 0) {
		parts.push(
			t('files.transfer_conflict.summary_part_renamed', { n: resp.renamed_count }, '{{n}} renamed')
		);
	}
	if (resp.merged_count > 0) {
		parts.push(
			t('files.transfer_conflict.summary_part_merged', { n: resp.merged_count }, '{{n}} merged')
		);
	}
	if (resp.skipped_count > 0) {
		parts.push(
			t('files.transfer_conflict.summary_part_skipped', { n: resp.skipped_count }, '{{n}} skipped')
		);
	}

	if (parts.length > 0) {
		ui.notify(
			t(
				'files.transfer_conflict.toast_summary',
				{ summary: parts.join(', ') },
				'Transfer complete: {{summary}}'
			),
			'success'
		);
	}
}

export async function runTransfer(opts: RunTransferOptions): Promise<RunTransferResult> {
	const fileIds = opts.items.filter((it) => it.kind === 'file').map((it) => it.id);
	const folderIds = opts.items.filter((it) => it.kind === 'folder').map((it) => it.id);
	if (fileIds.length === 0 && folderIds.length === 0) {
		return { status: 'noop' };
	}

	const preflight = await transferPreflight(opts.targetFolderId, {
		operation: opts.operation,
		fileIds,
		folderIds,
		expandFolderConflicts: true
	});

	if (!preflight.supported) {
		if (opts.operation === 'copy') {
			await copyFiles(fileIds, opts.targetFolderId);
			await copyFolders(folderIds, opts.targetFolderId);
		} else {
			for (const fid of fileIds) {
				await moveFile(fid, opts.targetFolderId);
			}
			for (const did of folderIds) {
				if (did === opts.targetFolderId) continue;
				await moveFolder(did, opts.targetFolderId);
			}
		}
		ui.notify(
			opts.operation === 'copy' ? t('files.copied', 'Copied') : t('files.moved', 'Moved'),
			'success'
		);
		return { status: 'completed', fallback: true };
	}

	if (preflight.cycle_folder_ids.length > 0) {
		throw new Error(
			t(
				'files.transfer_conflict.error_cycle',
				'Cannot move or copy a folder into itself or one of its subfolders.'
			)
		);
	}

	const sameFiles = new Set(preflight.same_parent_file_ids);
	const sameFolders = new Set(preflight.same_parent_folder_ids);
	const effectiveFileIds =
		opts.operation === 'move' ? fileIds.filter((id) => !sameFiles.has(id)) : fileIds;
	const effectiveFolderIds =
		opts.operation === 'move' ? folderIds.filter((id) => !sameFolders.has(id)) : folderIds;

	if (effectiveFileIds.length === 0 && effectiveFolderIds.length === 0) {
		return { status: 'noop' };
	}

	let decisions: TransferItemDecision[];
	if (hasUserFacingConflicts(preflight)) {
		opts.onBeforePrompt?.();
		const chosen = await promptTransferConflicts({
			operation: opts.operation,
			targetName: opts.targetName,
			totalItems: effectiveFileIds.length + effectiveFolderIds.length,
			preflight
		});
		if (chosen === null) {
			return { status: 'cancelled' };
		}
		decisions = chosen;
	} else {
		decisions = buildTransferDecisions(preflight, {}, opts.operation);
	}

	const response = await executeTransfer(opts.targetFolderId, {
		operation: opts.operation,
		fileIds: effectiveFileIds,
		folderIds: effectiveFolderIds,
		decisions
	});

	summarizeResponse(opts.operation, response);
	return { status: 'completed', fallback: false, response };
}
