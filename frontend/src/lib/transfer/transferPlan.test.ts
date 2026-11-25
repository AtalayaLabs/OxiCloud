import { describe, expect, it } from 'vitest';
import type {
	TransferFileConflictDto,
	TransferFolderConflictDto,
	TransferKindConflictDto,
	TransferPreflightResponse
} from '$lib/api/endpoints/transfer';
import { emptyTransferPreflight } from '$lib/api/endpoints/transfer';
import {
	applyTransferBulkAction,
	buildTransferDecisions,
	countActiveTopLevelTransfers,
	formatTransferKeepBothPreview,
	hasUserFacingConflicts,
	initialTransferResolutions,
	nestedConflictsForFolder,
	transferConflictKey
} from './transferPlan';

function fileConflict(
	overrides: Partial<TransferFileConflictDto> & { source_id: string; source_name: string }
): TransferFileConflictDto {
	return {
		relative_path: overrides.source_name,
		source_size: 120,
		source_modified_at: '2026-11-20T12:00:00Z',
		source_hash: 'src-hash',
		target_id: `tgt-${overrides.source_id}`,
		target_folder_id: 'dest-root',
		target_size: 100,
		target_modified_at: '2026-11-19T12:00:00Z',
		target_hash: 'tgt-hash',
		identical: false,
		can_update_target: true,
		can_delete_source: true,
		parent_folder_conflict_id: null,
		...overrides
	};
}

function folderConflict(
	overrides: Partial<TransferFolderConflictDto> & { source_id: string; source_name: string }
): TransferFolderConflictDto {
	return {
		relative_path: overrides.source_name,
		target_id: `tgt-${overrides.source_id}`,
		source_file_count: 3,
		target_file_count: 2,
		nested_conflict_count: 1,
		nested_identical_count: 0,
		source_has_shares: false,
		can_delete_source: true,
		...overrides
	};
}

function kindConflict(
	overrides: Partial<TransferKindConflictDto> & {
		source_id: string;
		source_name: string;
		source_kind: 'file' | 'folder';
	}
): TransferKindConflictDto {
	return {
		relative_path: overrides.source_name,
		existing_id: `existing-${overrides.source_id}`,
		existing_kind: overrides.source_kind === 'file' ? 'folder' : 'file',
		parent_folder_conflict_id: null,
		...overrides
	};
}

