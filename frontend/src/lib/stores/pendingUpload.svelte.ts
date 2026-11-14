import { untrack } from 'svelte';
import type { DroppedEntry } from '$lib/utils/droppedEntries';

/**
 * One OS file drop, extracted and waiting for `/files` to perform it.
 *
 * Why a store and not a direct call: the uploader is page-local to
 * `/files/[...path]` — it owns the progress notification, the abort handle,
 * the reload-guard breadcrumb and the post-upload reload. The sidebar's drive
 * rows (`DrivePicker`, rendered by `AppShell` on EVERY route) accept OS files
 * too, and they have none of that. So the sidebar extracts the drop, stages
 * it here, and navigates to the target drive's root; the page drains it as
 * soon as that folder is loaded.
 *
 * Navigating is a feature, not a side effect: an upload the user cannot see
 * land is an upload they will re-do. Dropping on a drive therefore means
 * "put these there, and take me there".
 *
 * The staged `File` objects are live handles to the OS files — cheap to hold
 * (no bytes are read) but only valid for this document, which is why nothing
 * here is persisted.
 */
export type PendingUpload = {
	/** Folder the files go into — a drive's `root_folder_id`. */
	targetFolderId: string;
	/** Directory tree, when a folder was dropped. Mutually exclusive with `files`. */
	tree: DroppedEntry[] | null;
	/** Flat drop. */
	files: File[];
};

let staged = $state<PendingUpload | null>(null);
/** When `staged` was set, for the staleness rule in `claim`. */
let stagedAt = 0;

/**
 * How long a staged drop stays claimable.
 *
 * Without a bound, a drop whose navigation never arrived (a folder that
 * failed to load, a user who went somewhere else first) would sit here and
 * fire the next time that folder happened to be opened — an upload the user
 * did not ask for, minutes or hours after the gesture. Generous enough to
 * cover a slow cold load, short enough that nothing surprises anyone.
 */
const STALE_AFTER_MS = 2 * 60 * 1000;

export const pendingUpload = {
	/** Folder a staged drop is waiting on, or null — cheap to poll in an `$effect`. */
	get targetFolderId(): string | null {
		return staged?.targetFolderId ?? null;
	},

	/**
	 * Hold a drop for `/files` to perform. Overwrites any previous one: two
	 * drops in flight at once has no sane meaning, and the newer gesture is
	 * the one the user is watching.
	 */
	stage(upload: PendingUpload): void {
		untrack(() => {
			staged = upload;
			stagedAt = Date.now();
		});
	},

	/**
	 * Claim the drop staged for `folderId`, clearing it — returns null when
	 * nothing is staged for that folder. Clearing on claim is what keeps the
	 * draining `$effect` from uploading the same files twice when it re-runs.
	 */
	claim(folderId: string): PendingUpload | null {
		return untrack(() => {
			if (staged?.targetFolderId !== folderId) return null;
			const claimed = staged;
			staged = null;
			if (Date.now() - stagedAt > STALE_AFTER_MS) return null;
			return claimed;
		});
	},

	/**
	 * Drop a staged upload without performing it. For the case where the
	 * navigation never reaches its folder (a failed load, or the user going
	 * somewhere else first) — stale `File` handles must not fire into
	 * whatever folder is open later.
	 */
	discard(): void {
		untrack(() => {
			staged = null;
		});
	}
};
