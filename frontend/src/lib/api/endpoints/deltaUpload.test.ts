import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { allowConsole } from '$lib/testing/allowConsole';

// This suite drives the delta path's refusals on purpose: HTTP 507 quota
// rejection, the worker asking for a no-wasm fallback, and the worker's
// `onerror`. All three log — see the console gate in `vitest-setup.ts`.
allowConsole('warn', 'delta quota rejection and worker fallback');
allowConsole('error', 'delta worker onerror, which disables delta for the tab');

vi.mock('$lib/api/csrf', () => ({ getCsrfToken: () => 'tok' }));

// vi.mock is hoisted; build the spies with vi.hoisted so the factories can use them.
const { blake3Mock, byHashMock, batchMock } = vi.hoisted(() => ({
	blake3Mock: vi.fn(),
	byHashMock: vi.fn(),
	batchMock: vi.fn()
}));

vi.mock('$lib/vendor/hashWasm', () => ({ blake3HexOfFile: blake3Mock }));
vi.mock('$lib/api/endpoints/files', () => ({
	createFileByHash: byHashMock,
	dedupCheckBatch: batchMock
}));

import { ApiError } from '$lib/api/client';
import {
	DELTA_UPLOAD_MIN_SIZE,
	instantUploadOwned,
	resolveOwnedHashes,
	throwFromDedupAnswer,
	tryDeltaUpload
} from './deltaUpload';

describe('tryDeltaUpload (delta worker)', () => {
	type Handler = ((e: { data: unknown }) => void) | null;
	let lastWorker: FakeWorker | null = null;

	class FakeWorker {
		onmessage: Handler = null;
		onerror: (() => void) | null = null;
		posted: unknown[] = [];
		terminated = false;
		constructor() {
			// Capture the instance the code-under-test creates so the test can drive its events.
			// eslint-disable-next-line @typescript-eslint/no-this-alias
			lastWorker = this;
		}
		postMessage(msg: unknown) {
			this.posted.push(msg);
		}
		terminate() {
			this.terminated = true;
		}
		emit(data: unknown) {
			this.onmessage?.({ data });
		}
	}

	function bigFile(): File {
		const f = new File(['x'], 'big.bin');
		Object.defineProperty(f, 'size', { value: DELTA_UPLOAD_MIN_SIZE + 1 });
		return f;
	}

	beforeEach(() => {
		lastWorker = null;
		vi.stubGlobal('Worker', FakeWorker as unknown as typeof Worker);
	});
	afterEach(() => vi.unstubAllGlobals());

	it('skips delta for a small file', async () => {
		const small = new File(['x'], 's.txt');
		expect(await tryDeltaUpload(small, 'folder')).toBeNull();
	});

	it('skips delta when there is no folder', async () => {
		expect(await tryDeltaUpload(bigFile(), null)).toBeNull();
	});

	it('resolves ok on a 201 done message and reports progress', async () => {
		const onProgress = vi.fn();
		const p = tryDeltaUpload(bigFile(), 'folder', onProgress);
		await Promise.resolve();
		expect(lastWorker).toBeTruthy();
		lastWorker!.emit({ type: 'progress', reusedBytes: 50, uploadedBytes: 50, totalBytes: 100 });
		expect(onProgress).toHaveBeenCalledWith(99);
		lastWorker!.emit({ type: 'done', status: 201, body: { message: 'ok' } });
		const res = await p;
		expect(res?.ok).toBe(true);
		expect(res?.savedBytes).toBe(50);
		expect(lastWorker!.terminated).toBe(true);
	});

	it('falls back to null on a fallback message', async () => {
		const p = tryDeltaUpload(bigFile(), 'folder');
		await Promise.resolve();
		lastWorker!.emit({ type: 'fallback', reason: 'no wasm' });
		expect(await p).toBeNull();
	});

	it('reports a quota error on a legacy 507 done message (no error_type)', async () => {
		// Legacy body — only `error`, no `error_type`. The worker-msg path
		// must still surface a quota answer so the orchestrator renders
		// SOMETHING on the toast; `errorType` is undefined so the
		// orchestrator's `ApiError` will fall through to the raw server
		// message (acceptable fallback). This arm covers deployments mid-
		// migration where the server hasn't yet shipped normalized bodies.
		const p = tryDeltaUpload(bigFile(), 'folder');
		await Promise.resolve();
		lastWorker!.emit({ type: 'done', status: 507, body: { error: 'over quota' } });
		const res = await p;
		expect(res?.isQuotaError).toBe(true);
		expect(res?.errorMsg).toBe('over quota');
		expect(res?.errorType).toBeUndefined();
		expect(res?.status).toBe(507);
	});

	it('threads error_type on a normalized 507 done message', async () => {
		// Post-normalization the server emits the full `ApiErrorBody` and
		// the worker forwards it verbatim as `msg.body`. The delta path
		// must read `error_type` (just like `instantUploadOwned` reads
		// `errorBody.error_type` from `createFileByHash`) so the thrown
		// `ApiError` carries `errorType: 'QuotaExceeded'` and the toast
		// lands the friendly i18n copy ("Espace insuffisant…") instead
		// of the server's byte-arithmetic `message`. A regression that
		// drops the `msg.body.error_type` read trips here.
		const p = tryDeltaUpload(bigFile(), 'folder');
		await Promise.resolve();
		lastWorker!.emit({
			type: 'done',
			status: 507,
			body: {
				status: '507 Insufficient Storage',
				error: 'Drive quota exceeded: 420192487 + 427741 > 314572800 bytes',
				message: 'Drive quota exceeded: 420192487 + 427741 > 314572800 bytes',
				error_type: 'QuotaExceeded'
			} as unknown as { message?: string; error?: string; still_missing?: unknown }
		});
		const res = await p;
		expect(res?.isQuotaError).toBe(true);
		expect(res?.errorType).toBe('QuotaExceeded');
		expect(res?.errorMsg).toContain('Drive quota exceeded');
		expect(res?.status).toBe(507);
	});

	it('resolves null on a worker error', async () => {
		const p = tryDeltaUpload(bigFile(), 'folder');
		await Promise.resolve();
		lastWorker!.onerror?.();
		expect(await p).toBeNull();
	});
});

