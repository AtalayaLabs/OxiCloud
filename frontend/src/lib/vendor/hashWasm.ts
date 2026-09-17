/**
 * Lazy loader + minimal typing for the vendored OxiCloud hash WASM
 * (`/vendors/hash-wasm/oxicloud_hash_wasm.js`) — the same BLAKE3 crate the
 * server and the delta worker use. Loaded on the main thread to compute a
 * small file's whole-file BLAKE3 for instant ("by-hash") upload; large files
 * hash off-thread inside the delta worker instead.
 */
import { base } from '$app/paths';

interface HashWasmModule {
	/** wasm-bindgen init; resolves once the `.wasm` is instantiated. */
	default: () => Promise<unknown>;
	/** One-shot BLAKE3 of a buffer → 64-char lowercase hex. */
	blake3Hex: (data: Uint8Array) => string;
}

const WASM_GLUE_URL = `${base}/vendors/hash-wasm/oxicloud_hash_wasm.js`;

let modPromise: Promise<HashWasmModule> | null = null;

function load(): Promise<HashWasmModule> {
	if (!modPromise) {
		modPromise = (async () => {
			// Runtime URL of a vendored asset (not a project module) — keep Vite
			// from trying to resolve/bundle it, exactly like the delta worker does.
			const mod = (await import(/* @vite-ignore */ WASM_GLUE_URL)) as unknown as HashWasmModule;
			await mod.default();
			return mod;
		})().catch((err) => {
			modPromise = null; // let a later call retry after a transient failure
			throw err;
		});
	}
	return modPromise;
}

/**
 * Whole-file BLAKE3 (64-char lowercase hex) of `file`. Matches the server's
 * `file_hash` (BLAKE3 over the whole content), so it can be handed to
 * `/api/files/by-hash`. Reads the file fully into memory — intended for small
 * files only.
 */
export async function blake3HexOfFile(file: File): Promise<string> {
	const mod = await load();
	const bytes = new Uint8Array(await file.arrayBuffer());
	return mod.blake3Hex(bytes);
}
