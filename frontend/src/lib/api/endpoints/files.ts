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

/**
 * Upload with progress reporting. `fetch` can't surface upload progress, so this
 * uses XHR; CSRF headers are attached the same way as {@link uploadFile}.
 * `onProgress` receives a fraction in [0, 1] (or NaN when length is unknown).
 *
 * DPoP proof is minted per attempt and attached as a `DPoP` header, mirroring
 * the `apiFetch` interceptor — required for bound sessions under `required`
 * mode (server 401s any state-changing call otherwise). Fresh `DPoP-Nonce`
 * from the response is pushed into the shared nonce cache so the next
 * request (through either apiFetch or another XHR) stays in sync. On a
 * `use_dpop_nonce` challenge the upload is retried ONCE with the freshly-
 * harvested nonce.
 */
export async function uploadFileWithProgress(
	folderId: string | null,
	file: File,
	onProgress: (fraction: number) => void
): Promise<void> {
	// Dynamic import — falls back to a headerless XHR if the DPoP module
	// isn't loadable (SubtleCrypto disabled, IndexedDB blocked, etc.).
	// Bound sessions in `required` mode still 401, but that's the fail-
	// open contract already documented for other DPoP-aware raw callers
	// (`fetchMe`).
	let dpopMod: typeof import('$lib/auth/dpop-proof') | null = null;
	try {
		dpopMod = await import('$lib/auth/dpop-proof');
	} catch {
		/* no dpop module → plain XHR */
	}
	const url = `${location.origin}/api/files/upload`;

	const attempt = (): Promise<void> =>
		new Promise((resolve, reject) => {
			const form = new FormData();
			if (folderId) form.append('folder_id', folderId);
			form.append('file', file);
			const xhr = new XMLHttpRequest();
			xhr.open('POST', '/api/files/upload');
			xhr.withCredentials = true;
			for (const [k, v] of Object.entries(getCsrfHeaders())) xhr.setRequestHeader(k, v);

			// Self-aborting watchdog so a stalled connection can never pin an upload
			// slot forever (and leave a zombie XHR holding one of the browser's few
			// per-host connections). While the body is uploading we reset the deadline
			// on every progress tick — a slow but *moving* transfer is fine; once the
			// body is fully sent we give the server a fixed window to respond. On a
			// stall we `xhr.abort()`, which frees the connection immediately.
			const SEND_STALL_MS = 30_000;
			const RESPONSE_MS = 60_000;
			let watchdog: ReturnType<typeof setTimeout>;
			const arm = (ms: number) => {
				clearTimeout(watchdog);
				watchdog = setTimeout(() => xhr.abort(), ms);
			};

			const doSend = (proof: string | null) => {
				if (proof) xhr.setRequestHeader('DPoP', proof);
				xhr.upload.onprogress = (e) => {
					onProgress(e.lengthComputable ? e.loaded / e.total : NaN);
					arm(SEND_STALL_MS);
				};
				xhr.upload.onload = () => arm(RESPONSE_MS); // body sent — wait for the server
				xhr.onload = () => {
					clearTimeout(watchdog);
					// Sync the shared nonce cache from the response — the server
					// rotates the nonce on every response, and other callers
					// (apiFetch, fetchMe) share the same in-memory store.
					if (dpopMod) dpopMod.updateNonceFromHeader(xhr.getResponseHeader('DPoP-Nonce'));
					// Nonce challenge → surface a distinctive rejection so the outer
					// retry can re-arm a fresh XHR (the current one has already
					// consumed its request body).
					if (xhr.status === 401 && /use_dpop_nonce/i.test(xhr.getResponseHeader('WWW-Authenticate') ?? '')) {
						const err = new Error('dpop_nonce_challenge') as Error & { isNonceChallenge?: boolean };
						err.isNonceChallenge = true;
						reject(err);
						return;
					}
					if (xhr.status >= 200 && xhr.status < 300) resolve();
					else {
						// Flag quota so a batch can stop early instead of retrying every file.
						const err = new Error(`upload failed: ${xhr.status}`) as Error & { isQuota?: boolean };
						err.isQuota = xhr.status === 507;
						reject(err);
					}
				};
				xhr.onerror = () => {
					clearTimeout(watchdog);
					reject(new Error('upload failed: network error'));
				};
				xhr.onabort = () => {
					clearTimeout(watchdog);
					reject(new Error('upload stalled — aborted'));
				};
				arm(SEND_STALL_MS);
				xhr.send(form);
			};

			if (dpopMod) {
				dpopMod
					.buildDpopProof('POST', url)
					.catch(() => null)
					.then(doSend);
			} else {
				doSend(null);
			}
		});

	try {
		await attempt();
	} catch (err) {
		if ((err as { isNonceChallenge?: boolean } | null)?.isNonceChallenge) {
			// Nonce was harvested by the failed attempt's onload; retry ONCE.
			// A second challenge would loop, so any further failure surfaces.
			await attempt();
			return;
		}
		throw err;
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
