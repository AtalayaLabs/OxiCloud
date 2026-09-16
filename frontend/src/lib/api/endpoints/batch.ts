/**
 * Batch operations (/api/batch/*). Used for multi-item copy — move and delete
 * already have per-item endpoints the files view loops over, but copy only
 * exists as a batch endpoint on the backend.
 */
import { ApiError, apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/**
 * One item's failure, as `BatchOperationResponse.failed[]` carries it.
 * `error_type` is the same vocabulary single-item endpoints put on
 * `ErrorResponse`, which is what lets a batch failure be reported as
 * precisely as a single-item one.
 */
interface BatchFailure {
	id: string;
	error: string;
	error_type: string;
}

/**
 * A failed batch does NOT answer in the `{ error }` shape the rest of the API
 * uses — it returns `{ successful, failed[], stats }`. Reading only `error` /
 * `message` therefore always missed, and every batch failure surfaced as
 * "/api/batch/files/copy failed: 400" no matter the cause.
 */
async function post(url: string, body: unknown): Promise<void> {
	const res = await apiFetch(url, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (res.ok) return;

	const payload = (await res.json().catch(() => null)) as {
		error?: string;
		message?: string;
		failed?: BatchFailure[];
	} | null;

	// The first failure speaks for the batch. When items failed for different
	// reasons the server already declined to pick a status (it returns 400),
	// and a toast can only carry one sentence anyway — the per-item detail is
	// in the response for any caller that wants to render it properly.
	const first = payload?.failed?.[0];
	throw new ApiError(
		res.status,
		res.statusText,
		url,
		first?.error_type,
		first?.error ?? payload?.error ?? payload?.message
	);
}

export function copyFiles(fileIds: string[], targetFolderId: string | null): Promise<void> {
	if (fileIds.length === 0) return Promise.resolve();
	return post('/api/batch/files/copy', { file_ids: fileIds, target_folder_id: targetFolderId });
}

export function copyFolders(folderIds: string[], targetFolderId: string | null): Promise<void> {
	if (folderIds.length === 0) return Promise.resolve();
	return post('/api/batch/folders/copy', {
		folder_ids: folderIds,
		target_folder_id: targetFolderId
	});
}
