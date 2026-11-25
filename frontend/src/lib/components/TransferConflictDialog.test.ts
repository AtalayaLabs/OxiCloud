import { render, fireEvent, waitFor, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it } from 'vitest';
import { emptyTransferPreflight } from '$lib/api/endpoints/transfer';
import {
	promptTransferConflicts,
	transferConflictStore
} from '$lib/stores/transferConflict.svelte';
import TransferConflictDialog from './TransferConflictDialog.svelte';

afterEach(() => {
	transferConflictStore.cancel();
});

describe('TransferConflictDialog', () => {
	it('defaults folders to Merge showing nested file rows and hides nested rows when switched to Keep both', async () => {
		const promise = promptTransferConflicts({
			operation: 'move',
			targetName: 'Target',
			totalItems: 1,
			preflight: {
				...emptyTransferPreflight(),
				folder_conflicts: [
					{
						source_id: 'dir-1',
						source_name: 'Docs',
						relative_path: 'Docs',
						target_id: 'tgt-dir-1',
						source_file_count: 4,
						target_file_count: 2,
						nested_conflict_count: 1,
						nested_identical_count: 0,
						source_has_shares: true,
						can_delete_source: true
					}
				],
				file_conflicts: [
					{
						source_id: 'nested-1',
						source_name: 'budget.xlsx',
						relative_path: 'Docs/budget.xlsx',
						source_size: 2048,
						source_modified_at: '2026-11-24T10:00:00Z',
						source_hash: 'src-hash',
						target_id: 'tgt-nested-1',
						target_folder_id: 'tgt-dir-1',
						target_size: 1024,
						target_modified_at: '2026-11-20T10:00:00Z',
						target_hash: 'tgt-hash-123',
						identical: false,
						can_update_target: true,
						can_delete_source: true,
						parent_folder_conflict_id: 'dir-1'
					}
				]
			}
		});

		const { getByTestId, queryByTestId, getByRole } = render(TransferConflictDialog);

		const folderRow = getByTestId('transfer-conflict-folder-dir-1');
		expect(folderRow.textContent).toContain('Has shares');
		expect(getByTestId('transfer-conflict-file-nested-1')).toBeTruthy();

		// Switch nested file to Replace (attaches target_hash when Merge stays active).
		const nestedRow = getByTestId('transfer-conflict-file-nested-1');
		await fireEvent.click(within(nestedRow).getByRole('radio', { name: 'Replace' }));

		const folderGroup = getByRole('radiogroup', { name: 'Docs' });

		// Switching folder to Keep both hides nested child rows and drops their decisions.
		await fireEvent.click(within(folderGroup).getByRole('radio', { name: 'Keep both' }));
		expect(queryByTestId('transfer-conflict-file-nested-1')).toBeNull();

		// Switching back to Merge reveals nested rows again.
		await fireEvent.click(within(folderGroup).getByRole('radio', { name: 'Merge' }));
		expect(getByTestId('transfer-conflict-file-nested-1')).toBeTruthy();

		await fireEvent.click(getByRole('button', { name: /Continue/i }));
		const decisions = await promise;
		expect(decisions).toEqual([
			{ source_id: 'dir-1', kind: 'folder', action: 'merge' },
			{
				source_id: 'nested-1',
				kind: 'file',
				action: 'replace',
				expected_hash: 'tgt-hash-123'
			}
		]);
	});

	it('disables Replace on read-only targets, blocks Replace on kind mismatch, and resolves null on cancel', async () => {
		const promise = promptTransferConflicts({
			operation: 'copy',
			targetName: 'Shared',
			totalItems: 2,
			preflight: {
				...emptyTransferPreflight(),
				file_conflicts: [
					{
						source_id: 'f-ro',
						source_name: 'policy.pdf',
						relative_path: 'policy.pdf',
						source_size: 512,
						source_modified_at: '2026-11-24T10:00:00Z',
						source_hash: 'h1',
						target_id: 't-ro',
						target_folder_id: 'dest',
						target_size: 512,
						target_modified_at: '2026-11-20T10:00:00Z',
						target_hash: 'h2',
						identical: false,
						can_update_target: false,
						can_delete_source: true,
						parent_folder_conflict_id: null
					}
				],
				kind_conflicts: [
					{
						source_id: 'k-1',
						source_name: 'notes',
						source_kind: 'file',
						relative_path: 'notes',
						existing_id: 'dir-existing',
						existing_kind: 'folder',
						parent_folder_conflict_id: null
					}
				]
			}
		});

		const { getByTestId, getByRole } = render(TransferConflictDialog);

		const roRow = getByTestId('transfer-conflict-file-f-ro');
		const replaceRadio = within(roRow).getByRole('radio', { name: 'Replace' }) as HTMLButtonElement;
		expect(replaceRadio.disabled).toBe(true);

		const kindRow = getByTestId('transfer-conflict-kind-k-1');
		expect(within(kindRow).queryByRole('radio', { name: 'Replace' })).toBeNull();
		expect(within(kindRow).getByRole('radio', { name: 'Keep both' })).toBeTruthy();

		await fireEvent.click(getByRole('button', { name: 'Cancel' }));
		await waitFor(async () => {
			expect(await promise).toBeNull();
		});
	});
});
