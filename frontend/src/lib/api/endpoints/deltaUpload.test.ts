import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

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

import {
	DELTA_UPLOAD_MIN_SIZE,
	instantUploadOwned,
	resolveOwnedHashes,
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

	it('reports a quota error on a 507 done message', async () => {
		const p = tryDeltaUpload(bigFile(), 'folder');
		await Promise.resolve();
		lastWorker!.emit({ type: 'done', status: 507, body: { error: 'over quota' } });
		const res = await p;
		expect(res?.isQuotaError).toBe(true);
		expect(res?.errorMsg).toBe('over quota');
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

	it('surfaces a quota error on 507', async () => {
		byHashMock.mockResolvedValue({ ok: false, status: 507 });
		expect(await instantUploadOwned('folder', fakeFile(2 * MB), hashOf('a'))).toEqual({
			ok: false,
			isQuotaError: true,
			errorMsg: 'Storage quota exceeded'
		});
	});
});