const fakeFile = (size: number, name = 'x.bin') => ({ size, name }) as unknown as File;
const hashOf = (name: string) => name.padEnd(64, '0');
const MB = 1024 * 1024;

describe('resolveOwnedHashes (batch check)', () => {
	beforeEach(() => {
		blake3Mock.mockReset();
		batchMock.mockReset();
		blake3Mock.mockImplementation((f: File) => Promise.resolve(hashOf(f.name)));
	});

	it('hits nothing when no files are in-band (empty / >= delta threshold)', async () => {
		const owned = await resolveOwnedHashes([
			fakeFile(0, 'empty'),
			fakeFile(DELTA_UPLOAD_MIN_SIZE, 'big')
		]);
		expect(owned.size).toBe(0);
		expect(blake3Mock).not.toHaveBeenCalled();
		expect(batchMock).not.toHaveBeenCalled();
	});

	it('hashes in-band files and maps only the server-owned subset in ONE batch call', async () => {
		const a = fakeFile(2 * MB, 'a');
		const b = fakeFile(3 * MB, 'b');
		batchMock.mockResolvedValue(new Set([hashOf('a')])); // server owns only "a"
		const owned = await resolveOwnedHashes([a, b]);
		expect(batchMock).toHaveBeenCalledTimes(1);
		expect(batchMock).toHaveBeenCalledWith([hashOf('a'), hashOf('b')]);
		expect(owned.get(a)).toBe(hashOf('a'));
		expect(owned.has(b)).toBe(false);
	});

	it('falls back to an empty map when client-side hashing fails', async () => {
		blake3Mock.mockRejectedValue(new Error('wasm down'));
		const owned = await resolveOwnedHashes([fakeFile(2 * MB, 'a')]);
		expect(owned.size).toBe(0);
		expect(batchMock).not.toHaveBeenCalled();
	});

	it('falls back to an empty map when the batch request fails', async () => {
		batchMock.mockRejectedValue(new Error('network'));
		const owned = await resolveOwnedHashes([fakeFile(2 * MB, 'a')]);
		expect(owned.size).toBe(0);
	});
});

describe('instantUploadOwned (zero-byte create)', () => {
	beforeEach(() => byHashMock.mockReset());

	it('reports zero-byte success on 201', async () => {
		byHashMock.mockResolvedValue({ ok: true, status: 201, data: { id: 'f1' } });
		const r = await instantUploadOwned('folder', fakeFile(2 * MB, 'a'), hashOf('a'));
		expect(r).toEqual({ ok: true, data: { id: 'f1' }, savedBytes: 2 * MB });
		// 4th arg is §9's source mtime, forwarded from `file.lastModified`.
		// `fakeFile` does not set one, so it arrives as undefined — which is
		// the correct behaviour: `createFileByHash` omits the field entirely
		// rather than sending a falsy value the server would reject as
		// pre-2000.
		expect(byHashMock).toHaveBeenCalledWith('folder', 'a', hashOf('a'), undefined);
	});

	it('falls back (null) when the blob vanished (404)', async () => {
		byHashMock.mockResolvedValue({ ok: false, status: 404 });
		expect(await instantUploadOwned('folder', fakeFile(2 * MB), hashOf('a'))).toBeNull();
	});

	it('surfaces a quota error on 507 with the server message + errorType', async () => {
		// Server ships normalized ApiErrorBody on 507; `instantUploadOwned`
		// must thread `error_type` and the real `message` through the
		// DeltaUploadAnswer so the upload orchestrator can construct an
		// `ApiError` whose `errorMessage()` lookup lands the friendly i18n
		// copy ("Espace insuffisant…") instead of the hardcoded English
		// string that was in `errorMsg` pre-fix. Pin against the exact
		// server shape — if `createFileByHash` ever stops returning
		// `errorBody`, or `instantUploadOwned` drops the error_type /
		// server message read, the toast silently reverts to the raw
		// English string and this trips.
		byHashMock.mockResolvedValue({
			ok: false,
			status: 507,
			errorBody: {
				status: '507 Insufficient Storage',
				error: 'Drive quota exceeded: 420192487 + 427741 > 314572800 bytes',
				message: 'Drive quota exceeded: 420192487 + 427741 > 314572800 bytes',
				error_type: 'QuotaExceeded'
			}
		});
		expect(await instantUploadOwned('folder', fakeFile(2 * MB), hashOf('a'))).toEqual({
			ok: false,
			isQuotaError: true,
			errorMsg: 'Drive quota exceeded: 420192487 + 427741 > 314572800 bytes',
			errorType: 'QuotaExceeded',
			status: 507
		});
	});

	it('falls back to a hardcoded message when the 507 body is missing', async () => {
		// Shape-tolerance guard: a 507 that arrived with no body (misconfigured
		// proxy, legacy server pre-normalization) still surfaces as an
		// `isQuotaError` answer so the orchestrator renders a non-empty
		// toast — just without the friendly i18n copy (errorType undefined).
		byHashMock.mockResolvedValue({ ok: false, status: 507, errorBody: undefined });
		expect(await instantUploadOwned('folder', fakeFile(2 * MB), hashOf('a'))).toEqual({
			ok: false,
			isQuotaError: true,
			errorMsg: 'Storage quota exceeded',
			errorType: undefined,
			status: 507
		});
	});
});

