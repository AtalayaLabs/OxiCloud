import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
// Keep the real `ApiError` class — the upload-path tests assert
// `rejects.toMatchObject({ name: 'ApiError', errorType: … })` and
// `instanceof ApiError` relies on the real constructor identity.
vi.mock('$lib/api/client', async () => {
	const actual = await vi.importActual<typeof import('$lib/api/client')>('$lib/api/client');
	return {
		...actual,
		apiFetch: vi.fn(),
		apiJson: vi.fn(),
		withBase: (p: string) => p
	};
});
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// Controllable DPoP proof — `uploadFileWithProgress` uses XHR (needed
// for upload-progress events) and manually signs a proof. Default null
// so pre-existing tests are unaffected; the XHR-DPoP block below opts
// in.
const dpopState = vi.hoisted(() => ({ proof: null as string | null }));
vi.mock('$lib/auth/dpop-proof', async () => {
	const actual =
		await vi.importActual<typeof import('$lib/auth/dpop-proof')>('$lib/auth/dpop-proof');
	return {
		...actual,
		buildDpopProof: vi.fn(async () => dpopState.proof)
	};
});
import { apiFetch } from '$lib/api/client';
import {
	uploadFile,
	renameFile,
	moveFile,
	deleteFile,
	fileDownloadUrl,
	fileInlineUrl,
	uploadFileWithProgress
} from './files';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
describe('files endpoint URL builders', () => {
	it('build download/inline URLs', () => {
		expect(fileDownloadUrl('id1')).toContain('id1');
		expect(fileDownloadUrl('id1')).toContain('/api/files/');
		expect(fileInlineUrl('id1')).toContain('id1');
	});
});
describe('files endpoint mutations', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		f.mockResolvedValue({ ok: true, status: 200, json: async () => ({ id: 'x' }) });
	});
	it('call the API for upload/rename/move/delete', async () => {
		const file = new File([new Uint8Array([1])], 'f.txt', { type: 'text/plain' });
		await uploadFile('fid', file).catch(() => {});
		await renameFile('id', 'new').catch(() => {});
		await moveFile('id', 'dest').catch(() => {});
		await deleteFile('id').catch(() => {});
		expect(f).toHaveBeenCalled();
	});
});

// ── uploadFileWithProgress — DPoP + XHR ──────────────────────────────
//
// The upload path uses raw XMLHttpRequest (fetch can't emit upload-
// progress events). That means it bypasses the `apiFetch` DPoP
// interceptor and has to sign a proof + handle a `use_dpop_nonce`
// challenge itself. These tests guard:
//   - DPoP header is attached before send.
//   - Progress callback fires from `upload.onprogress`.
//   - Nonce is harvested from the response into the shared cache.
//   - A `use_dpop_nonce` challenge on the first attempt triggers ONE
//     retry (fresh XHR — the failed one already consumed its body).
//   - A second challenge does NOT loop (would mask a server-side bug).
//   - A 507 rejection carries `isQuota: true` for the batch orchestrator.

/**
 * Minimal `XMLHttpRequest` stand-in — implements only the surface
 * `uploadFileWithProgress` touches. Tests drive it by calling
 * `respond(status, headers)` / `fireProgress()` / `fireError()`.
 */
class MockXHR {
	method = '';
	url = '';
	withCredentials = false;
	requestHeaders = new Map<string, string>();
	responseHeaders = new Map<string, string>();
	status = 0;
	responseText = '';
	body: unknown = null;
	upload: {
		onprogress: ((e: ProgressEvent) => void) | null;
		onload: (() => void) | null;
	} = { onprogress: null, onload: null };
	onload: (() => void) | null = null;
	onerror: (() => void) | null = null;
	onabort: (() => void) | null = null;

