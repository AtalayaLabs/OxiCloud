import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('$lib/api/endpoints/folders', () => ({
	uploadPreflight: vi.fn()
}));
vi.mock('$lib/api/endpoints/deltaUpload', () => ({
	hashFilesPooled: vi.fn()
}));

import { hashFilesPooled } from '$lib/api/endpoints/deltaUpload';
import { uploadPreflight } from '$lib/api/endpoints/folders';
import { analyzeUploadCandidates } from './preflight';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function file(name: string, size: number, lastModified = 1_704_067_205_000): File {
	return new File([new Uint8Array(size)], name, { type: 'text/plain', lastModified });
}

describe('analyzeUploadCandidates', () => {
	beforeEach(() => {
		vi.clearAllMocks();
	});

	it('detects 0-byte and same-hash identical files, skips hashing mismatched sizes, and caches computed hashes', async () => {
		const fIdenticalEmpty = file('empty.txt', 0);
		const fSameSizeIdentical = file('same.txt', 16);
		const fSameSizeChanged = file('changed.txt', 16, 1_704_067_210_000);
		const fDiffSize = file('diff.txt', 32);
		const fReadOnly = file('locked.txt', 8);
		const fNew = file('fresh.txt', 10);

		const sameHash = 'a'.repeat(64);
		const oldChangedHash = 'b'.repeat(64);
		const newChangedHash = 'c'.repeat(64);

		m(uploadPreflight).mockResolvedValue({
			supported: true,
			existing_folders: [{ relative_path: 'Sub', id: 'dir-sub' }],
			existing_files: [
				{
					relative_path: 'empty.txt',
					id: 'id-empty',
					folder_id: 'root',
					name: 'empty.txt',
					size: 0,
					modified_at: '2024-01-01T00:00:00Z',
					content_hash: 'e'.repeat(64),
					can_update: true
				},
				{
					relative_path: 'Sub/same.txt',
					id: 'id-same',
					folder_id: 'dir-sub',
					name: 'same.txt',
					size: 16,
					modified_at: '2024-01-01T00:00:00Z',
					content_hash: sameHash,
					can_update: true
				},
				{
					relative_path: 'Sub/changed.txt',
					id: 'id-changed',
					folder_id: 'dir-sub',
					name: 'changed.txt',
					size: 16,
					modified_at: '2024-01-01T00:00:00Z',
					content_hash: oldChangedHash,
					can_update: true
				},
				{
					relative_path: 'diff.txt',
					id: 'id-diff',
					folder_id: 'root',
					name: 'diff.txt',
					size: 64,
					modified_at: '2024-01-01T00:00:00Z',
					content_hash: 'd'.repeat(64),
					can_update: true
				},
				{
					relative_path: 'locked.txt',
					id: 'id-locked',
					folder_id: 'root',
					name: 'locked.txt',
					size: 20,
					modified_at: '2024-01-01T00:00:00Z',
					content_hash: 'f'.repeat(64),
					can_update: false
				}
			],
			kind_conflicts: []
		});

		m(hashFilesPooled).mockImplementation(async (files: File[]) => {
			// Only same-size non-empty candidates are hashed!
			expect(files).toEqual([fSameSizeIdentical, fSameSizeChanged]);
			return new Map([
				[fSameSizeIdentical, sameHash.toUpperCase()],
				[fSameSizeChanged, newChangedHash]
			]);
		});

		const analysis = await analyzeUploadCandidates('root', [
			{ file: fIdenticalEmpty, relativePath: './empty.txt' },
			{ file: fSameSizeIdentical, relativePath: 'Sub/same.txt' },
			{ file: fSameSizeChanged, relativePath: 'Sub/changed.txt' },
			{ file: fDiffSize, relativePath: 'diff.txt' },
			{ file: fReadOnly, relativePath: 'locked.txt' },
			{ file: fNew, relativePath: 'Sub/fresh.txt' }
		]);

		expect(analysis.preflightSupported).toBe(true);
		expect(analysis.existingFolders.get('Sub')).toBe('dir-sub');
		expect(analysis.newEntries.map((e) => e.relativePath)).toEqual(['Sub/fresh.txt']);
		expect(analysis.identicalEntries.map((e) => e.relativePath)).toEqual([
			'empty.txt',
			'Sub/same.txt'
		]);

		expect(analysis.conflicts.map((c) => c.relativePath)).toEqual([
			'Sub/changed.txt',
			'diff.txt',
			'locked.txt'
		]);
		expect(analysis.conflicts[0].comparison).toBe('local_newer');
		expect(analysis.conflicts[2].canReplace).toBe(false);

		// Computed hashes are preserved for resolveOwnedHashes reuse (no double hashing).
		expect(analysis.precomputedHashes.get(fSameSizeChanged)).toBe(newChangedHash);
		expect(analysis.precomputedHashes.has(fDiffSize)).toBe(false);
	});

	it('propagates ancestor kind conflicts to nested files and falls back cleanly on external mounts', async () => {
		const nested = file('inner.txt', 5);
		m(uploadPreflight).mockResolvedValueOnce({
			supported: true,
			existing_folders: [],
			existing_files: [],
			kind_conflicts: [
				{ relative_path: 'BlockedDir', existing_kind: 'file', existing_id: 'f-block' }
			]
		});

		const analysis = await analyzeUploadCandidates('root', [
			{ file: nested, relativePath: 'BlockedDir/sub/inner.txt' }
		]);

		expect(analysis.conflicts).toHaveLength(1);
		expect(analysis.conflicts[0].kind).toBe('kind_mismatch');
		expect(analysis.conflicts[0].blockingPath).toBe('BlockedDir');
		expect(analysis.conflicts[0].blockingKind).toBe('file');
		expect(analysis.conflicts[0].canReplace).toBe(false);

		// External mount fallback (`supported: false`):
		m(uploadPreflight).mockResolvedValueOnce({
			supported: false,
			existing_folders: [],
			existing_files: [],
			kind_conflicts: []
		});
		const ext = await analyzeUploadCandidates('ext:mnt-1', [
			{ file: nested, relativePath: 'a.txt' }
		]);
		expect(ext.preflightSupported).toBe(false);
		expect(ext.conflicts).toHaveLength(0);
		expect(ext.newEntries).toHaveLength(1);
	});
});
