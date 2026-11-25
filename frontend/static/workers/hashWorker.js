/**
 * OxiCloud — whole-file BLAKE3 hashing worker.
 *
 * Computes the instant-upload ("does the server already own this?") hashes
 * OFF the main thread. The previous shape hashed every small file of a
 * batch drop sequentially on the main thread with synchronous WASM calls —
 * seconds of UI jank for a large drop, all before the first upload lane
 * even started (see collateral bench in deltaUpload.hash.test.ts).
 *
 * Protocol with the spawner (one worker handles many requests):
 *   in  : { id: number, file: File }
 *   out : { id: number, hex: string }        — success
 *         { id: number, error: string }      — this file failed (caller
 *                                              falls back to plain upload)
 */

// Served at `{base}/workers/hashWorker.js` — derive the deployment base
// path from our own URL so the WASM import works under subpath deployments.
const BASE = self.location.pathname.replace(/\/workers\/[^/]*$/, '');
const WASM_GLUE_URL = `${BASE}/vendors/hash-wasm/oxicloud_hash_wasm.js`;

let modPromise = null;
function load() {
	if (!modPromise) {
		modPromise = import(WASM_GLUE_URL).then(async (mod) => {
			await mod.default();
			return mod;
		});
	}
	return modPromise;
}

const CHUNK_SIZE = 8 * 1024 * 1024;

self.onmessage = async (ev) => {
	const { id, file } = ev.data;
	try {
		const mod = await load();
		let hex;
		if (file.size <= CHUNK_SIZE) {
			const bytes = new Uint8Array(await file.arrayBuffer());
			hex = mod.blake3Hex(bytes);
		} else {
			const hasher = new mod.Blake3Hasher();
			try {
				for (let offset = 0; offset < file.size; offset += CHUNK_SIZE) {
					const slice = file.slice(offset, Math.min(file.size, offset + CHUNK_SIZE));
					const bytes = new Uint8Array(await slice.arrayBuffer());
					hasher.update(bytes);
				}
				hex = hasher.finalizeHex();
			} finally {
				hasher.free();
			}
		}
		self.postMessage({ id, hex });
	} catch (err) {
		self.postMessage({ id, error: String(err) });
	}
};
