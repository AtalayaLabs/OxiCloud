import { describe, expect, it } from 'vitest';
import {
	applyBulkAction,
	buildExecutionPlan,
	compareMtimeSeconds,
	defaultResolutionFor,
	formatKeepBothPreview,
	type UploadConflictItem,
	type UploadPreflightAnalysis
} from './conflictPlan';

function makeFile(name: string, size = 12, lastModified = 1_704_067_200_000): File {
	const f = new File([new Uint8Array(size)], name, { type: 'text/plain', lastModified });
	return f;
}

function makeConflict(overrides: Partial<UploadConflictItem> = {}): UploadConflictItem {
	const file = overrides.file ?? makeFile('doc.pdf');
	const relativePath = overrides.relativePath ?? file.name;
	return {
		id: relativePath,
		relativePath,
		kind: 'content',
		file,
		localSize: file.size,
		localModifiedMs: file.lastModified,
		remote: {
			id: 'file-1',
			folderId: 'folder-root',
			name: file.name,
			size: 100,
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

describe('formatKeepBothPreview', () => {
	it('mirrors backend extension and dotfile rules with (2) suffix', () => {
		expect(formatKeepBothPreview('report.pdf')).toBe('report (2).pdf');
		expect(formatKeepBothPreview('archive.tar.gz')).toBe('archive (2).tar.gz');
		expect(formatKeepBothPreview('.env')).toBe('.env (2)');
		expect(formatKeepBothPreview('.env.local')).toBe('.env (2).local');
		expect(formatKeepBothPreview('README')).toBe('README (2)');
	});
});

describe('compareMtimeSeconds', () => {
	it('rounds local milliseconds to whole seconds so sub-second drift is ignored', () => {
		// 2024-01-01T00:00:00Z = 1704067200s
		expect(compareMtimeSeconds(1_704_067_200_400, '2024-01-01T00:00:00Z')).toBe('same');
		expect(compareMtimeSeconds(1_704_067_200_600, '2024-01-01T00:00:00Z')).toBe('local_newer');
		expect(compareMtimeSeconds(1_704_067_199_000, '2024-01-01T00:00:00Z')).toBe('remote_newer');
		expect(compareMtimeSeconds(0, '2024-01-01T00:00:00Z')).toBeNull();
	});
});

describe('applyBulkAction & buildExecutionPlan', () => {
	it('defaults conflicts safely to skip', () => {
		expect(defaultResolutionFor(makeConflict())).toBe('skip');
	});

	it('respects canReplace=false for replace_all and replace_if_newer', () => {
		const newerWritable = makeConflict({
			id: 'a.txt',
			relativePath: 'a.txt',
			file: makeFile('a.txt'),
			canReplace: true,
			comparison: 'local_newer'
		});
		const olderWritable = makeConflict({
			id: 'b.txt',
			relativePath: 'b.txt',
			file: makeFile('b.txt'),
			canReplace: true,
			comparison: 'remote_newer'
		});
		const newerReadOnly = makeConflict({
			id: 'c.txt',
			relativePath: 'c.txt',
			file: makeFile('c.txt'),
			canReplace: false,
			comparison: 'local_newer'
		});
		const kindMismatch = makeConflict({
			id: 'Docs/x.txt',
			relativePath: 'Docs/x.txt',
			kind: 'kind_mismatch',
			blockingKind: 'file',
			blockingPath: 'Docs',
			canReplace: false,
			remote: undefined,
			comparison: null
		});

		const list = [newerWritable, olderWritable, newerReadOnly, kindMismatch];

		expect(applyBulkAction(list, 'replace_if_newer')).toEqual({
			'a.txt': 'replace',
			'b.txt': 'skip',
			'c.txt': 'skip',
			'Docs/x.txt': 'skip'
		});

		expect(applyBulkAction(list, 'replace_all')).toEqual({
			'a.txt': 'replace',
			'b.txt': 'replace',
			'c.txt': 'skip',
			'Docs/x.txt': 'skip'
		});

		expect(applyBulkAction(list, 'keep_both_all')).toEqual({
			'a.txt': 'keep_both',
			'b.txt': 'keep_both',
			'c.txt': 'keep_both',
			'Docs/x.txt': 'keep_both'
		});
	});

	it('schedules topological folders, reuses existing folders, and renames kind-conflicting ancestors only when needed', () => {
		const newFile = makeFile('fresh.txt');
		const keepFile = makeFile('x.txt');
		const replaceFile = makeFile('r.txt');
		const analysis: UploadPreflightAnalysis = {
			preflightSupported: true,
			existingFolders: new Map([['Existing', 'f-existing']]),
			kindConflicts: [{ relativePath: 'BlockedDir', existingKind: 'file', existingId: 'f-block' }],
			newEntries: [{ file: newFile, relativePath: 'Existing/Sub/fresh.txt' }],
			identicalEntries: [{ file: makeFile('same.txt'), relativePath: 'same.txt' }],
			conflicts: [
				makeConflict({
					id: 'r.txt',
					relativePath: 'r.txt',
					file: replaceFile,
					canReplace: true
				}),
				makeConflict({
					id: 'BlockedDir/sub/x.txt',
					relativePath: 'BlockedDir/sub/x.txt',
					file: keepFile,
					kind: 'kind_mismatch',
					blockingKind: 'file',
					blockingPath: 'BlockedDir',
					canReplace: false,
					remote: undefined
				})
			],
			precomputedHashes: new Map()
		};

		const plan = buildExecutionPlan(analysis, {
			'r.txt': 'replace',
			'BlockedDir/sub/x.txt': 'keep_both'
		});

		// 'Existing' is reused, never recreated.
		expect(plan.foldersToCreate).toEqual([
			{
				relativePath: 'BlockedDir',
				parentRelDir: '',
				name: 'BlockedDir',
				onConflict: 'rename'
			},
			{
				relativePath: 'BlockedDir/sub',
				parentRelDir: 'BlockedDir',
				name: 'sub',
				onConflict: 'fail'
			},
			{
				relativePath: 'Existing/Sub',
				parentRelDir: 'Existing',
				name: 'Sub',
				onConflict: 'fail'
			}
		]);

		expect(plan.transfers).toEqual([
			{
				mode: 'create',
				file: newFile,
				relativePath: 'Existing/Sub/fresh.txt',
				parentRelDir: 'Existing/Sub',
				onConflict: 'fail'
			},
			{
				mode: 'replace',
				file: replaceFile,
				relativePath: 'r.txt',
				fileId: 'file-1',
				folderId: 'folder-root',
				expectedHash: 'a'.repeat(64)
			},
			{
				mode: 'create',
				file: keepFile,
				relativePath: 'BlockedDir/sub/x.txt',
				parentRelDir: 'BlockedDir/sub',
				onConflict: 'rename'
			}
		]);

		expect(plan.skippedIdenticalCount).toBe(1);
		expect(plan.skippedConflictCount).toBe(0);
	});
});