describe('throwFromDedupAnswer', () => {
	// `uploadOneFile` in `+page.svelte` is a Svelte-component method, hard
	// to isolate without mounting the component. The wrap-DeltaUploadAnswer-
	// to-ApiError logic was extracted into this util so a plain Vitest can
	// pin the glue that connects `instantUploadOwned` / `tryDeltaUpload`
	// (which hand back a `DeltaUploadAnswer`) to `lib/utils/errors.ts`'s
	// `errorMessage()` lookup (which keys the friendly i18n copy off
	// `errorType`). A regression that drops `answer.errorType` or
	// `answer.errorMsg` from the ApiError construction trips here.

	it('throws an ApiError carrying the server-side errorType + message on QuotaExceeded', () => {
		expect(() =>
			throwFromDedupAnswer(
				{
					ok: false,
					isQuotaError: true,
					errorMsg: 'Drive quota exceeded: 420192487 + 427741 > 314572800 bytes',
					errorType: 'QuotaExceeded',
					status: 507
				},
				'/api/files/by-hash'
			)
		).toThrow(ApiError);
		// Separate call so we can inspect the thrown value — Vitest's
		// toThrow receives either the class or a matcher, not both.
		try {
			throwFromDedupAnswer(
				{
					ok: false,
					isQuotaError: true,
					errorMsg: 'Drive quota exceeded: …',
					errorType: 'QuotaExceeded',
					status: 507
				},
				'/api/files/by-hash'
			);
		} catch (e) {
			const err = e as ApiError & { isQuota: boolean };
			expect(err.name).toBe('ApiError');
			expect(err.status).toBe(507);
			expect(err.errorType).toBe('QuotaExceeded');
			expect(err.message).toBe('Drive quota exceeded: …');
			expect(err.isQuota).toBe(true);
			expect(err.resource).toBe('/api/files/by-hash');
		}
	});

	it('defaults status to 507 when the DeltaUploadAnswer carries none', () => {
		// Pre-fix the inline wrap in `+page.svelte` used `new Error(…)` and
		// had no status at all; the extraction preserves the historical
		// "dedup failures without a status are 507" invariant (the only
		// non-fallback status the dedup paths emitted). A regression that
		// changes the default to 500 (or 400) would mis-route the toast
		// through non-quota branches and break the batch `isQuota` short-
		// circuit — pin so the default stays wire-correct.
		try {
			throwFromDedupAnswer(
				{ ok: false, errorMsg: 'something', errorType: 'InternalError' },
				'/api/files/by-hash'
			);
		} catch (e) {
			const err = e as ApiError & { isQuota: boolean };
			expect(err.status).toBe(507);
			expect(err.isQuota).toBe(false);
		}
	});

	it('propagates undefined errorType — the orchestrator must still throw', () => {
		// Shape-tolerance guard: `tryDeltaUpload` on a legacy pre-normalization
		// server returns `{ok:false, errorMsg, errorType: undefined}`.
		// `throwFromDedupAnswer` must still throw an ApiError (so the toast
		// renders), with `errorType` undefined so `errorMessage()` falls
		// through to the server's own message rather than any default copy.
		try {
			throwFromDedupAnswer(
				{ ok: false, isQuotaError: true, errorMsg: 'over quota', status: 507 },
				'/api/files/by-hash'
			);
		} catch (e) {
			const err = e as ApiError & { isQuota: boolean };
			expect(err.errorType).toBeUndefined();
			expect(err.message).toBe('over quota');
			expect(err.isQuota).toBe(true);
		}
	});
});
