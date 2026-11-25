import type { UploadConflictPolicy } from '$lib/api/endpoints/files';
import type { DroppedEntry } from '$lib/utils/droppedEntries';

export type ConflictResolution = 'skip' | 'replace' | 'keep_both';
export type BulkConflictAction = 'skip_all' | 'replace_all' | 'replace_if_newer' | 'keep_both_all';
export type MtimeComparison = 'local_newer' | 'remote_newer' | 'same';

export interface ExistingFileMeta {
	id: string;
	folderId: string;
	name: string;
	size: number;
	modifiedAt: string;
	contentHash: string;
	canUpdate: boolean;
}

export interface UploadConflictItem {
	/** Stable identifier matching `relativePath`. */
	id: string;
	relativePath: string;
	kind: 'content' | 'kind_mismatch';
	blockingKind?: 'file' | 'folder';
	blockingPath?: string;
	file: File;
	localSize: number;
	localModifiedMs: number;
	remote?: ExistingFileMeta;
	/** False for read-only / Contributor permissions or kind mismatches. */
	canReplace: boolean;
	comparison: MtimeComparison | null;
	/** Illustrative disambiguated name when choosing "Keep both". */
	keepBothPreviewName: string;
}

export interface PreflightKindConflict {
	relativePath: string;
	existingKind: 'file' | 'folder';
	existingId: string;
}

export interface UploadPreflightAnalysis {
	preflightSupported: boolean;
	/** Relative directory path (e.g. "Photos/2026") -> existing server folder ID. */
	existingFolders: ReadonlyMap<string, string>;
	kindConflicts: ReadonlyArray<PreflightKindConflict>;
	newEntries: DroppedEntry[];
	identicalEntries: DroppedEntry[];
	conflicts: UploadConflictItem[];
	/** Whole-file hashes already computed during preflight comparison. */
	precomputedHashes: Map<File, string>;
}

export interface PlannedFolderCreation {
	relativePath: string;
	parentRelDir: string;
	name: string;
	onConflict: UploadConflictPolicy;
}

export type PlannedFileTransfer =
	| {
			mode: 'create';
			file: File;
			relativePath: string;
			parentRelDir: string;
			onConflict: UploadConflictPolicy;
	  }
	| {
			mode: 'replace';
			file: File;
			relativePath: string;
			fileId: string;
			folderId: string;
			expectedHash: string;
	  };

export interface UploadExecutionPlan {
	foldersToCreate: PlannedFolderCreation[];
	transfers: PlannedFileTransfer[];
	skippedIdenticalCount: number;
	skippedConflictCount: number;
	precomputedHashes: Map<File, string>;
}

const COMPOUND_EXTENSIONS = ['.tar.gz', '.tar.xz', '.tar.bz2', '.tar.zst', '.tar.lz4', '.tar.br'];

/**
 * Preview a `(2)` disambiguated sibling filename using the same rules as the
 * server's `domain::services::name_conflict::split_stem_and_extension`:
 * keeps compound extensions (`.tar.gz`), protects leading dotfiles (`.env`),
 * and splits secondary dotfile suffixes (`.env.local` -> `.env (2).local`).
 */
export function formatKeepBothPreview(name: string, attempt = 2): string {
	const lower = name.toLowerCase();
	for (const ext of COMPOUND_EXTENSIONS) {
		if (lower.endsWith(ext) && name.length > ext.length) {
			const cut = name.length - ext.length;
			const stem = name.slice(0, cut);
			if (stem.length > 0 && stem !== '.') {
				return `${stem} (${attempt})${name.slice(cut)}`;
			}
		}
	}
	const dot = name.lastIndexOf('.');
	if (dot > 0 && dot < name.length - 1) {
		return `${name.slice(0, dot)} (${attempt})${name.slice(dot)}`;
	}
	return `${name} (${attempt})`;
}

/**
 * Compare a local file's millisecond timestamp against the server's ISO-8601
 * `content_modified_at`. Rounds both to whole seconds so sub-millisecond
 * truncation on PostgreSQL storage cannot trigger false "Local newer" hints.
 */
export function compareMtimeSeconds(localMs: number, remoteIso: string): MtimeComparison | null {
	if (!localMs || !Number.isFinite(localMs)) return null;
	const parsedRemoteMs = Date.parse(remoteIso);
	if (Number.isNaN(parsedRemoteMs)) return null;
	const localSec = Math.round(localMs / 1000);
	const remoteSec = Math.round(parsedRemoteMs / 1000);
	if (localSec > remoteSec) return 'local_newer';
	if (localSec < remoteSec) return 'remote_newer';
	return 'same';
}

