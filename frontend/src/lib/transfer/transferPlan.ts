/**
 * Pure conflict-planning helpers for internal move/copy (`POST /api/folders/{id}/transfer`).
 *
 * Reuses `compareMtimeSeconds` and `formatKeepBothPreview` from `$lib/upload/conflictPlan`
 * so file mtime semantics and `stem (2).ext` previews stay identical across Upload and Move/Copy.
 */
import type {
	ItemKind,
	TransferDecisionAction,
	TransferFileConflictDto,
	TransferFolderConflictDto,
	TransferItemDecision,
	TransferKindConflictDto,
	TransferOperation,
	TransferPreflightResponse
} from '$lib/api/endpoints/transfer';
import {
	compareMtimeSeconds,
	formatKeepBothPreview,
	type MtimeComparison
} from '$lib/upload/conflictPlan';

export type TransferBulkAction =
	| 'skip_all'
	| 'keep_both_all'
	| 'replace_all'
	| 'replace_if_newer'
	| 'merge_all_folders';

export interface NestedFolderConflicts {
	files: TransferFileConflictDto[];
	kinds: TransferKindConflictDto[];
}

export function transferConflictKey(kind: ItemKind, sourceId: string): string {
	return `${kind}:${sourceId}`;
}

export function compareTransferMtime(fc: TransferFileConflictDto): MtimeComparison | null {
	return compareMtimeSeconds(Date.parse(fc.source_modified_at), fc.target_modified_at);
}

/**
 * Matches backend `NameKind` suffixing: folders never strip a dotted suffix
 * (`v1.2` → `v1.2 (2)`), whereas files keep their extension intact.
 */
export function formatTransferKeepBothPreview(name: string, kind: ItemKind): string {
	return kind === 'folder' ? `${name} (2)` : formatKeepBothPreview(name);
}

/**
 * `true` when the user must choose how to handle at least one collision.
 * Same-parent items (`same_parent_file_ids` / `same_parent_folder_ids`) are NOT
 * user-facing conflicts: Copy auto-duplicates them as `name (2)` directly, and Move
 * ignores them as no-ops.
 */
export function hasUserFacingConflicts(preflight: TransferPreflightResponse): boolean {
	return (
		preflight.file_conflicts.length > 0 ||
		preflight.folder_conflicts.length > 0 ||
		preflight.kind_conflicts.length > 0
	);
}

export function topLevelFileConflicts(
	preflight: TransferPreflightResponse
): TransferFileConflictDto[] {
	return preflight.file_conflicts.filter((fc) => !fc.parent_folder_conflict_id);
}

export function topLevelKindConflicts(
	preflight: TransferPreflightResponse
): TransferKindConflictDto[] {
	return preflight.kind_conflicts.filter((kc) => !kc.parent_folder_conflict_id);
}

export function nestedConflictsForFolder(
	preflight: TransferPreflightResponse,
	folderSourceId: string
): NestedFolderConflicts {
	return {
		files: preflight.file_conflicts.filter((fc) => fc.parent_folder_conflict_id === folderSourceId),
		kinds: preflight.kind_conflicts.filter((kc) => kc.parent_folder_conflict_id === folderSourceId)
	};
}

/**
 * Safe defaults matching Solution A:
 * - Folders default to `'merge'` (recursive merge without overwriting colliding files).
 * - Files & kind mismatches default to `'skip'` (non-destructive until user opts in).
 */
export function initialTransferResolutions(
	preflight: TransferPreflightResponse
): Record<string, TransferDecisionAction> {
	const out: Record<string, TransferDecisionAction> = {};
	for (const dc of preflight.folder_conflicts) {
		out[transferConflictKey('folder', dc.source_id)] = 'merge';
	}
	for (const fc of preflight.file_conflicts) {
		out[transferConflictKey('file', fc.source_id)] = 'skip';
	}
	for (const kc of preflight.kind_conflicts) {
		out[transferConflictKey('folder' === kc.source_kind ? 'folder' : 'file', kc.source_id)] =
			'skip';
	}
	return out;
}

export function applyTransferBulkAction(
	preflight: TransferPreflightResponse,
	current: Record<string, TransferDecisionAction>,
	bulk: TransferBulkAction
): Record<string, TransferDecisionAction> {
	const next: Record<string, TransferDecisionAction> = { ...current };

	switch (bulk) {
		case 'skip_all':
			for (const dc of preflight.folder_conflicts) {
				next[transferConflictKey('folder', dc.source_id)] = 'skip';
			}
			for (const fc of preflight.file_conflicts) {
				next[transferConflictKey('file', fc.source_id)] = 'skip';
			}
			for (const kc of preflight.kind_conflicts) {
				next[transferConflictKey(kc.source_kind, kc.source_id)] = 'skip';
			}
			return next;

		case 'keep_both_all':
			for (const dc of preflight.folder_conflicts) {
				next[transferConflictKey('folder', dc.source_id)] = 'keep_both';
			}
			for (const fc of preflight.file_conflicts) {
				next[transferConflictKey('file', fc.source_id)] = 'keep_both';
			}
			for (const kc of preflight.kind_conflicts) {
				next[transferConflictKey(kc.source_kind, kc.source_id)] = 'keep_both';
			}
			return next;

		case 'merge_all_folders':
			for (const dc of preflight.folder_conflicts) {
				next[transferConflictKey('folder', dc.source_id)] = 'merge';
			}
			return next;

		case 'replace_all':
			for (const fc of preflight.file_conflicts) {
				if (fc.can_update_target) {
					next[transferConflictKey('file', fc.source_id)] = 'replace';
				}
			}
			return next;

		case 'replace_if_newer':
			for (const fc of preflight.file_conflicts) {
				if (!fc.can_update_target || fc.identical) continue;
				const cmp = compareTransferMtime(fc);
				if (cmp === 'local_newer') {
					next[transferConflictKey('file', fc.source_id)] = 'replace';
				} else {
					next[transferConflictKey('file', fc.source_id)] = 'skip';
				}
			}
			return next;
	}
}

