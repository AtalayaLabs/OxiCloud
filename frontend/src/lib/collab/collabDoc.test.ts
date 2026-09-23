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
import { KIND_AWARENESS, KIND_SYNC, decodeFrame } from './wireCodec';

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

	it('sends no handshake once destroyed', async () => {
		const doc = new CollabDoc({ fileId: '11111111-2222-3333-4444-555555555555' });
		doc.connect();
		doc.destroy();
		await bus.open();
		// Scoped to SYNC rather than "nothing at all": `destroy()`
		// legitimately emits one AWARENESS frame retracting this
		// client's presence (see the departure suite below). The
		// handshake is what must not fire on a doc nobody is holding.
		expect(bus.sent.filter((b) => decodeFrame(b)?.kind === KIND_SYNC)).toHaveLength(0);
	});
});

// A reloaded tab returns with a brand-new awareness clientID. The old
// one only leaves its peers' registries if somebody announces it — and
// nobody did, so every refresh added a phantom person to everyone
// else's presence badge.
describe('departure announcement', () => {
	const FILE = '11111111-2222-3333-4444-555555555555';
	const departures = () =>
		bus.sent.map((b) => decodeFrame(b)).filter((f) => f?.kind === KIND_AWARENESS);

	it('reaches the wire while the send path is still wired', async () => {
		const doc = new CollabDoc({ fileId: FILE });
		doc.connect();
		await bus.open();
		// Publish some presence so there is a state to retract.
		doc.awareness.setLocalStateField('user', { name: 'ed' });
		bus.sent.length = 0;

		doc.destroy();

		// `destroy()` used to detach the awareness update handler BEFORE
		// calling `awareness.destroy()`, so the "I'm gone" tick was
		// emitted into a void.
		expect(departures()).toHaveLength(1);
	});

	it('happens only once across pagehide and destroy', async () => {
		const doc = new CollabDoc({ fileId: FILE });
		doc.connect();
		await bus.open();
		doc.awareness.setLocalStateField('user', { name: 'ed' });
		bus.sent.length = 0;

		// Both hooks fire on a normal reload; either may win.
		doc.announceDeparture();
		doc.destroy();

		expect(departures()).toHaveLength(1);
	});
});
