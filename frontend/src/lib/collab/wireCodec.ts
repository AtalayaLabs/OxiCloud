/**
 * Wire codec for the collab binary-frame protocol.
 *
 * The FE mirror of `src/application/services/collab_wire.rs`. Frame
 * layout on the WebSocket:
 *
 *     [1 byte kind][16 bytes file_id BE][payload…]
 *
 * `file_id` is a UUID serialised as its 16 raw bytes (network order,
 * matching the server's `Uuid::as_bytes()`). Payload semantics are
 * kind-specific:
 *
 *   * `UPDATE`    — a Yjs update blob (opaque; `Y.encodeStateAsUpdate` /
 *                    `Y.applyUpdate` bytes).
 *   * `AWARENESS` — a Yjs awareness message (opaque; presence-only,
 *                    not persisted).
 *   * `SYNC`      — a Yjs state vector (client → server, sync-step-1)
 *                    or a Yjs update blob (server → client,
 *                    sync-step-2). Direction is inferred from context.
 *
 * Kept pure — no WebSocket, no reactivity. Wire tests below are the
 * only assertion surface; the transport is
 * `$lib/message-bus/client.svelte.ts`.
 */

export const KIND_UPDATE = 0x01;
export const KIND_AWARENESS = 0x02;
export const KIND_SYNC = 0x03;

export type Kind = typeof KIND_UPDATE | typeof KIND_AWARENESS | typeof KIND_SYNC;

export interface BinaryFrame {
	kind: Kind;
	fileId: string; // canonical dashed UUID
	payload: Uint8Array;
}

/** Parse a dashed UUID into its 16 raw bytes. Throws on malformed
 *  input. Same shape as the server-side helper in
 *  `src/bin/rt-hurl-helper.rs::parse_uuid_bytes`. */
export function uuidToBytes(uuid: string): Uint8Array {
	const hex = uuid.replace(/-/g, '');
	if (hex.length !== 32) throw new Error(`bad uuid: ${uuid}`);
	const out = new Uint8Array(16);
	for (let i = 0; i < 16; i++) {
		const byte = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
		if (Number.isNaN(byte)) throw new Error(`bad uuid hex: ${uuid}`);
		out[i] = byte;
	}
	return out;
}

/** Format 16 raw UUID bytes as canonical dashed hex — inverse of
 *  `uuidToBytes`. Never throws (any byte input yields a well-formed
 *  string; caller ensures length 16 upstream). */
export function bytesToUuid(bytes: Uint8Array): string {
	if (bytes.length !== 16) throw new Error(`bad uuid bytes: len=${bytes.length}`);
	let out = '';
	for (let i = 0; i < 16; i++) {
		if (i === 4 || i === 6 || i === 8 || i === 10) out += '-';
		out += bytes[i].toString(16).padStart(2, '0');
	}
	return out;
}

/** Assemble a binary frame ready for `WebSocket.send`. */
export function encodeFrame(kind: Kind, fileId: string, payload: Uint8Array): Uint8Array {
	const idBytes = uuidToBytes(fileId);
	const out = new Uint8Array(17 + payload.length);
	out[0] = kind;
	out.set(idBytes, 1);
	out.set(payload, 17);
	return out;
}

/** Split an incoming binary frame from `WebSocket.onmessage`.
 *  Returns `null` for frames that don't match the collab shape (too
 *  short, unknown kind) — the caller decides whether that's a
 *  protocol violation or just "not for me".
 *
 *  `data` may be `ArrayBuffer` (Node) OR `Uint8Array` (browser
 *  `binaryType: 'arraybuffer'` produces the former; some polyfills
 *  produce the latter). Normalise upstream. */
export function decodeFrame(data: Uint8Array): BinaryFrame | null {
	if (data.length < 17) return null;
	const kind = data[0];
	if (kind !== KIND_UPDATE && kind !== KIND_AWARENESS && kind !== KIND_SYNC) return null;
	const fileId = bytesToUuid(data.subarray(1, 17));
	const payload = data.subarray(17);
	return { kind: kind as Kind, fileId, payload };
}