describe('transferPlan', () => {
	it('treats same-parent copy items as non-conflicting and emits keep_both decisions for copy', () => {
		const preflight: TransferPreflightResponse = {
			...emptyTransferPreflight(),
			same_parent_file_ids: ['f-same'],
			same_parent_folder_ids: ['d-same']
		};

		expect(hasUserFacingConflicts(preflight)).toBe(false);
		expect(buildTransferDecisions(preflight, {}, 'copy')).toEqual([
			{ source_id: 'f-same', kind: 'file', action: 'keep_both' },
			{ source_id: 'd-same', kind: 'folder', action: 'keep_both' }
		]);
		expect(buildTransferDecisions(preflight, {}, 'move')).toEqual([]);
	});

	it('initializes folders to merge and files/kind-mismatches to skip cleanly', () => {
		const preflight: TransferPreflightResponse = {
			...emptyTransferPreflight(),
			folder_conflicts: [folderConflict({ source_id: 'd1', source_name: 'Docs' })],
			file_conflicts: [
				fileConflict({ source_id: 'f-top', source_name: 'readme.md' }),
				fileConflict({
					source_id: 'f-nested',
					source_name: 'budget.xlsx',
					relative_path: 'Docs/budget.xlsx',
					parent_folder_conflict_id: 'd1'
				})
			],
			kind_conflicts: [kindConflict({ source_id: 'k1', source_name: 'notes', source_kind: 'file' })]
		};

		const initial = initialTransferResolutions(preflight);
		expect(initial[transferConflictKey('folder', 'd1')]).toBe('merge');
		expect(initial[transferConflictKey('file', 'f-top')]).toBe('skip');
		expect(initial[transferConflictKey('file', 'f-nested')]).toBe('skip');
		expect(initial[transferConflictKey('file', 'k1')]).toBe('skip');
	});

	it('applies replace_if_newer and guards read-only files cleanly', () => {
		const preflight: TransferPreflightResponse = {
			...emptyTransferPreflight(),
			file_conflicts: [
				// Source strictly newer + writable -> replace
				fileConflict({
					source_id: 'f-newer',
					source_name: 'a.txt',
					source_modified_at: '2026-11-25T10:00:00Z',
					target_modified_at: '2026-11-24T10:00:00Z',
					can_update_target: true
				}),
				// Target strictly newer -> skip
				fileConflict({
					source_id: 'f-older',
					source_name: 'b.txt',
					source_modified_at: '2026-11-20T10:00:00Z',
					target_modified_at: '2026-11-24T10:00:00Z',
					can_update_target: true
				}),
				// Read-only target -> stays skip even on replace_all
				fileConflict({
					source_id: 'f-ro',
					source_name: 'c.txt',
					source_modified_at: '2026-11-25T10:00:00Z',
					target_modified_at: '2026-11-20T10:00:00Z',
					can_update_target: false
				})
			]
		};

		const newer = applyTransferBulkAction(
			preflight,
			initialTransferResolutions(preflight),
			'replace_if_newer'
		);
		expect(newer[transferConflictKey('file', 'f-newer')]).toBe('replace');
		expect(newer[transferConflictKey('file', 'f-older')]).toBe('skip');
		expect(newer[transferConflictKey('file', 'f-ro')]).toBe('skip');

		const allReplace = applyTransferBulkAction(
			preflight,
			initialTransferResolutions(preflight),
			'replace_all'
		);
		expect(allReplace[transferConflictKey('file', 'f-older')]).toBe('replace');
		expect(allReplace[transferConflictKey('file', 'f-ro')]).toBe('skip');
	});

	it('attaches target_hash on replace and prunes nested decisions unless parent folder merges', () => {
		const preflight: TransferPreflightResponse = {
			...emptyTransferPreflight(),
			folder_conflicts: [folderConflict({ source_id: 'd1', source_name: 'Docs' })],
			file_conflicts: [
				fileConflict({
					source_id: 'f-nested',
					source_name: 'spec.pdf',
					relative_path: 'Docs/spec.pdf',
					target_hash: 'cas-sha-99',
					parent_folder_conflict_id: 'd1'
				})
			],
			kind_conflicts: [kindConflict({ source_id: 'k1', source_name: 'bin', source_kind: 'folder' })]
		};

		expect(nestedConflictsForFolder(preflight, 'd1').files).toHaveLength(1);

		// Folder defaults to merge, nested file set to replace, kind mismatch pinned against replace
		const mergeDecisions = buildTransferDecisions(
			preflight,
			{
				[transferConflictKey('folder', 'd1')]: 'merge',
				[transferConflictKey('file', 'f-nested')]: 'replace',
				[transferConflictKey('folder', 'k1')]: 'replace' // invalid for kind mismatch -> clamped to skip
			},
			'move'
		);
		expect(mergeDecisions).toEqual([
			{ source_id: 'd1', kind: 'folder', action: 'merge' },
			{
				source_id: 'f-nested',
				kind: 'file',
				action: 'replace',
				expected_hash: 'cas-sha-99'
			},
			{ source_id: 'k1', kind: 'folder', action: 'skip' }
		]);

		// When parent folder switches to keep_both, nested f-nested decision must be omitted
		const renameDecisions = buildTransferDecisions(
			preflight,
			{
				[transferConflictKey('folder', 'd1')]: 'keep_both',
				[transferConflictKey('file', 'f-nested')]: 'replace'
			},
			'move'
		);
		expect(renameDecisions).toEqual([
			{ source_id: 'd1', kind: 'folder', action: 'keep_both' },
			{ source_id: 'k1', kind: 'folder', action: 'skip' }
		]);

		// Active top-level count reflects skipped kind mismatch
		expect(
			countActiveTopLevelTransfers(
				preflight,
				2,
				{
					[transferConflictKey('folder', 'd1')]: 'keep_both',
					[transferConflictKey('folder', 'k1')]: 'skip'
				},
				'move'
			)
		).toBe(1);
	});

	it('formats folder keep-both names without stripping dotted version tags', () => {
		expect(formatTransferKeepBothPreview('archive.tar.gz', 'file')).toBe('archive (2).tar.gz');
		expect(formatTransferKeepBothPreview('release.v2', 'folder')).toBe('release.v2 (2)');
	});
});
