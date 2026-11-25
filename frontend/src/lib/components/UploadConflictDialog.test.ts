import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import UploadConflictDialog from './UploadConflictDialog.svelte';
import { formatKeepBothPreview, type UploadConflictItem } from '$lib/upload/conflictPlan';

function makeConflict(overrides: Partial<UploadConflictItem> = {}): UploadConflictItem {
	const name = overrides.relativePath ?? 'report.pdf';
	const file = new File([new Uint8Array(64)], name.split('/').pop() ?? name, {
		type: 'application/pdf',
		lastModified: 1_704_067_200_000
	});
	return {
		id: name,
		relativePath: name,
		kind: 'content',
		file,
		localSize: 64,
		localModifiedMs: file.lastModified,
		remote: {
			id: `id-${name}`,
			folderId: 'folder-1',
			name: file.name,
			size: 32,
			modifiedAt: '2024-01-01T00:00:00Z',
			contentHash: 'a'.repeat(64),
			canUpdate: true
		},
		canReplace: true,
		comparison: 'local_newer',
		keepBothPreviewName: formatKeepBothPreview(file.name),
		...overrides
	};
}

describe('UploadConflictDialog', () => {
	beforeEach(() => {
		vi.clearAllMocks();
	});

	it('defaults every conflict safely to skip and returns resolved map on confirm', async () => {
		const onresolve = vi.fn();
		const oncancel = vi.fn();
		render(UploadConflictDialog, {
			props: {
				open: true,
				conflicts: [
					makeConflict({ id: 'newer.pdf', relativePath: 'newer.pdf', comparison: 'local_newer' }),
					makeConflict({
						id: 'older.pdf',
						relativePath: 'older.pdf',
						comparison: 'remote_newer'
					}),
					makeConflict({
						id: 'locked.pdf',
						relativePath: 'locked.pdf',
						canReplace: false,
						comparison: 'local_newer'
					})
				],
				newCount: 2,
				identicalCount: 3,
				onresolve,
				oncancel
			}
		});

		const rows = await screen.findAllByTestId('upload-conflict-row');
		expect(rows).toHaveLength(3);

		const summary = screen.getByTestId('upload-conflict-summary').textContent ?? '';
		expect(summary).toContain('2');
		expect(summary).toContain('3');

		const skipRadios = screen.getAllByTestId('upload-conflict-choice-skip');
		for (const r of skipRadios) {
			expect(r.getAttribute('aria-checked')).toBe('true');
		}

		await fireEvent.click(screen.getByTestId('upload-conflict-bulk-newer'));

		const keepBothRadios = screen.getAllByTestId('upload-conflict-choice-keep-both');
		await fireEvent.click(keepBothRadios[1]);

		await fireEvent.click(screen.getByTestId('upload-conflict-confirm'));

		expect(onresolve).toHaveBeenCalledWith({
			'newer.pdf': 'replace',
			'older.pdf': 'keep_both',
			'locked.pdf': 'skip'
		});
		expect(oncancel).not.toHaveBeenCalled();
	});

	it('disables replace for kind mismatches and read-only items and supports bulk replace all', async () => {
		const onresolve = vi.fn();
		render(UploadConflictDialog, {
			props: {
				open: true,
				conflicts: [
					makeConflict({ id: 'writable.txt', relativePath: 'writable.txt', canReplace: true }),
					makeConflict({
						id: 'Docs/x.txt',
						relativePath: 'Docs/x.txt',
						kind: 'kind_mismatch',
						blockingKind: 'file',
						blockingPath: 'Docs',
						canReplace: false,
						remote: undefined,
						comparison: null
					})
				],
				newCount: 0,
				identicalCount: 0,
				onresolve,
				oncancel: vi.fn()
			}
		});

		const replaceRadios = await screen.findAllByTestId('upload-conflict-choice-replace');
		expect((replaceRadios[0] as HTMLButtonElement).disabled).toBe(false);
		expect((replaceRadios[1] as HTMLButtonElement).disabled).toBe(true);

		await fireEvent.click(screen.getByTestId('upload-conflict-bulk-replace'));
		await fireEvent.click(screen.getByTestId('upload-conflict-confirm'));

		expect(onresolve).toHaveBeenCalledWith({
			'writable.txt': 'replace',
			'Docs/x.txt': 'skip'
		});
	});

	it('calls oncancel when cancelled', async () => {
		const oncancel = vi.fn();
		render(UploadConflictDialog, {
			props: {
				open: true,
				conflicts: [makeConflict()],
				newCount: 1,
				identicalCount: 0,
				onresolve: vi.fn(),
				oncancel
			}
		});

		await fireEvent.click(await screen.findByTestId('upload-conflict-cancel'));
		expect(oncancel).toHaveBeenCalledTimes(1);
	});
});
