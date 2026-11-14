import { it, expect, beforeEach, afterEach, vi } from 'vitest';
import { pendingUpload } from './pendingUpload.svelte';

const file = (name: string) => new File(['x'], name, { type: 'text/plain' });

beforeEach(() => {
	pendingUpload.discard();
});

afterEach(() => {
	vi.useRealTimers();
	pendingUpload.discard();
});

it('hands the staged drop to the folder it was aimed at', () => {
	pendingUpload.stage({ targetFolderId: 'drive-root', tree: null, files: [file('a.txt')] });
	expect(pendingUpload.targetFolderId).toBe('drive-root');

	const claimed = pendingUpload.claim('drive-root');
	expect(claimed?.files.map((f) => f.name)).toEqual(['a.txt']);
});

// The destination is the whole point of the handoff: these files were dropped
// on ONE drive, and the page that drains them is the same page every other
// folder is browsed through. A claim that ignored the id would upload them
// into whichever folder happened to be open when the effect ran.
it('refuses a claim from any other folder, and keeps the drop staged', () => {
	pendingUpload.stage({ targetFolderId: 'drive-root', tree: null, files: [file('a.txt')] });

	expect(pendingUpload.claim('some-other-folder')).toBeNull();
	expect(pendingUpload.targetFolderId).toBe('drive-root');
	expect(pendingUpload.claim('drive-root')).not.toBeNull();
});

// The draining `$effect` re-runs on every `currentId`/stage change, so "claim
// clears" is what stands between one gesture and two uploads of the same
// files.
it('yields the drop exactly once', () => {
	pendingUpload.stage({ targetFolderId: 'f1', tree: null, files: [file('a.txt')] });

	expect(pendingUpload.claim('f1')).not.toBeNull();
	expect(pendingUpload.claim('f1')).toBeNull();
	expect(pendingUpload.targetFolderId).toBeNull();
});

// A stage whose navigation never arrived (folder failed to load, user went
// elsewhere first) must not fire the next time that folder is opened — an
// upload nobody asked for, long after the drag.
it('expires a drop it never got to perform', () => {
	vi.useFakeTimers();
	pendingUpload.stage({ targetFolderId: 'f1', tree: null, files: [file('a.txt')] });

	vi.advanceTimersByTime(2 * 60 * 1000 + 1);
	expect(pendingUpload.claim('f1')).toBeNull();
	// And it is gone, not merely withheld.
	expect(pendingUpload.targetFolderId).toBeNull();
});

// A dropped FOLDER arrives as a walked tree, not a FileList; both shapes have
// to survive the handoff because the two uploaders on the page are different
// functions (`uploadTree` vs `uploadBatch`).
it('carries a dropped directory tree through unchanged', () => {
	pendingUpload.stage({
		targetFolderId: 'f1',
		tree: [{ file: file('deep.txt'), relativePath: 'top/deep.txt' }],
		files: []
	});

	const claimed = pendingUpload.claim('f1');
	expect(claimed?.tree?.[0].relativePath).toBe('top/deep.txt');
	expect(claimed?.files).toEqual([]);
});

it('keeps only the newest gesture when two drops are staged', () => {
	pendingUpload.stage({ targetFolderId: 'f1', tree: null, files: [file('old.txt')] });
	pendingUpload.stage({ targetFolderId: 'f2', tree: null, files: [file('new.txt')] });

	expect(pendingUpload.claim('f1')).toBeNull();
	expect(pendingUpload.claim('f2')?.files.map((f) => f.name)).toEqual(['new.txt']);
});
