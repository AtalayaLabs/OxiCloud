import { describe, expect, it, vi, beforeEach } from 'vitest';

const { bus } = vi.hoisted(() => {
	let release: (() => void) | null = null;
	let reconnect: (() => void) | null = null;
	return {
		bus: {
			sent: [] as Uint8Array[],
			subscribe: vi.fn(() => () => {}),
			registerBinaryHandler: vi.fn(() => () => {}),
			registerWriteDeniedHandler: vi.fn(() => () => {}),
			collabFlush: vi.fn(async () => {}),
			sendBinary: vi.fn((bytes: Uint8Array) => {
				bus.sent.push(bytes);
				return true;
			}),
			whenConnected: vi.fn(() => new Promise<void>((resolve) => (release = resolve))),
			onReconnect: vi.fn((cb: () => void) => {
				reconnect = cb;
				return () => {
					reconnect = null;
				};
			}),
			/** Let the pending `whenConnected()` resolve. */
			open: async () => {
				release?.();
				release = null;
				await Promise.resolve();
			},
			/** Fire the bus's reconnect hook. */
			reopen: () => reconnect?.()
		}
	};
});
vi.mock('$lib/message-bus/client.svelte', () => ({ messageBus: bus }));

import { CollabDoc } from './collabDoc';
import { KIND_SYNC, decodeFrame } from './wireCodec';

beforeEach(() => {
	bus.sent.length = 0;
	vi.clearAllMocks();
});

describe('sync-step-1 handshake', () => {
	// The frame carries the state vector the server diffs against, and nobody
	// retries it. Sending it into a socket that is still connecting drops it
	// silently — the editor then shows an empty document with no failed
	// request to point at.
	it('waits for the socket instead of firing into a connecting one', async () => {
		const doc = new CollabDoc({ fileId: '11111111-2222-3333-4444-555555555555' });
		doc.connect();
		expect(bus.sendBinary).not.toHaveBeenCalled();

		await bus.open();
		expect(bus.sent).toHaveLength(1);
		expect(decodeFrame(bus.sent[0])?.kind).toBe(KIND_SYNC);
		doc.destroy();
	});

	// The bus replays subscribes on reconnect but not binary frames, so
	// without this a socket that dropped mid-session comes back subscribed to
	// a document nobody re-synced.
	it('re-syncs after a reconnect', async () => {
		const doc = new CollabDoc({ fileId: '11111111-2222-3333-4444-555555555555' });
		doc.connect();
		await bus.open();
		bus.reopen();
		expect(bus.sent).toHaveLength(2);
		expect(decodeFrame(bus.sent[1])?.kind).toBe(KIND_SYNC);
		doc.destroy();
	});

	it('sends nothing once destroyed', async () => {
		const doc = new CollabDoc({ fileId: '11111111-2222-3333-4444-555555555555' });
		doc.connect();
		doc.destroy();
		await bus.open();
		expect(bus.sendBinary).not.toHaveBeenCalled();
	});
});
