import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), apiJson: vi.fn() }));
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
		this.requestHeaders.set(k, v);
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
	beforeEach(() => {
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
		expect(xhr.url).toBe('/api/files/upload');
		expect(xhr.requestHeaders.get('DPoP')).toBe('proof.upload');
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
		expect(xhr.requestHeaders.get('DPoP')).toBeUndefined();
		xhr.respond(200);
		await p;
	});

	it('surfaces a 507 with isQuota flag for the batch orchestrator', async () => {
		const file = new File([new Uint8Array([1])], 'a.txt');
		const p = uploadFileWithProgress(null, file, () => {});
		const xhr = await waitForXhr(xhrs);
		xhr.respond(507);
		await expect(p).rejects.toMatchObject({
			isQuota: true,
			message: expect.stringContaining('507')
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
		expect(second.requestHeaders.get('DPoP')).toBe('proof.upload');
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

		await expect(p).rejects.toThrow(/dpop_nonce_challenge/);
		expect(xhrs).toHaveLength(2); // never a third
	});
});
