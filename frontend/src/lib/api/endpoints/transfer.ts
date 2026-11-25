/** Transfer preflight & conflict-aware move/copy endpoints (`POST /api/folders/{id}/transfer*`). */
import { apiErrorFromResponse, apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type {
	ItemKind,
	TransferDecisionAction,
	TransferExecuteRequest,
	TransferExecuteResponse,
	TransferFileConflictDto,
	TransferFolderConflictDto,
	TransferItemDecision,
	TransferItemOutcome,
	TransferKindConflictDto,
	TransferOperation,
	TransferOutcomeStatus,
	TransferPreflightResponse
} from '$lib/api/generated/openapi/types.gen';

export type {
	ItemKind,
	TransferDecisionAction,
	TransferExecuteRequest,
	TransferExecuteResponse,
	TransferFileConflictDto,
	TransferFolderConflictDto,
	TransferItemDecision,
	TransferItemOutcome,
	TransferKindConflictDto,
	TransferOperation,
	TransferOutcomeStatus,
	TransferPreflightResponse
};

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/** Matches backend `MAX_TRANSFER_ITEMS` in `src/application/dtos/transfer_dto.rs`. */
export const TRANSFER_BATCH_SIZE = 1000;

export function emptyTransferPreflight(): TransferPreflightResponse {
	return {
		supported: true,
		cycle_folder_ids: [],
		same_parent_file_ids: [],
		same_parent_folder_ids: [],
		file_conflicts: [],
		folder_conflicts: [],
		kind_conflicts: []
	};
}

/**
 * Partition top-level file/folder IDs into batches whose combined length stays
 * within `TRANSFER_BATCH_SIZE` (preserving order so small/normal transfers stay
 * in a single HTTP request).
 */
function sliceTransferIds(
	fileIds: string[],
	folderIds: string[]
): Array<{ fileIds: string[]; folderIds: string[] }> {
	if (fileIds.length + folderIds.length <= TRANSFER_BATCH_SIZE) {
		return [{ fileIds, folderIds }];
	}
	const chunks: Array<{ fileIds: string[]; folderIds: string[] }> = [];
	let fi = 0;
	let di = 0;
	while (fi < fileIds.length || di < folderIds.length) {
		const takeFiles = Math.min(TRANSFER_BATCH_SIZE, fileIds.length - fi);
		const fileSlice = fileIds.slice(fi, fi + takeFiles);
		fi += takeFiles;
		const remaining = TRANSFER_BATCH_SIZE - fileSlice.length;
		const takeFolders = Math.min(remaining, folderIds.length - di);
		const folderSlice = folderIds.slice(di, di + takeFolders);
		di += takeFolders;
		chunks.push({ fileIds: fileSlice, folderIds: folderSlice });
	}
	return chunks;
}

/**
 * Inspect internal move/copy destinations before mutation (`POST /api/folders/{id}/transfer-preflight`),
 * short-circuiting cleanly on external mounts (`supported: false`).
 */
export async function transferPreflight(
	targetFolderId: string,
	req: {
		operation: TransferOperation;
		fileIds: string[];
		folderIds: string[];
		expandFolderConflicts?: boolean;
	},
	signal?: AbortSignal
): Promise<TransferPreflightResponse> {
	if (req.fileIds.length === 0 && req.folderIds.length === 0) {
		return emptyTransferPreflight();
	}

	const merged = emptyTransferPreflight();
	const chunks = sliceTransferIds(req.fileIds, req.folderIds);
	for (const chunk of chunks) {
		const res = await apiFetch(`/api/folders/${targetFolderId}/transfer-preflight`, {
			method: 'POST',
			credentials: 'same-origin',
			headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
			body: JSON.stringify({
				operation: req.operation,
				file_ids: chunk.fileIds,
				folder_ids: chunk.folderIds,
				expand_folder_conflicts: req.expandFolderConflicts ?? true
			}),
			signal
		});
		if (!res.ok) throw await apiErrorFromResponse(res);
		const data = (await res.json()) as TransferPreflightResponse;
		if (!data.supported) {
			return { ...emptyTransferPreflight(), supported: false };
		}
		merged.cycle_folder_ids.push(...data.cycle_folder_ids);
		merged.same_parent_file_ids.push(...data.same_parent_file_ids);
		merged.same_parent_folder_ids.push(...data.same_parent_folder_ids);
		merged.file_conflicts.push(...data.file_conflicts);
		merged.folder_conflicts.push(...data.folder_conflicts);
		merged.kind_conflicts.push(...data.kind_conflicts);
	}
	return merged;
}

/**
 * Execute an internal move/copy with explicit per-item conflict resolutions
 * (`POST /api/folders/{id}/transfer`).
 */
export async function executeTransfer(
	targetFolderId: string,
	req: {
		operation: TransferOperation;
		fileIds: string[];
		folderIds: string[];
		decisions: TransferItemDecision[];
	},
	signal?: AbortSignal
): Promise<TransferExecuteResponse> {
	const chunks = sliceTransferIds(req.fileIds, req.folderIds);
	const aggregate: TransferExecuteResponse = {
		outcomes: [],
		moved_count: 0,
		copied_count: 0,
		replaced_count: 0,
		renamed_count: 0,
		merged_count: 0,
		skipped_count: 0,
		failed_count: 0
	};

	if (chunks.length === 1) {
		const res = await apiFetch(`/api/folders/${targetFolderId}/transfer`, {
			method: 'POST',
			credentials: 'same-origin',
			headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
			body: JSON.stringify({
				operation: req.operation,
				file_ids: req.fileIds,
				folder_ids: req.folderIds,
				decisions: req.decisions
			}),
			signal
		});
		if (!res.ok) throw await apiErrorFromResponse(res);
		return (await res.json()) as TransferExecuteResponse;
	}

	const folderDecisionMap = new Map(
		req.folderIds.map((id) => [
			id,
			req.decisions.filter((d) => d.kind === 'folder' && d.source_id === id)
		])
	);
	const fileDecisionMap = new Map(
		req.fileIds.map((id) => [
			id,
			req.decisions.filter((d) => d.kind === 'file' && d.source_id === id)
		])
	);
	// Also forward any nested folder-merge child decisions alongside their chunk.
	const topKeys = new Set([
		...req.fileIds.map((id) => `file:${id}`),
		...req.folderIds.map((id) => `folder:${id}`)
	]);
	const nestedDecisions = req.decisions.filter((d) => !topKeys.has(`${d.kind}:${d.source_id}`));

	for (let i = 0; i < chunks.length; i++) {
		const chunk = chunks[i];
		const chunkDecisions: TransferItemDecision[] = [
			...chunk.fileIds.flatMap((id) => fileDecisionMap.get(id) ?? []),
			...chunk.folderIds.flatMap((id) => folderDecisionMap.get(id) ?? []),
			...(i === 0 ? nestedDecisions : [])
		];
		const res = await apiFetch(`/api/folders/${targetFolderId}/transfer`, {
			method: 'POST',
			credentials: 'same-origin',
			headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
			body: JSON.stringify({
				operation: req.operation,
				file_ids: chunk.fileIds,
				folder_ids: chunk.folderIds,
				decisions: chunkDecisions
			}),
			signal
		});
		if (!res.ok) throw await apiErrorFromResponse(res);
		const data = (await res.json()) as TransferExecuteResponse;
		aggregate.outcomes.push(...data.outcomes);
		aggregate.moved_count += data.moved_count;
		aggregate.copied_count += data.copied_count;
		aggregate.replaced_count += data.replaced_count;
		aggregate.renamed_count += data.renamed_count;
		aggregate.merged_count += data.merged_count;
		aggregate.skipped_count += data.skipped_count;
		aggregate.failed_count += data.failed_count;
	}
	return aggregate;
}