/** Safe default when a conflict appears: never destroy remote bytes unprompted. */
export function defaultResolutionFor(_conflict: UploadConflictItem): ConflictResolution {
	return 'skip';
}

/** Compute per-conflict resolutions for one bulk action, respecting permissions & kind mismatches. */
export function applyBulkAction(
	conflicts: readonly UploadConflictItem[],
	action: BulkConflictAction
): Record<string, ConflictResolution> {
	const out: Record<string, ConflictResolution> = {};
	for (const c of conflicts) {
		switch (action) {
			case 'skip_all':
				out[c.id] = 'skip';
				break;
			case 'keep_both_all':
				out[c.id] = 'keep_both';
				break;
			case 'replace_all':
				out[c.id] = c.canReplace ? 'replace' : 'skip';
				break;
			case 'replace_if_newer':
				out[c.id] = c.canReplace && c.comparison === 'local_newer' ? 'replace' : 'skip';
				break;
		}
	}
	return out;
}

function splitParentAndName(relPath: string): { parentRelDir: string; name: string } {
	const slash = relPath.lastIndexOf('/');
	if (slash < 0) return { parentRelDir: '', name: relPath };
	return {
		parentRelDir: relPath.slice(0, slash),
		name: relPath.slice(slash + 1)
	};
}

/**
 * Turn preflight analysis + user conflict decisions into an executable folder
 * creation order and file transfer list.
 *
 * Folders already present in `analysis.existingFolders` are reused directly
 * without `POST /api/folders`. Ancestor directories blocked by an existing file
 * are scheduled with `onConflict: 'rename'` only if at least one descendant
 * chooses `keep_both`.
 */
export function buildExecutionPlan(
	analysis: UploadPreflightAnalysis,
	resolutions: Readonly<Record<string, ConflictResolution>>
): UploadExecutionPlan {
	const transfers: PlannedFileTransfer[] = [];
	const neededDirs = new Set<string>();
	let skippedConflictCount = 0;

	const kindConflictDirs = new Set(
		analysis.kindConflicts.filter((kc) => kc.existingKind === 'file').map((kc) => kc.relativePath)
	);

	const addAncestors = (relDir: string) => {
		let cur = relDir;
		while (cur.length > 0) {
			neededDirs.add(cur);
			const slash = cur.lastIndexOf('/');
			cur = slash < 0 ? '' : cur.slice(0, slash);
		}
	};

	for (const entry of analysis.newEntries) {
		const { parentRelDir } = splitParentAndName(entry.relativePath);
		addAncestors(parentRelDir);
		transfers.push({
			mode: 'create',
			file: entry.file,
			relativePath: entry.relativePath,
			parentRelDir,
			onConflict: 'fail'
		});
	}

	for (const conflict of analysis.conflicts) {
		const chosen = resolutions[conflict.id] ?? defaultResolutionFor(conflict);
		if (chosen === 'skip') {
			skippedConflictCount += 1;
			continue;
		}
		if (chosen === 'replace') {
			if (!conflict.canReplace || !conflict.remote) {
				skippedConflictCount += 1;
				continue;
			}
			transfers.push({
				mode: 'replace',
				file: conflict.file,
				relativePath: conflict.relativePath,
				fileId: conflict.remote.id,
				folderId: conflict.remote.folderId,
				expectedHash: conflict.remote.contentHash
			});
			continue;
		}
		// 'keep_both'
		const { parentRelDir } = splitParentAndName(conflict.relativePath);
		addAncestors(parentRelDir);
		transfers.push({
			mode: 'create',
			file: conflict.file,
			relativePath: conflict.relativePath,
			parentRelDir,
			onConflict: 'rename'
		});
	}

	const sortedDirs = [...neededDirs]
		.filter((dir) => !analysis.existingFolders.has(dir))
		.sort((a, b) => {
			const depthDiff = a.split('/').length - b.split('/').length;
			return depthDiff !== 0 ? depthDiff : a.localeCompare(b);
		});

	const foldersToCreate: PlannedFolderCreation[] = sortedDirs.map((relDir) => {
		const { parentRelDir, name } = splitParentAndName(relDir);
		return {
			relativePath: relDir,
			parentRelDir,
			name,
			onConflict: kindConflictDirs.has(relDir) ? 'rename' : 'fail'
		};
	});

	return {
		foldersToCreate,
		transfers,
		skippedIdenticalCount: analysis.identicalEntries.length,
		skippedConflictCount,
		precomputedHashes: analysis.precomputedHashes
	};
}
