/**
 * Reading an OS file drop: what the user dropped, flattened into something
 * uploadable.
 *
 * Lived inside `/files/[...path]/+page.svelte` until the sidebar's drive rows
 * became drop targets too (2026-11-13). Both callers have to make the same
 * decision — directory tree or flat file list — and `webkitGetAsEntry()` has
 * a rule (below) that is easy to get wrong when copied, so it lives in one
 * place.
 */

/** A dropped file plus its path relative to the drop root. */
export type DroppedEntry = { file: File; relativePath: string };

/**
 * Expand dropped OS entries into `{file, relativePath}` rows, walking any
 * directory tree via the (non-standard but ubiquitous) `webkitGetAsEntry` /
 * `createReader` API. Returns `null` when nothing dropped was a directory, so
 * the caller takes the simpler flat-`FileList` path.
 */
export async function collectDroppedEntries(dt: DataTransfer): Promise<DroppedEntry[] | null> {
	// `webkitGetAsEntry()` must be read synchronously while the event is live.
	const roots: FileSystemEntry[] = [];
	let sawDir = false;
	for (const item of Array.from(dt.items)) {
		const entry = item.webkitGetAsEntry();
		if (entry) {
			roots.push(entry);
			if (entry.isDirectory) sawDir = true;
		}
	}
	if (!sawDir) return null;

	const out: DroppedEntry[] = [];
	async function walk(entry: FileSystemEntry, prefix: string): Promise<void> {
		if (entry.isFile) {
			const file = await new Promise<File>((resolve, reject) =>
				(entry as FileSystemFileEntry).file(resolve, reject)
			);
			out.push({ file, relativePath: prefix + entry.name });
		} else if (entry.isDirectory) {
			const reader = (entry as FileSystemDirectoryEntry).createReader();
			const dirPrefix = `${prefix}${entry.name}/`;
			// readEntries yields in batches; loop until it returns an empty one.
			for (;;) {
				const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
					reader.readEntries(resolve, reject)
				);
				if (batch.length === 0) break;
				for (const child of batch) await walk(child, dirPrefix);
			}
		}
	}
	for (const root of roots) await walk(root, '');
	return out;
}

/**
 * The whole payload of a drop, in the two shapes an uploader needs: a
 * directory `tree` when one was dropped, otherwise the flat `files`.
 *
 * Read this BEFORE the drop handler yields, and keep the result — a
 * `DataTransfer`'s `items` and `files` are emptied once the event is done, so
 * a handler that hands the drop off somewhere else (the sidebar staging a
 * drop for `/files` to perform) must extract first and pass the extraction,
 * never the event.
 */
export async function readDroppedUpload(
	dt: DataTransfer
): Promise<{ tree: DroppedEntry[] | null; files: File[] }> {
	const tree = await collectDroppedEntries(dt);
	return { tree, files: tree ? [] : Array.from(dt.files ?? []) };
}
