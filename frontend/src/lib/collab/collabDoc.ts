/**
 * Per-file collaborative-doc handle.
 *
 * Owns one `Y.Doc`, wires it to the message-bus WS:
 *
 *   1. `rt.subscribe collab:{fileId}` — clears the server-side
 *      `Permission::Read` gate; without this the server doesn't install
 *      a broadcast forwarder for this socket and no fan-out reaches us.
 *   2. Register a binary-frame handler keyed on `fileId` — every
 *      inbound `[kind][file_id][payload]` frame with our id lands here.
 *   3. Sync-step-1 handshake on connect — send `0x03 SYNC` with the
 *      Doc's current state vector; server replies with `0x03 SYNC`
 *      carrying the missing updates, we `Y.applyUpdate`. `Y.Doc`
 *      converges even if the reply arrives before the outbound update
 *      — CRDT semantics are idempotent.
 *   4. Local updates — every `Y.Doc.on('update')` fires an `0x01 UPDATE`
 *      out with the incremental bytes. The server broadcasts to other
 *      subscribers AND flushes to the file's blob via the debouncer.
 *   5. Explicit save — `flush()` sends `rt.collab_flush {file_id}` for
 *      the "on tab close / on save" path.
 *
 * Awareness (presence cursors) is out of scope for the simple version —
 * `y-protocols/awareness` slots in as C6.
 *
 * This class is transport + lifecycle only. Editor binding
 * (`y-codemirror.next`) mounts against `yText()` in a Svelte component.
 */

import log from 'loglevel';
import * as Y from 'yjs';

import { messageBus } from '$lib/message-bus/client.svelte';
import type { UnsubscribeHandle } from '$lib/message-bus/client.svelte';
import { KIND_SYNC, KIND_UPDATE, decodeFrame, encodeFrame } from './wireCodec';

const collabLog = log.getLogger('oxi:collab');

/** The Y.Doc root-type name the server uses for the file's text.
 *  Must match `ROOT_TEXT_NAME` in
 *  `src/application/services/collab_session_service.rs`. */
export const ROOT_TEXT_NAME = 'content';

export type SyncState = 'idle' | 'syncing' | 'synced' | 'disconnected';

export interface CollabDocOpts {
	fileId: string;
	/** Called whenever `syncState` changes so the UI can render a
	 *  status pill without wiring a Svelte store from inside a plain
	 *  TS class. */
	onSyncStateChange?: (state: SyncState) => void;
}

export class CollabDoc {
	readonly fileId: string;
	readonly doc: Y.Doc;

	#syncState: SyncState = 'idle';
	#onSyncStateChange?: (state: SyncState) => void;
	#unsubscribeTopic: UnsubscribeHandle | null = null;
	#unregisterHandler: (() => void) | null = null;
	#docUpdateHandler: ((update: Uint8Array, origin: unknown) => void) | null = null;
	#destroyed = false;

	constructor(opts: CollabDocOpts) {
		this.fileId = opts.fileId;
		this.#onSyncStateChange = opts.onSyncStateChange;
		this.doc = new Y.Doc();
	}

	/** Kick off the connection. Safe to call once per instance; a
	 *  second call is a no-op. Errors are logged; the syncState
	 *  transitions reflect what actually happened. */
	async connect(): Promise<void> {
		if (this.#destroyed) throw new Error('CollabDoc destroyed');
		if (this.#unsubscribeTopic) return; // already connected

		this.#setSyncState('syncing');

		// Register the binary handler BEFORE subscribing — sync-step-2
		// races with the subscribe ack, so we must be ready to receive.
		this.#unregisterHandler = messageBus.registerBinaryHandler(this.fileId, (bytes) =>
			this.#onBinary(bytes)
		);

		// Subscribe — the message-bus client handles the reconnect
		// replay, so a socket drop + reopen re-does this without our
		// help. `subscribe` clears the AuthZ gate (Read on the file);
		// the handler here is empty because collab publishes on the
		// bus are the BINARY plane, not `rt.event` text notifications.
		this.#unsubscribeTopic = messageBus.subscribe(
			`collab:${this.fileId}`,
			() => {
				// No-op: collab events are binary, not text. This handler
				// only fires for JSON `rt.event` frames on this topic —
				// which the server doesn't publish for collab. Kept as
				// a defensive log so a future producer that DOES send
				// text on this topic is discoverable.
				collabLog.debug('unexpected text event on collab topic', { fileId: this.fileId });
			},
			(revoked) => {
				// Server evicted us — grant revoked, file deleted, etc.
				collabLog.warn('collab subscription revoked', {
					fileId: this.fileId,
					reason: revoked.reason
				});
				this.#setSyncState('disconnected');
			}
		);

		// Local updates → wire. Filter by origin so the update we
		// apply from a peer (origin === this) doesn't echo back.
		this.#docUpdateHandler = (update, origin) => {
			if (origin === this) return; // remote-applied, don't reflect
			const frame = encodeFrame(KIND_UPDATE, this.fileId, update);
			messageBus.sendBinary(frame);
		};
		this.doc.on('update', this.#docUpdateHandler);

		// Sync-step-1: send our state vector, server replies with the
		// missing updates. Send immediately — the WS is already
		// connected (subscribe kicked it) OR will be shortly, and the
		// bus client's #onOpen replays the subscribe on reconnect but
		// NOT this binary frame. We rely on the server's actor to
		// re-broadcast state to a reconnected client via any future
		// UPDATE it applies; a proper reconnect handshake with
		// on-reconnect resync is a follow-up polish slice.
		const sv = Y.encodeStateVector(this.doc);
		const frame = encodeFrame(KIND_SYNC, this.fileId, sv);
		messageBus.sendBinary(frame);
	}

	/** Dispose. Idempotent; safe from Svelte $effect cleanup. */
	destroy(): void {
		if (this.#destroyed) return;
		this.#destroyed = true;
		if (this.#docUpdateHandler) {
			this.doc.off('update', this.#docUpdateHandler);
			this.#docUpdateHandler = null;
		}
		if (this.#unregisterHandler) {
			this.#unregisterHandler();
			this.#unregisterHandler = null;
		}
		if (this.#unsubscribeTopic) {
			this.#unsubscribeTopic();
			this.#unsubscribeTopic = null;
		}
		this.doc.destroy();
	}

	/** The `Y.Text` fragment the CodeMirror binding attaches to. */
	yText(): Y.Text {
		return this.doc.getText(ROOT_TEXT_NAME);
	}

	/** Reactive-friendly getter for the current sync state. */
	syncState(): SyncState {
		return this.#syncState;
	}

	#setSyncState(next: SyncState): void {
		if (next === this.#syncState) return;
		this.#syncState = next;
		this.#onSyncStateChange?.(next);
	}

	#onBinary(bytes: Uint8Array): void {
		const frame = decodeFrame(bytes);
		if (!frame) {
			collabLog.warn('unparseable binary frame', { len: bytes.length });
			return;
		}
		if (frame.fileId !== this.fileId) return; // not for us (belt-and-braces)
		switch (frame.kind) {
			case KIND_UPDATE:
			case KIND_SYNC: {
				// Both kinds carry Yjs update bytes on the s→c
				// direction; apply idempotently. Origin `this` marks
				// the update as "remote-applied" so `#docUpdateHandler`
				// doesn't reflect it back to the wire.
				try {
					Y.applyUpdate(this.doc, frame.payload, this);
					this.#setSyncState('synced');
				} catch (err) {
					collabLog.warn('applyUpdate failed', { fileId: this.fileId, error: err });
				}
				break;
			}
			// AWARENESS not handled in the simple version.
		}
	}
}
