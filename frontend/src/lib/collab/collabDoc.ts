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
import * as awarenessProtocol from 'y-protocols/awareness';

import { messageBus } from '$lib/message-bus/client.svelte';
import type { UnsubscribeHandle } from '$lib/message-bus/client.svelte';
import { KIND_AWARENESS, KIND_SYNC, KIND_UPDATE, decodeFrame, encodeFrame } from './wireCodec';

/** Caller's effective capabilities on this file, resolved by the
 *  server on the `rt.subscribe collab:<id>` ack. `canWrite` reflects
 *  `Permission::Update` at subscribe time; a mid-session grant
 *  demotion doesn't refresh this until the client reconnects (the
 *  matching server-side eviction is a follow-up slice — see
 *  memory `project_collab_authz_eviction_pending`). */
export interface CollabCapabilities {
	canWrite: boolean;
}

const collabLog = log.getLogger('oxi:collab');

/** The Y.Doc root-type name the server uses for the file's text.
 *  Must match `ROOT_TEXT_NAME` in
 *  `src/application/services/collab_session_service.rs`. */
export const ROOT_TEXT_NAME = 'content';

/** UI-visible lifecycle state of a `CollabDoc`.
 *
 *   * `idle`          — constructed, not connected yet.
 *   * `syncing`       — subscribe in flight OR sync-step-2 pending.
 *   * `synced`        — at least one incoming update was applied
 *                       (or an empty sync-step-2 came back).
 *   * `disconnected`  — was working, lost the connection. Reconnect
 *                       machinery in the bus client tries to recover.
 *   * `denied`        — subscribe was refused by the server. Terminal
 *                       for this file id: no retry will succeed
 *                       spontaneously. Covers "no Read grant" AND
 *                       "unknown file" both — the server collapses the
 *                       two to the same `no_read` wire code for
 *                       anti-enumeration, and so does this state.
 *   * `unavailable`   — bus-level circuit breaker tripped. The server
 *                       is unreachable, an explicit user action
 *                       (refresh) is required to retry.
 */
export type SyncState = 'idle' | 'syncing' | 'synced' | 'disconnected' | 'denied' | 'unavailable';

export interface CollabDocOpts {
	fileId: string;
	/** Called whenever `syncState` changes so the UI can render a
	 *  status pill without wiring a Svelte store from inside a plain
	 *  TS class. */
	onSyncStateChange?: (state: SyncState) => void;
	/** Called when the server ack'd the collab subscribe and returned
	 *  the caller's per-file capabilities. Fires once per subscribe
	 *  cycle (initial + every reconnect replay). Consumers gate the
	 *  editor on `canWrite`: `false` means the caller has Read but
	 *  not Update — CodeMirror MUST mount read-only, and outbound
	 *  UPDATE frames MUST be suppressed (server would reject them
	 *  and, today, drop the WS). */
	onCapabilities?: (caps: CollabCapabilities) => void;
}

export class CollabDoc {
	readonly fileId: string;
	readonly doc: Y.Doc;
	/** Presence / awareness registry — cursor position, user handle,
	 *  colour. Passed to `yCollab(yText, awareness)` in the editor so
	 *  `y-codemirror.next` renders peer cursors with names. Emits
	 *  binary `0x02 AWARENESS` frames on local changes; applies
	 *  incoming `0x02` frames from the wire. */
	readonly awareness: awarenessProtocol.Awareness;

	#syncState: SyncState = 'idle';
	#onSyncStateChange?: (state: SyncState) => void;
	#onCapabilities?: (caps: CollabCapabilities) => void;
	/** Dispose fn returned by `messageBus.registerWriteDeniedHandler`,
	 *  cleared in `destroy()`. */
	#unregisterWriteDenied: (() => void) | null = null;
	/** Effective write capability. Suppresses outbound UPDATE frames
	 *  when false — the local Y.Doc still mutates freely (the editor
	 *  handles gating), but nothing goes on the wire so the server
	 *  never has to enforce `no_edit` on this session. Server truth
	 *  wins: `false` is set from the subscribe ack, and stays false
	 *  until a reconnect that returns `can_write: true` (or a fresh
	 *  attach). Defaults to `false` — a fail-closed default matches
	 *  the server-side gate. */
	#canWrite = false;
	#unsubscribeTopic: UnsubscribeHandle | null = null;
	#unregisterHandler: (() => void) | null = null;
	#docUpdateHandler: ((update: Uint8Array, origin: unknown) => void) | null = null;
	#awarenessUpdateHandler:
		| ((
				changes: { added: number[]; updated: number[]; removed: number[] },
				origin: unknown
		  ) => void)
		| null = null;
	#destroyed = false;

	constructor(opts: CollabDocOpts) {
		this.fileId = opts.fileId;
		this.#onSyncStateChange = opts.onSyncStateChange;
		this.#onCapabilities = opts.onCapabilities;
		this.doc = new Y.Doc();
		this.awareness = new awarenessProtocol.Awareness(this.doc);
	}