/**
 * Translate UI conflict choices into the `TransferItemDecision[]` array consumed by
 * `POST /api/folders/{id}/transfer`, including CAS `expected_hash` on replacements
 * and pruning nested child decisions whenever a parent folder is NOT set to `'merge'`.
 */
export function buildTransferDecisions(
	preflight: TransferPreflightResponse,
	resolutions: Record<string, TransferDecisionAction>,
	operation: TransferOperation
): TransferItemDecision[] {
	const decisions: TransferItemDecision[] = [];
	const mergedFolderIds = new Set<string>();

	for (const dc of preflight.folder_conflicts) {
		const raw = resolutions[transferConflictKey('folder', dc.source_id)] ?? 'merge';
		const action: TransferDecisionAction = raw === 'replace' ? 'merge' : raw;
		if (action === 'merge') mergedFolderIds.add(dc.source_id);
		decisions.push({
			source_id: dc.source_id,
			kind: 'folder',
			action
		});
	}

	for (const fc of preflight.file_conflicts) {
		if (fc.parent_folder_conflict_id && !mergedFolderIds.has(fc.parent_folder_conflict_id)) {
			continue;
		}
		let action = resolutions[transferConflictKey('file', fc.source_id)] ?? 'skip';
		if (action === 'merge') action = 'skip';
		if (action === 'replace' && !fc.can_update_target) action = 'skip';
		decisions.push({
			source_id: fc.source_id,
			kind: 'file',
			action,
			...(action === 'replace' ? { expected_hash: fc.target_hash } : {})
		});
	}

	for (const kc of preflight.kind_conflicts) {
		if (kc.parent_folder_conflict_id && !mergedFolderIds.has(kc.parent_folder_conflict_id)) {
			continue;
		}
		const raw = resolutions[transferConflictKey(kc.source_kind, kc.source_id)] ?? 'skip';
		const action: TransferDecisionAction = raw === 'keep_both' ? 'keep_both' : 'skip';
		decisions.push({
			source_id: kc.source_id,
			kind: kc.source_kind,
			action
		});
	}

	if (operation === 'copy') {
		for (const id of preflight.same_parent_file_ids) {
			decisions.push({ source_id: id, kind: 'file', action: 'keep_both' });
		}
		for (const id of preflight.same_parent_folder_ids) {
			decisions.push({ source_id: id, kind: 'folder', action: 'keep_both' });
		}
	}

	return decisions;
}

export function canReplaceAnyFile(preflight: TransferPreflightResponse): boolean {
	return preflight.file_conflicts.some((fc) => fc.can_update_target);
}

export function hasAnyFolderConflict(preflight: TransferPreflightResponse): boolean {
	return preflight.folder_conflicts.length > 0;
}

export function countIdenticalNestedFiles(preflight: TransferPreflightResponse): number {
	const topIdentical = preflight.file_conflicts.filter((fc) => fc.identical).length;
	const folderIdentical = preflight.folder_conflicts.reduce(
		(acc, dc) => acc + dc.nested_identical_count,
		0
	);
	return Math.max(topIdentical, folderIdentical);
}

/** Number of top-level items that will actually perform work (not skipped). */
export function countActiveTopLevelTransfers(
	preflight: TransferPreflightResponse,
	totalTopLevelItems: number,
	resolutions: Record<string, TransferDecisionAction>,
	operation: TransferOperation
): number {
	const folderMap = new Map<string, TransferFolderConflictDto>(
		preflight.folder_conflicts.map((dc) => [dc.source_id, dc])
	);
	let skippedTop = 0;

	for (const [id] of folderMap) {
		if ((resolutions[transferConflictKey('folder', id)] ?? 'merge') === 'skip') {
			skippedTop++;
		}
	}
	for (const fc of topLevelFileConflicts(preflight)) {
		if ((resolutions[transferConflictKey('file', fc.source_id)] ?? 'skip') === 'skip') {
			skippedTop++;
		}
	}
	for (const kc of topLevelKindConflicts(preflight)) {
		if ((resolutions[transferConflictKey(kc.source_kind, kc.source_id)] ?? 'skip') === 'skip') {
			skippedTop++;
		}
	}
	if (operation === 'move') {
		skippedTop += preflight.same_parent_file_ids.length + preflight.same_parent_folder_ids.length;
	}
	return Math.max(0, totalTopLevelItems - skippedTop);
}
