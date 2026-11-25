import { beforeEach, describe, expect, it, vi } from 'vitest';
import { emptyTransferPreflight } from '$lib/api/endpoints/transfer';

vi.mock('$lib/api/endpoints/transfer', async (importOriginal) => {
	const actual = await importOriginal<typeof import('$lib/api/endpoints/transfer')>();
	return {
		...actual,
		transferPreflight: vi.fn(),
		executeTransfer: vi.fn()
	};
});
vi.mock('$lib/api/endpoints/files', () => ({ moveFile: vi.fn() }));
vi.mock('$lib/api/endpoints/folders', () => ({ moveFolder: vi.fn() }));
vi.mock('$lib/api/endpoints/batch', () => ({ copyFiles: vi.fn(), copyFolders: vi.fn() }));
vi.mock('$lib/stores/transferConflict.svelte', () => ({
	promptTransferConflicts: vi.fn()
}));
vi.mock('$lib/stores/ui.svelte', () => ({
	ui: { notify: vi.fn() }
}));

import { copyFiles, copyFolders } from '$lib/api/endpoints/batch';
import { moveFile } from '$lib/api/endpoints/files';
import { moveFolder } from '$lib/api/endpoints/folders';
import { executeTransfer, transferPreflight } from '$lib/api/endpoints/transfer';
import { promptTransferConflicts } from '$lib/stores/transferConflict.svelte';
import { ui } from '$lib/stores/ui.svelte';
import { runTransfer } from './runTransfer';

const m = <T extends (...args: never[]) => unknown>(fn: T) =>
	fn as unknown as ReturnType<typeof vi.fn>;