	/** Whether the caller has Update on this file, per the last
	 *  server-side ack. Consumers gate outbound edits on this — the
	 *  local doc still mutates freely (rendering-only), but frames
	 *  won't hit the wire. */
	get canWrite(): boolean {
		return this.#canWrite;
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

		// Register the write-denied handler so an out-of-band grant
		// change (or a legacy client that ignored the FE gate) triggers
		// the same read-only UX the subscribe-ack path uses. Flipping
		// `#canWrite` here means the local Y.Doc origin gate stops
		// firing UPDATE frames, and the caller's `onCapabilities`
		// callback flips the editor into read-only.
		this.#unregisterWriteDenied = messageBus.registerWriteDeniedHandler(this.fileId, (params) => {
			collabLog.warn('server refused write — dropping to read-only', {
				fileId: this.fileId,
				reason: params.reason
			});
			if (this.#canWrite) {
				this.#canWrite = false;
				this.#onCapabilities?.({ canWrite: false });
			}
		});

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
				// Server evicted us. Two sub-cases:
				//   * `subscribe_denied` — synthetic reason the bus
				//     client emits when the INITIAL subscribe was
				//     refused (no Read grant or unknown file — the
				//     server collapses the two for anti-enum). This
				//     is terminal for this fileId; UI should render
				//     a "no access" state, not a "reconnecting" one.
				//   * Anything else — mid-session eviction (grant
				//     revoked, file deleted, admin kicked, session
				//     migrated). UI signals "disconnected" and the
				//     bus's reconnect machinery gives up after the
				//     server refuses subsequent attempts.
				collabLog.warn('collab subscription revoked', {
					fileId: this.fileId,
					reason: revoked.reason
				});
				// `subscribe_denied` is a client-synthetic reason emitted
				// by `MessageBusClient.#failSubscribe` — outside the
				// server-owned `RtRevokedReason` enum, so widen to
				// `string` for the compare. Consumers should always
				// treat unknown reasons defensively anyway.
				const reason: string = revoked.reason;
				this.#setSyncState(reason === 'subscribe_denied' ? 'denied' : 'disconnected');
			},
			(ack) => {
				// The subscribe ack carries per-file capabilities for
				// collab topics — see `RtSubscribeAck.capabilities`.
				// Missing / malformed capabilities fall back to
				// `canWrite: false` (fail-closed): if the server didn't
				// advertise a write capability, we don't offer edit UX.
				// Fires once per subscribe cycle including reconnect
				// replays, so an admin that toggles the caller's Update
				// grant between socket drops sees the new capability
				// take effect on the next reconnect.
				const canWrite = ack.capabilities?.can_write === true;
				this.#canWrite = canWrite;
				this.#onCapabilities?.({ canWrite });
			}
		);

		// Local updates → wire. Filter by origin so the update we
		// apply from a peer (origin === this) doesn't echo back.
		//
		// Also gate on `canWrite`: a Viewer's local Y.Doc still applies
		// (the editor mounts read-only so this is mostly moot, but
		// programmatic mutations elsewhere in the class — e.g. seeding
		// — must not leak upward). Without this gate, sending a Viewer's
		// UPDATE gets the WS closed by the server's `no_edit` handler
		// (a hard close, not a graceful denial — the graceful path is
		// a separate follow-up slice).
		this.#docUpdateHandler = (update, origin) => {
			if (origin === this) return; // remote-applied, don't reflect
			if (!this.#canWrite) return; // read-only: swallow local edits
			const frame = encodeFrame(KIND_UPDATE, this.fileId, update);
			messageBus.sendBinary(frame);
		};
		this.doc.on('update', this.#docUpdateHandler);

		// Awareness → wire. Same origin-guard as CRDT updates: an
		// incoming peer awareness we've just applied would otherwise
		// echo back to the server. `y-protocols/awareness`'s change
		// signal names the client IDs whose state changed; encoding
		// against just those clients produces the minimal wire diff.
		this.#awarenessUpdateHandler = (changes, origin) => {
			if (origin === this) return;
			const changedIds = [...changes.added, ...changes.updated, ...changes.removed];
			if (changedIds.length === 0) return;
			const update = awarenessProtocol.encodeAwarenessUpdate(this.awareness, changedIds);
			const frame = encodeFrame(KIND_AWARENESS, this.fileId, update);
			messageBus.sendBinary(frame);
		};
		this.awareness.on('update', this.#awarenessUpdateHandler);

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
		if (this.#awarenessUpdateHandler) {
			this.awareness.off('update', this.#awarenessUpdateHandler);
			this.#awarenessUpdateHandler = null;
		}
		if (this.#unregisterHandler) {
			this.#unregisterHandler();
			this.#unregisterHandler = null;
		}
		if (this.#unregisterWriteDenied) {
			this.#unregisterWriteDenied();
			this.#unregisterWriteDenied = null;
		}
		if (this.#unsubscribeTopic) {
			this.#unsubscribeTopic();
			this.#unsubscribeTopic = null;
		}
		// `Awareness.destroy` emits one final "remove this client" tick
		// that peers use to render the cursor going away. Do it BEFORE
		// tearing down the Y.Doc — Awareness is bound to the doc's
		// clientID and needs the doc alive to encode the removal.
		this.awareness.destroy();
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
			case KIND_AWARENESS: {
				// Peer presence — apply into our local awareness
				// registry. Same origin-guard as CRDT updates so the
				// applied change doesn't echo back to the server.
				try {
					awarenessProtocol.applyAwarenessUpdate(this.awareness, frame.payload, this);
				} catch (err) {
					collabLog.warn('applyAwarenessUpdate failed', {
						fileId: this.fileId,
						error: err
					});
				}
				break;
			}
		}
	}
}
