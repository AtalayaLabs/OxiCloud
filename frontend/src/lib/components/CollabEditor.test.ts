import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, waitFor } from '@testing-library/svelte';
import * as Y from 'yjs';

const { bus } = vi.hoisted(() => {
	let binaryHandler: ((bytes: Uint8Array) => void) | null = null;
	return {
		bus: {
			subscribe: vi.fn(() => () => {}),
			registerBinaryHandler: vi.fn((_fileId: string, handler: (b: Uint8Array) => void) => {
				binaryHandler = handler;
				return () => {
					binaryHandler = null;
				};
			}),
			registerWriteDeniedHandler: vi.fn(() => () => {}),
			collabFlush: vi.fn(async () => {}),
			sendBinary: vi.fn(() => true),
			whenConnected: vi.fn(async () => {}),
			onReconnect: vi.fn(() => () => {}),
			/** Deliver a server frame to the doc, as the socket would. */
			deliver: (bytes: Uint8Array) => binaryHandler?.(bytes)
		}
	};
});
vi.mock('$lib/message-bus/client.svelte', () => ({ messageBus: bus }));

import CollabEditor from './CollabEditor.svelte';
import { KIND_SYNC } from '$lib/collab/wireCodec';
import { encodeFrame } from '$lib/collab/wireCodec';

const FILE_ID = '11111111-2222-3333-4444-555555555555';
const BODY = 'Ahoj z CRDT';

/** A sync-step-2 frame carrying `BODY` under the shared root name. */
function syncFrame(): Uint8Array {
	const source = new Y.Doc();
	source.getText('content').insert(0, BODY);
	return encodeFrame(KIND_SYNC, FILE_ID, Y.encodeStateAsUpdate(source));
}

beforeEach(() => vi.clearAllMocks());

describe('initial content', () => {
	// CodeMirror and `y-codemirror.next` are dynamic imports, so several
	// chunks load between `connect()` and the binding. Sync-step-2 answers
	// over an open socket in a millisecond or two and routinely wins that
	// race — and `yCollab` only applies updates that arrive after it binds.
	// Seeding `EditorState` from an empty string therefore left the editor
	// permanently blank, with no failed request to point at.
	it('shows a document that arrived before the editor bound', async () => {
		render(CollabEditor, { fileId: FILE_ID, filename: 'README.md' });
		await waitFor(() => expect(bus.registerBinaryHandler).toHaveBeenCalled());
		bus.deliver(syncFrame());

		await waitFor(() => expect(document.querySelector('.cm-content')).not.toBeNull());
		await waitFor(() => expect(document.querySelector('.cm-content')?.textContent).toContain(BODY));
	});

	it('shows a document that arrived after the editor bound', async () => {
		render(CollabEditor, { fileId: FILE_ID, filename: 'README.md' });
		await waitFor(() => expect(document.querySelector('.cm-content')).not.toBeNull());
		bus.deliver(syncFrame());

		await waitFor(() => expect(document.querySelector('.cm-content')?.textContent).toContain(BODY));
		// Seeding from the CRDT must not double up with the binding's own
		// delta when the update lands on either side of the bind.
		expect(document.querySelector('.cm-content')?.textContent?.split(BODY)).toHaveLength(2);
	});
});
