/**
 * Interrupted-upload registry — small helper so a page reload during
 * upload doesn't leave the user without a hint that work was in flight.
 *
 * Two coordinated behaviours:
 *
 *   1. `beforeunload` warning while any upload is active.
 *      Registered lazily: as soon as an upload starts we install a
 *      page-scope handler that triggers the browser's "Leave site?
 *      Changes may not be saved" prompt on refresh / tab close.
 *      Deliberate leave (user clicks Leave) proceeds normally.
 *
 *   2. `sessionStorage`-backed "interrupted uploads" register.
 *      Every `uploadBatch` writes a record while it runs, clears it on
 *      completion. If a reload happens mid-flight, the record survives
 *      into the next page load and `readAndClearInterrupted` surfaces
 *      it so the layout can toast: "Uploads were interrupted — re-drop
 *      the files to resume (already-uploaded chunks reuse)."
 *
 *      `sessionStorage` (not `localStorage`) on purpose: entries clear
 *      when the tab closes entirely, so a "closed the tab an hour ago"
 *      user isn't nagged. Only a same-tab reload preserves them.
 */

/** Key under which the interrupted-uploads register is stored. */
const STORAGE_KEY = 'oxi:upload:interrupted';

/** One record per active `uploadBatch()` invocation. */
export interface InterruptedRecord {
	/** UI-facing description — filename for singleton uploads, "N files" for batches. */
	description: string;
	/** Where the batch was targeted. `null` = drive root. */
	folderId: string | null;
	/** UNIX ms — used as the identity key that matches start/finish
	 *  calls so multiple concurrent batches don't step on each other. */
	startedAt: number;
}

// ── Page-scope beforeunload guard ────────────────────────────────────

let activeBatches = 0;

function beforeUnloadHandler(e: BeforeUnloadEvent): void {
	// Spec-compliant trigger for the browser's "Leave site?" dialog.
	// Modern Chrome/Firefox/Safari all honor preventDefault(). The
	// browser shows its own confirmation copy — we can't customize it.
	e.preventDefault();
}

/** Register a live upload batch so the beforeunload guard is active while
 *  it runs. Balanced by `releaseUploadGuard` in the batch's finally. */
export function acquireUploadGuard(): void {
	if (typeof window === 'undefined') return;
	if (activeBatches === 0) {
		window.addEventListener('beforeunload', beforeUnloadHandler);
	}
	activeBatches++;
}

/** Match to `acquireUploadGuard`; when the last active batch releases,
 *  the beforeunload listener is removed so unrelated navigations don't
 *  trigger the browser's "leave site?" prompt. */
export function releaseUploadGuard(): void {
	if (typeof window === 'undefined') return;
	activeBatches = Math.max(0, activeBatches - 1);
	if (activeBatches === 0) {
		window.removeEventListener('beforeunload', beforeUnloadHandler);
	}
}

// ── sessionStorage register ──────────────────────────────────────────

function readAll(): InterruptedRecord[] {
	if (typeof sessionStorage === 'undefined') return [];
	try {
		const raw = sessionStorage.getItem(STORAGE_KEY);
		if (!raw) return [];
		const parsed = JSON.parse(raw);
		return Array.isArray(parsed) ? (parsed as InterruptedRecord[]) : [];
	} catch {
		return [];
	}
}

function writeAll(records: InterruptedRecord[]): void {
	if (typeof sessionStorage === 'undefined') return;
	try {
		if (records.length === 0) sessionStorage.removeItem(STORAGE_KEY);
		else sessionStorage.setItem(STORAGE_KEY, JSON.stringify(records));
	} catch {
		/* quota exceeded / disabled — silent; the register is best-effort. */
	}
}

/** Add a record when a batch starts. Returns a handle to pass back to
 *  `markUploadFinished` on completion / failure — that way multiple
 *  concurrent batches don't step on each other's entries. */
export function markUploadStarted(description: string, folderId: string | null): number {
	const record: InterruptedRecord = {
		description,
		folderId,
		startedAt: Date.now()
	};
	const all = readAll();
	all.push(record);
	writeAll(all);
	return record.startedAt;
}

/** Remove the record when the batch completes (success or failure). Uses
 *  the `markUploadStarted` return value as the identity key. */
export function markUploadFinished(startedAt: number): void {
	const all = readAll();
	const idx = all.findIndex((r) => r.startedAt === startedAt);
	if (idx >= 0) {
		all.splice(idx, 1);
		writeAll(all);
	}
}

/** Read and clear the register — called by the layout on mount. Returns
 *  what was there. Empties the register in the same call so a subsequent
 *  refresh doesn't re-notify. */
export function readAndClearInterrupted(): InterruptedRecord[] {
	const all = readAll();
	writeAll([]);
	return all;
}
