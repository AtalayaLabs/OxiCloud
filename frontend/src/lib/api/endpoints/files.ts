import { uploadTransport } from '../upload-transport';
/** File endpoints — ported from fileOperations.js. */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/**
 * Instant upload: materialise a file from a blob the caller **already owns**,
 * by its whole-file BLAKE3 — zero content bytes cross the wire. Returns the HTTP
 * status so the caller can fall back to a plain upload on 404 (hash not owned).
 * Scoped to the caller's own content server-side (no cross-user probing).
 */
export async function createFileByHash(
	folderId: string,
	name: string,
	hash: string
): Promise<{ ok: boolean; status: number; data?: unknown }> {
	const res = await apiFetch('/api/files/by-hash', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ name, folder_id: folderId, hash })
	});
	const data = res.ok ? await res.json().catch(() => undefined) : undefined;
	return { ok: res.ok, status: res.status, data };
}

/**
 * Batch dedup check: given candidate whole-file BLAKE3 hashes, return the set
 * the caller **already owns** — in a single round trip. Drives instant uploads:
 * a file whose hash is in the set can be created with zero content bytes.
 * Resolves an empty set on any failure, so the caller just uploads everything.
 */
export async function dedupCheckBatch(hashes: string[]): Promise<Set<string>> {
	if (hashes.length === 0) return new Set();
	const res = await apiFetch('/api/dedup/check-batch', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ hashes })
	});
	if (!res.ok) return new Set();
	const data = (await res.json().catch(() => null)) as { owned?: string[] } | null;
	return new Set(data?.owned ?? []);
}

export async function uploadFile(folderId: string | null, file: File): Promise<void> {
	const form = new FormData();
	if (folderId) form.append('folder_id', folderId);
	form.append('file', file);
	const res = await apiFetch('/api/files/upload', {
		method: 'POST',
		credentials: 'same-origin',
		cache: 'no-store',
		headers: getCsrfHeaders(), // multipart boundary set automatically; do not set Content-Type
		body: form
	});
	if (!res.ok) throw new Error(`upload failed: ${res.status}`);
}

/** Upload through the generated client with an XHR transport for progress events. */
export async function uploadFileWithProgress(
	folderId: string | null,
	file: File,
	onProgress: (fraction: number) => void
): Promise<void> {
	const form = new FormData();
	if (folderId) form.append('folder_id', folderId);
	form.append('file', file);
	const response = await apiFetch('/api/files/upload', {
		method: 'POST',
		body: form,
		fetch: uploadTransport(form, onProgress)
	});
	if (!response.ok) {
		const error = new Error(`upload failed: ${response.status}`) as Error & { isQuota?: boolean };
		error.isQuota = response.status === 507;
		throw error;
	}
}

export async function renameFile(fileId: string, name: string): Promise<void> {
	const res = await apiFetch(`/api/files/${fileId}/rename`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ name })
	});
	if (!res.ok) throw new Error(`rename file failed: ${res.status}`);
}

export async function moveFile(fileId: string, targetFolderId: string | null): Promise<void> {
	const res = await apiFetch(`/api/files/${fileId}/move`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ folder_id: targetFolderId || null })
	});
	if (!res.ok) throw new Error(`move file failed: ${res.status}`);
}

export async function deleteFile(fileId: string): Promise<void> {
	const res = await apiFetch(`/api/files/${fileId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`delete file failed: ${res.status}`);
}

export function fileDownloadUrl(fileId: string): string {
	return `/api/files/${fileId}`;
}

export function fileInlineUrl(fileId: string): string {
	return `/api/files/${fileId}?inline=true`;
}

/** Thumbnail URL for a file at the given size (server-rendered, content-typed). */
export function fileThumbnailUrl(
	fileId: string,
	size: 'icon' | 'preview' | 'large' = 'preview'
): string {
	return `/api/files/${fileId}/thumbnail/${size}`;
}

/**
 * Thumbnail size matched to the rendering slot. List rows draw thumbnails in
 * a 40×40 box, so the 150px `icon` rendition is already ≥2× retina density —
 * fetching the 400px `preview` there moved ~7× more pixels than the slot can
 * show (benches/ROUND12.md §F1). Grid cards (100×70 slot) keep `preview`.
 */
export function thumbSizeForView(view: 'grid' | 'list'): 'icon' | 'preview' {
	return view === 'list' ? 'icon' : 'preview';
}
