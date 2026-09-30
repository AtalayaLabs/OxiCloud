// @vitest-environment node
import { File } from 'node:buffer';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { describe, expect, it } from 'vitest';

const workerSource = readFileSync('static/workers/deltaWorker.js', 'utf8');

interface WorkerMessage {
	type: string;
	status?: number;
	reason?: string;
	uploadedBytes?: number;
}

interface Scenario {
	sizes?: number[];
	limit?: number;
	initialMissing?: boolean;
	recovery?: string[];
	putStatus?: number;
	repeatConflict?: boolean;
}

/** Run the shipped worker, substituting only WASM loading and the network.
 * Frame encoding, batching, recovery and progress all execute real code. */
async function runWorker({
	sizes = [4, 4, 4, 4, 4],
	limit = 16,
	initialMissing = false,
	recovery,
	putStatus = 200,
	repeatConflict = false
}: Scenario = {}) {
	const chunks = sizes.map((size, i) => [`hash-${i}`, size] as const);
	const contents = sizes.map((size, i) => new Uint8Array(size).fill(i + 1));
	const file = new File(contents, 'recovery.bin');
	const messages: WorkerMessage[] = [];
	const puts: { url: string; frames: number[][]; bytes: number; headers: HeadersInit }[] = [];
	const requests: string[] = [];
	let commits = 0;
	const scope: {
		location: { pathname: string };
		postMessage: (message: WorkerMessage) => void;
		onmessage?: (event: { data: unknown }) => Promise<void>;
	} = {
		location: { pathname: '/cloud/workers/deltaWorker.js' },
		postMessage: (message) => messages.push(message)
	};

	// Node's VM does not resolve the browser's absolute WASM import. Replace
	// that one expression, retaining the real loadWasm() initialization path.
	const importExpression = 'await import(WASM_GLUE_URL)';
	expect(workerSource.split(importExpression)).toHaveLength(2);
	runInNewContext(workerSource.replace(importExpression, 'await loadTestWasm(WASM_GLUE_URL)'), {
		self: scope,
		setInterval,
		clearInterval,
		loadTestWasm: async (url: string) => {
			expect(url).toBe('/cloud/vendors/hash-wasm/oxicloud_hash_wasm.js');
			return {
				default: async () => {},
				DeltaChunker: class {
					update() {
						return '[]';
					}
					finish() {
						return JSON.stringify({ chunks, file_hash: 'whole-file-hash' });
					}
					free() {}
				}
			};
		},
		fetch: async (url: string, init: RequestInit) => {
			requests.push(url);
			if (url.endsWith('/negotiate')) {
				return Response.json({ missing: initialMissing ? chunks.map(([h]) => h) : [] });
			}
			if (url.endsWith('/chunks')) {
				const wire = init.body as Uint8Array;
				const view = new DataView(wire.buffer, wire.byteOffset, wire.byteLength);
				const frames: number[][] = [];
				for (let offset = 0; offset < wire.byteLength; ) {
					const length = view.getUint32(offset, false);
					offset += 4;
					expect(offset + length).toBeLessThanOrEqual(wire.byteLength);
					frames.push(Array.from(wire.subarray(offset, offset + length)));
					offset += length;
				}
				puts.push({ url, frames, bytes: wire.byteLength, headers: init.headers ?? {} });
				return new Response(null, { status: putStatus });
			}
			if (url.endsWith('/commit')) {
				commits++;
				if ((!initialMissing && commits === 1) || repeatConflict) {
					return Response.json(
						{ still_missing: recovery ?? chunks.map(([h]) => h) },
						{ status: 409 }
					);
				}
				return Response.json({}, { status: 201 });
			}
			throw new Error(`Unexpected request: ${url}`);
		}
	});
	await scope.onmessage!({
		data: { file, folderId: 'folder', name: file.name, csrfToken: 'csrf', uploadBatchBytes: limit }
	});
	return { puts, messages, commits, requests, contents };
}

describe('delta worker request bounds', () => {
	it.each([true, false])(
		'accounts for frame headers during initialMissing=%s uploads',
		async (initialMissing) => {
			const { puts, messages, contents, commits, requests } = await runWorker({ initialMissing });
			expect(puts.map((p) => p.bytes)).toEqual([16, 16, 8]);
			expect(puts.flatMap((p) => p.frames)).toEqual(contents.map((c) => Array.from(c)));
			expect(puts.every((p) => p.url === '/cloud/api/files/delta/chunks')).toBe(true);
			expect(puts.every((p) => new Headers(p.headers).get('X-CSRF-Token') === 'csrf')).toBe(true);
			expect(messages.at(-1)).toMatchObject({ type: 'done', status: 201, uploadedBytes: 20 });
			expect(commits).toBe(initialMissing ? 1 : 2);
			expect(requests.at(-1)).toBe('/cloud/api/files/delta/commit');
		}
	);

	it('recovers only requested chunks, once each, in bounded requests', async () => {
		const { puts, messages } = await runWorker({
			recovery: ['hash-4', 'hash-0', 'hash-4', 'hash-2']
		});
		expect(puts.map((p) => p.bytes)).toEqual([16, 8]);
		expect(puts.flatMap((p) => p.frames)).toEqual([
			[5, 5, 5, 5],
			[1, 1, 1, 1],
			[3, 3, 3, 3]
		]);
		expect(messages.at(-1)).toMatchObject({ type: 'done', status: 201, uploadedBytes: 12 });
	});

	it.each([true, false])(
		'sends an oversized indivisible chunk alone (%s)',
		async (initialMissing) => {
			const { puts, messages } = await runWorker({ sizes: [20, 4, 4], initialMissing });
			expect(puts.map((p) => p.bytes)).toEqual([24, 16]);
			expect(puts[0].frames).toHaveLength(1);
			expect(messages.at(-1)).toMatchObject({ type: 'done', status: 201 });
		}
	);

	it('stops recovery when a PUT is rejected, without recommitting', async () => {
		const { puts, messages, commits } = await runWorker({ putStatus: 413 });
		expect(puts).toHaveLength(1);
		expect(commits).toBe(1);
		expect(messages.at(-1)).toEqual({
			type: 'fallback',
			reason: 'retry chunk PUT failed (HTTP 413)'
		});
	});

	it('validates every requested hash before sending recovery bytes', async () => {
		const { puts, messages, commits } = await runWorker({ recovery: ['hash-0', 'unknown'] });
		expect(puts).toHaveLength(0);
		expect(commits).toBe(1);
		expect(messages.at(-1)).toMatchObject({
			type: 'fallback',
			reason: 'server requested an unknown chunk'
		});
	});

	it('keeps the existing two-recovery-attempt limit', async () => {
		const { puts, messages, commits } = await runWorker({ repeatConflict: true });
		expect(commits).toBe(3);
		expect(puts.map((p) => p.bytes)).toEqual([16, 16, 8, 16, 16, 8]);
		expect(messages.at(-1)).toMatchObject({ type: 'done', status: 409 });
	});

	it('does not PUT an empty recovery request', async () => {
		const { puts, messages, commits } = await runWorker({ recovery: [] });
		expect(puts).toHaveLength(0);
		expect(commits).toBe(2);
		expect(messages.at(-1)).toMatchObject({ type: 'done', status: 201 });
	});

	it('uses the default budget for a non-finite override', async () => {
		const { puts } = await runWorker({
			sizes: [4 * 1024 * 1024, 4 * 1024 * 1024],
			limit: Infinity
		});
		expect(puts.map((p) => p.bytes)).toEqual([4 * 1024 * 1024 + 4, 4 * 1024 * 1024 + 4]);
	});
});