	open(method: string, url: string): void {
		this.method = method;
		this.url = url;
	}
	setRequestHeader(k: string, v: string): void {
		this.requestHeaders.set(k.toLowerCase(), v);
	}
	send(body: unknown): void {
		this.body = body;
	}
	abort(): void {
		queueMicrotask(() => this.onabort?.());
	}
	getResponseHeader(k: string): string | null {
		return this.responseHeaders.get(k.toLowerCase()) ?? null;
	}
	// Test helpers
	fireProgress(loaded: number, total: number): void {
		this.upload.onprogress?.({ loaded, total, lengthComputable: true } as ProgressEvent);
	}
	respond(status: number, headers: Record<string, string> = {}): void {
		this.status = status;
		for (const [k, v] of Object.entries(headers)) this.responseHeaders.set(k.toLowerCase(), v);
		this.onload?.();
	}
	/** Respond with a JSON body — stashes the stringified payload in
	 * `responseText` and sets `Content-Type: application/json` so the
	 * upload-transport's `Response` wrapper carries the parseable body. */
	respondJson(status: number, body: unknown, headers: Record<string, string> = {}): void {
		this.responseText = JSON.stringify(body);
		this.respond(status, { 'Content-Type': 'application/json', ...headers });
	}
	fireError(): void {
		this.onerror?.();
	}
}

/** Yield to microtasks + timers so `uploadFileWithProgress`'s dynamic-
 * import + `buildDpopProof` chain resolves and `xhr.send()` is reached. */
async function waitForXhr(sink: MockXHR[], idx = 0, tries = 30): Promise<MockXHR> {
	for (let i = 0; i < tries; i++) {
		if (sink[idx] && sink[idx].body !== null) return sink[idx];
		await new Promise((r) => setTimeout(r, 5));
	}
	throw new Error(`XHR #${idx} never reached send() (had ${sink.length} instance(s))`);
}