describe('runTransfer', () => {
	beforeEach(() => {
		vi.clearAllMocks();
	});

	it('executes same-parent Copy directly with keep_both and does NOT open conflict modal', async () => {
		m(transferPreflight).mockResolvedValue({
			...emptyTransferPreflight(),
			same_parent_file_ids: ['f1'],
			same_parent_folder_ids: ['d1']
		});
		m(executeTransfer).mockResolvedValue({
			outcomes: [],
			moved_count: 0,
			copied_count: 0,
			replaced_count: 0,
			renamed_count: 2,
			merged_count: 0,
			skipped_count: 0,
			failed_count: 0
		});

		const res = await runTransfer({
			operation: 'copy',
			targetFolderId: 'cur',
			items: [
				{ id: 'f1', kind: 'file' },
				{ id: 'd1', kind: 'folder' }
			]
		});

		expect(res.status).toBe('completed');
		expect(promptTransferConflicts).not.toHaveBeenCalled();
		expect(executeTransfer).toHaveBeenCalledWith('cur', {
			operation: 'copy',
			fileIds: ['f1'],
			folderIds: ['d1'],
			decisions: [
				{ source_id: 'f1', kind: 'file', action: 'keep_both' },
				{ source_id: 'd1', kind: 'folder', action: 'keep_both' }
			]
		});
		expect(ui.notify).toHaveBeenCalledWith(expect.stringContaining('2 renamed'), 'success');
	});

	it('falls back to legacy move/copy endpoints when destination is an external mount (supported=false)', async () => {
		m(transferPreflight).mockResolvedValue({
			...emptyTransferPreflight(),
			supported: false
		});

		const res = await runTransfer({
			operation: 'move',
			targetFolderId: 'ext:mount/dir',
			items: [
				{ id: 'f1', kind: 'file' },
				{ id: 'd1', kind: 'folder' }
			]
		});

		expect(res).toEqual({ status: 'completed', fallback: true });
		expect(moveFile).toHaveBeenCalledWith('f1', 'ext:mount/dir');
		expect(moveFolder).toHaveBeenCalledWith('d1', 'ext:mount/dir');
		expect(executeTransfer).not.toHaveBeenCalled();
		expect(copyFiles).not.toHaveBeenCalled();
		expect(copyFolders).not.toHaveBeenCalled();
	});

	it('rejects descendant cycles cleanly before opening the conflict dialog', async () => {
		m(transferPreflight).mockResolvedValue({
			...emptyTransferPreflight(),
			cycle_folder_ids: ['d1']
		});

		await expect(
			runTransfer({
				operation: 'move',
				targetFolderId: 'd1-child',
				items: [{ id: 'd1', kind: 'folder' }]
			})
		).rejects.toThrow(/itself|subfolder/i);
		expect(promptTransferConflicts).not.toHaveBeenCalled();
		expect(executeTransfer).not.toHaveBeenCalled();
	});

	it('invokes onBeforePrompt then opens conflict modal, skips execution on cancel, and executes user choices on confirm', async () => {
		const conflictPreflight = {
			...emptyTransferPreflight(),
			file_conflicts: [
				{
					source_id: 'f1',
					source_name: 'doc.txt',
					relative_path: 'doc.txt',
					source_size: 10,
					source_modified_at: '2026-11-25T10:00:00Z',
					source_hash: 'h1',
					target_id: 't1',
					target_folder_id: 'dest',
					target_size: 8,
					target_modified_at: '2026-11-20T10:00:00Z',
					target_hash: 'h0',
					identical: false,
					can_update_target: true,
					can_delete_source: true,
					parent_folder_conflict_id: null
				}
			]
		};
		m(transferPreflight).mockResolvedValue(conflictPreflight);
		m(promptTransferConflicts).mockResolvedValueOnce(null);

		const onBeforePrompt = vi.fn();
		const cancelled = await runTransfer({
			operation: 'move',
			targetFolderId: 'dest',
			targetName: 'Dest',
			items: [{ id: 'f1', kind: 'file' }],
			onBeforePrompt
		});
		expect(onBeforePrompt).toHaveBeenCalledTimes(1);
		expect(cancelled).toEqual({ status: 'cancelled' });
		expect(executeTransfer).not.toHaveBeenCalled();

		m(promptTransferConflicts).mockResolvedValueOnce([
			{ source_id: 'f1', kind: 'file', action: 'replace', expected_hash: 'h0' }
		]);
		m(executeTransfer).mockResolvedValueOnce({
			outcomes: [
				{
					source_id: 'f1',
					kind: 'file',
					status: 'replaced',
					target_id: 't1',
					final_name: 'doc.txt',
					error: null,
					error_type: null
				}
			],
			moved_count: 0,
			copied_count: 0,
			replaced_count: 1,
			renamed_count: 0,
			merged_count: 0,
			skipped_count: 0,
			failed_count: 0
		});

		const done = await runTransfer({
			operation: 'move',
			targetFolderId: 'dest',
			items: [{ id: 'f1', kind: 'file' }]
		});
		expect(done.status).toBe('completed');
		expect(executeTransfer).toHaveBeenCalledWith('dest', {
			operation: 'move',
			fileIds: ['f1'],
			folderIds: [],
			decisions: [{ source_id: 'f1', kind: 'file', action: 'replace', expected_hash: 'h0' }]
		});
	});

	it('raises an error toast naming the localized error on partial failure', async () => {
		m(transferPreflight).mockResolvedValue(emptyTransferPreflight());
		m(executeTransfer).mockResolvedValue({
			outcomes: [
				{
					source_id: 'f1',
					kind: 'file',
					status: 'failed',
					target_id: null,
					final_name: null,
					error: 'Permission denied',
					error_type: 'AccessDenied'
				}
			],
			moved_count: 0,
			copied_count: 0,
			replaced_count: 0,
			renamed_count: 0,
			merged_count: 0,
			skipped_count: 0,
			failed_count: 1
		});

		await runTransfer({
			operation: 'move',
			targetFolderId: 'dest',
			items: [{ id: 'f1', kind: 'file' }]
		});

		expect(ui.notify).toHaveBeenCalledWith(expect.stringContaining('permission'), 'error');
	});
});
