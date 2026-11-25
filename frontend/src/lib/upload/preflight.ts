import { hashFilesPooled } from '$lib/api/endpoints/deltaUpload';
import { uploadPreflight } from '$lib/api/endpoints/folders';
import type {
	PreflightExistingFileDto,
	PreflightKindConflictDto
} from '$lib/api/generated/openapi/types.gen';
import type { DroppedEntry } from '$lib/utils/droppedEntries';
import {
	compareMtimeSeconds,
	formatKeepBothPreview,
	type UploadConflictItem,
	type UploadPreflightAnalysis
} from './conflictPlan';

export function normalizePreflightRelPath(raw: string): string {
	return raw.replace(/^(?:\.\/|\/)+/, '').normalize('NFC');
}

function basenameOf(relPath: string): string {
	const idx = relPath.lastIndexOf('/');
	return idx < 0 ? relPath : relPath.slice(idx + 1);
}

function prefixesOf(relPath: string): string[] {
	const parts = relPath.split('/');
	const out: string[] = [];
	let acc = '';
	for (const part of parts) {
		acc = acc ? `${acc}/${part}` : part;
		out.push(acc);
	}
	return out;
}

interface SameSizeCandidate {
	index: number;
	entry: DroppedEntry;
	remote: PreflightExistingFileDto;
}

/**
 * Inspect dropped files/folders against existing server contents BEFORE sending
 * any content bytes.
 *
 * - Different file sizes become immediate content conflicts (0 bytes hashed).
 * - Equal non-zero sizes are verified by streaming BLAKE3 via `hashFilesPooled`;
 *   matching hashes are silently classified as identical and skipped, while
 *   computed hashes stay in `precomputedHashes` so `resolveOwnedHashes` never
 *   hashes the same file twice.
 * - Ancestor paths colliding with a server file propagate `kind_mismatch` down
 *   to every descendant so the user can skip or choose Keep both cleanly.
 * - External mounts (`supported === false`) fall back gracefully without halting.
 */
export async function analyzeUploadCandidates(
	targetFolderId: string,
	entries: ReadonlyArray<DroppedEntry>,
	opts?: {
		signal?: AbortSignal;
		onHashProgress?: (done: number, total: number) => void;
	}
): Promise<UploadPreflightAnalysis> {
	const precomputedHashes = new Map<File, string>();
	if (entries.length === 0) {
		return {
			preflightSupported: true,
			existingFolders: new Map(),
			kindConflicts: [],
			newEntries: [],
			identicalEntries: [],
			conflicts: [],
			precomputedHashes
		};
	}

	const normalizedEntries: DroppedEntry[] = entries.map((e) => ({
		file: e.file,
		relativePath: normalizePreflightRelPath(e.relativePath)
	}));

	const response = await uploadPreflight(
		targetFolderId,
		normalizedEntries.map((e) => e.relativePath),
		opts?.signal
	);

	if (!response || !response.supported) {
		return {
			preflightSupported: false,
			existingFolders: new Map(),
			kindConflicts: [],
			newEntries: normalizedEntries,
			identicalEntries: [],
			conflicts: [],
			precomputedHashes
		};
	}

	const existingFolders = new Map<string, string>();
	for (const folder of response.existing_folders) {
		existingFolders.set(folder.relative_path, folder.id);
	}

	const existingFilesByPath = new Map<string, PreflightExistingFileDto>();
	for (const file of response.existing_files) {
		existingFilesByPath.set(file.relative_path, file);
	}

	const kindConflictByPath = new Map<string, PreflightKindConflictDto>();
	for (const kc of response.kind_conflicts) {
		kindConflictByPath.set(kc.relative_path, kc);
	}

	const newEntries: DroppedEntry[] = [];
	const identicalEntries: DroppedEntry[] = [];
	const indexedConflicts: Array<{ index: number; item: UploadConflictItem }> = [];
	const sameSizeCandidates: SameSizeCandidate[] = [];

	const pushContentConflict = (
		index: number,
		entry: DroppedEntry,
		remote: PreflightExistingFileDto
	) => {
		const name = basenameOf(entry.relativePath);
		indexedConflicts.push({
			index,
			item: {
				id: entry.relativePath,
				relativePath: entry.relativePath,
				kind: 'content',
				file: entry.file,
				localSize: entry.file.size,
				localModifiedMs: entry.file.lastModified,
				remote: {
					id: remote.id,
					folderId: remote.folder_id,
					name: remote.name,
					size: remote.size,
					modifiedAt: remote.modified_at,
					contentHash: remote.content_hash,
					canUpdate: remote.can_update
				},
				canReplace: remote.can_update,
				comparison: compareMtimeSeconds(entry.file.lastModified, remote.modified_at),
				keepBothPreviewName: formatKeepBothPreview(name)
			}
		});
	};

	for (const [index, entry] of normalizedEntries.entries()) {
		let blockingKindConflict: PreflightKindConflictDto | undefined;
		for (const prefix of prefixesOf(entry.relativePath)) {
			const match = kindConflictByPath.get(prefix);
			if (match) {
				blockingKindConflict = match;
				break;
			}
		}

		if (blockingKindConflict) {
			const name = basenameOf(entry.relativePath);
			indexedConflicts.push({
				index,
				item: {
					id: entry.relativePath,
					relativePath: entry.relativePath,
					kind: 'kind_mismatch',
					blockingKind: blockingKindConflict.existing_kind,
					blockingPath: blockingKindConflict.relative_path,
					file: entry.file,
					localSize: entry.file.size,
					localModifiedMs: entry.file.lastModified,
					canReplace: false,
					comparison: null,
					keepBothPreviewName: formatKeepBothPreview(name)
				}
			});
			continue;
		}

		const remote = existingFilesByPath.get(entry.relativePath);
		if (!remote) {
			newEntries.push(entry);
			continue;
		}

		if (entry.file.size === 0 && remote.size === 0) {
			identicalEntries.push(entry);
			continue;
		}

		if (entry.file.size !== remote.size) {
			pushContentConflict(index, entry, remote);
			continue;
		}

		sameSizeCandidates.push({ index, entry, remote });
	}

	if (sameSizeCandidates.length > 0) {
		const hashed = await hashFilesPooled(
			sameSizeCandidates.map((c) => c.entry.file),
			{
				signal: opts?.signal,
				onProgress: opts?.onHashProgress
			}
		);
		for (const [file, hex] of hashed) {
			precomputedHashes.set(file, hex);
		}
		for (const { index, entry, remote } of sameSizeCandidates) {
			const localHash = precomputedHashes.get(entry.file);
			if (localHash && localHash.toLowerCase() === remote.content_hash.toLowerCase()) {
				identicalEntries.push(entry);
			} else {
				pushContentConflict(index, entry, remote);
			}
		}
	}

	indexedConflicts.sort((a, b) => a.index - b.index);
	const conflicts = indexedConflicts.map((c) => c.item);

	return {
		preflightSupported: true,
		existingFolders,
		kindConflicts: response.kind_conflicts.map((kc) => ({
			relativePath: kc.relative_path,
			existingKind: kc.existing_kind,
			existingId: kc.existing_id
		})),
		newEntries,
		identicalEntries,
		conflicts,
		precomputedHashes
	};
}