describe('uploadFileWithProgress — DPoP + XHR', () => {
	let xhrs: MockXHR[];
	beforeEach(async () => {
		const actual = await vi.importActual<typeof import('$lib/api/client')>('$lib/api/client');
		f.mockImplementation(actual.apiFetch);
		vi.clearAllMocks();
		dpopState.proof = 'proof.upload';
		xhrs = [];
		const XhrStub = class extends MockXHR {
			constructor() {
				super();
				xhrs.push(this);
			}
		};
		vi.stubGlobal('XMLHttpRequest', XhrStub as unknown as typeof XMLHttpRequest);
	});
	afterEach(() => vi.unstubAllGlobals());

	it('attaches a DPoP header on the XHR and resolves on 2xx', async () => {
		const file = new File([new Uint8Array([1, 2, 3])], 'a.txt');
		const onProgress = vi.fn();
		const p = uploadFileWithProgress('folder-1', file, onProgress);
		const xhr = await waitForXhr(xhrs);

		expect(xhr.method).toBe('POST');
		expect(xhr.url).toBe(`${location.origin}/api/files/upload`);
		expect(xhr.requestHeaders.get('dpop')).toBe('proof.upload');
		expect(xhr.withCredentials).toBe(true);

		// Simulate progress + successful completion.
		xhr.fireProgress(1, 3);
		xhr.fireProgress(3, 3);
		xhr.respond(200);
		await expect(p).resolves.toBeUndefined();
		expect(onProgress).toHaveBeenCalled();
		expect(onProgress).toHaveBeenLastCalledWith(1);
	});

	it('sends no DPoP header when the proof module has no keypair (fail-open)', async () => {
		dpopState.proof = null;
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});
		const xhr = await waitForXhr(xhrs);
		expect(xhr.requestHeaders.get('dpop')).toBeUndefined();
		xhr.respond(200);
		await p;
	});

	it('surfaces a 507 with isQuota flag for the batch orchestrator', async () => {
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});
		const xhr = await waitForXhr(xhrs);
		// Server now ships normalized `ApiErrorBody` on 507. Shape:
		//   {status, error, message, error_type: "QuotaExceeded"}
		// Pre-fix the wrapper threw a plain `Error("upload failed: 507")`
		// with no `errorType` field, which collapsed every reason to the
		// same opaque toast — the regression Ed hit on 2026-11-01 (upload
		// to an over-quota shared drive silently reported "Echec du
		// téléchargement" with no cause). The fix parses the body and
		// throws an `ApiError` so `errorMessage()` can key the friendly
		// i18n copy ("Not enough space…") off `errorType === "QuotaExceeded"`.
		xhr.respondJson(507, {
			status: '507 Insufficient Storage',
			error: 'Drive quota exceeded: 421410098 + 1802867 > 314572800 bytes',
			message: 'Drive quota exceeded: 421410098 + 1802867 > 314572800 bytes',
			error_type: 'QuotaExceeded'
		});
		await expect(p).rejects.toMatchObject({
			name: 'ApiError',
			status: 507,
			errorType: 'QuotaExceeded',
			// `message` on the ApiError is the SERVER'S own message (via the
			// constructor's `serverMessage` arg). `errorMessage()` reads
			// `errorType` first and only falls back to this; the pin here
			// just confirms the operator-flavoured string survives the
			// hop in case the FE ever renders it (debug tools, etc).
			message: expect.stringContaining('Drive quota exceeded'),
			// Preserved so the batch orchestrator's "stop pulling work on a
			// full disk" short-circuit (`uploadAll` at
			// `files/[...path]/+page.svelte:756`) keeps working.
			isQuota: true
		});
	});

	it('surfaces a 403 with parsed errorType + server message', async () => {
		// Any non-507 error must also parse the normalized ApiErrorBody —
		// pre-fix every non-OK response collapsed into
		// `Error("upload failed: ${status}")` with no errorType, so UX
		// couldn't distinguish quota from permissions from quota-less
		// server errors. Pin on 403 so a regression that reverted the
		// parser to the generic Error path trips here too, not just on
		// quota.
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});
		const xhr = await waitForXhr(xhrs);
		xhr.respondJson(403, {
			status: '403 Forbidden',
			error: 'Create permission required on folder',
			message: 'Create permission required on folder',
			error_type: 'AccessDenied'
		});
		await expect(p).rejects.toMatchObject({
			name: 'ApiError',
			status: 403,
			errorType: 'AccessDenied',
			message: expect.stringContaining('Create permission'),
			isQuota: false
		});
	});

	it('throws ApiError with undefined errorType when the body is not JSON', async () => {
		// A proxied 503 page, a timeout gateway, or a buggy middleware can
		// return a non-JSON 5xx body. Pre-fix the parser crashed and the
		// upload reported "Upload failed" with no diagnostics. Shape-
		// tolerance guard: still throw an ApiError (so downstream
		// `errorMessage` can fall back to the status text), with
		// `errorType` undefined (no claim about the error_type we
		// couldn't parse).
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});
		const xhr = await waitForXhr(xhrs);
		xhr.responseText = '<html><body>502 Bad Gateway</body></html>';
		xhr.respond(502, { 'Content-Type': 'text/html' });
		await expect(p).rejects.toMatchObject({
			name: 'ApiError',
			status: 502,
			errorType: undefined,
			isQuota: false
		});
	});

	it('rejects on network error', async () => {
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});
		const xhr = await waitForXhr(xhrs);
		xhr.fireError();
		await expect(p).rejects.toThrow(/network error/);
	});

	it('retries once on a use_dpop_nonce challenge (fresh XHR)', async () => {
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});

		// First XHR — server sends the DPoP-Nonce challenge.
		const first = await waitForXhr(xhrs, 0);
		first.respond(401, {
			'WWW-Authenticate': 'DPoP error="use_dpop_nonce"',
			'DPoP-Nonce': 'srv-fresh'
		});

		// Retry mints a fresh XHR (the first one has already consumed
		// its body); it must also carry the DPoP header.
		const second = await waitForXhr(xhrs, 1);
		expect(second.requestHeaders.get('dpop')).toBe('proof.upload');
		second.respond(200);
		await expect(p).resolves.toBeUndefined();
		expect(xhrs).toHaveLength(2);
	});

	it('does not loop when the retry ALSO returns use_dpop_nonce', async () => {
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});

		const first = await waitForXhr(xhrs, 0);
		first.respond(401, {
			'WWW-Authenticate': 'DPoP error="use_dpop_nonce"',
			'DPoP-Nonce': 'srv-fresh'
		});
		const second = await waitForXhr(xhrs, 1);
		second.respond(401, {
			'WWW-Authenticate': 'DPoP error="use_dpop_nonce"',
			'DPoP-Nonce': 'srv-fresher'
		});

		// Pre-fix the wrapper threw `Error("upload failed: 401")`; post-fix
		// it throws an `ApiError` whose default message (no server body to
		// parse — the 401 is a bare DPoP challenge) follows the format
		// `API ${status} ${statusText} for ${resource}`. The point of this
		// pin is "the second 401 is NOT retried a third time" — assert
		// on the ApiError shape + status instead of the message string so
		// a future ApiError.message tweak doesn't break the retry pin.
		await expect(p).rejects.toMatchObject({ name: 'ApiError', status: 401 });
		expect(xhrs).toHaveLength(2); // never a third
	});
});
