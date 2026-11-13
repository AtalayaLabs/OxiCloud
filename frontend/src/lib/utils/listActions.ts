/**
 * Row actions every resource listing needs, in one place.
 *
 * These were copy-pasted per route before this module existed: the favourite
 * toggle in five files (`/recent`, `/favorites`, `/shared-with-me`, `/search`,
 * `/files`, plus `PhotoLightbox`), and the "Open parent folder" context entry —
 * with its three-state access probe — in four. Identical bodies, which means
 * a fix to one (say, the `folderAccessCached() === false` check that stops a
 * "flash of enabled") silently misses the rest.
 *
 * `/changes` is the first consumer. The older pages still carry their own
 * copies; migrating them is a mechanical follow-up and a separate diff, since
 * it touches five user-facing surfaces at once.
 */
import { goto } from '$app/navigation';
import { resolve } from '$app/paths';
import { addFavorite, removeFavorite } from '$lib/api/endpoints/favorites';
import { fileDownloadUrl } from '$lib/api/endpoints/files';
import { folderZipUrl } from '$lib/api/endpoints/folders';
import type { FileItem, FolderItem } from '$lib/api/types';
import { isFile, type ContextAction } from '$lib/components/ResourceList.svelte';
import { t } from '$lib/i18n/index.svelte';
import { errorToast } from '$lib/utils/errors';
import { folderAccessCached, probeFolderAccess } from '$lib/utils/folderAccess';

type Item = FileItem | FolderItem;

/**
 * The parent folder to open for an item, or `null` when there is none.
 *
 * Files carry `folder_id` (required by the DTO); folders carry `parent_id`,
 * which is null for a drive root. Collapsing both to one `null` gives callers
 * a single "nothing to open" signal.
 */
export function parentFolderIdOf(item: Item): string | null {
	return isFile(item) ? item.folder_id : (item.parent_id ?? null);
}

/**
 * Optimistic favourite toggle: flips the row immediately, reverts and toasts
 * if the write fails, so the star never shows a state the server rejected.
 */
export async function toggleFavorite(item: Item): Promise<void> {
	const kind = isFile(item) ? 'file' : 'folder';
	const wasFav = item.is_favorite;
	item.is_favorite = !wasFav;
	try {
		if (wasFav) await removeFavorite(kind, item.id);
		else await addFavorite(kind, item.id);
	} catch (e) {
		errorToast(e);
		item.is_favorite = wasFav;
	}
}

/** Download a file directly, or a folder as a server-built ZIP. */
export function downloadItem(item: Item): void {
	const a = document.createElement('a');
	a.href = isFile(item) ? fileDownloadUrl(item.id) : folderZipUrl(item.id);
	a.download = isFile(item) ? item.name : `${item.name}.zip`;
	document.body.appendChild(a);
	a.click();
	a.remove();
}

/**
 * "Open parent folder".
 *
 * Hidden only when there is literally no parent (a drive-root folder).
 * Otherwise always visible, and DISABLED when the caller lacks read on the
 * parent — a greyed row reads as "you can't do this here", where a missing
 * entry reads as "this feature doesn't exist". `folderAccessCached` answers
 * `true` / `false` / `undefined`, and only an explicit `false` disables, so an
 * unprobed row is not greyed by default.
 *
 * Pair with {@link prepareParentAccess} as the host's `menuPrepare`, which
 * primes the cache before the menu paints and avoids a flash of enabled.
 */
export function openParentContextAction(): ContextAction {
	return {
		key: 'open_parent',
		label: t('files.open_parent', 'Open parent folder'),
		icon: 'folder-open',
		visible: (item) => parentFolderIdOf(item) !== null,
		disabled: (item) => {
			const pid = parentFolderIdOf(item);
			return pid === null || folderAccessCached(pid) === false;
		},
		run: (item) => {
			const pid = parentFolderIdOf(item);
			if (pid) goto(resolve(`/files/${pid}`));
		}
	};
}

/**
 * `menuPrepare` companion for {@link openParentContextAction}: probes the
 * parent's readability lazily, only when a menu actually opens on that row,
 * and caches the answer for the session.
 */
export async function prepareParentAccess(item: Item): Promise<void> {
	const pid = parentFolderIdOf(item);
	if (pid) await probeFolderAccess(pid);
}

/** "Download" — files only; pair with {@link downloadZipContextAction}. */
export function downloadContextAction(): ContextAction {
	return {
		key: 'download',
		label: t('common.download', 'Download'),
		icon: 'download',
		visible: (item) => isFile(item),
		run: downloadItem
	};
}

/**
 * "Download as ZIP" — folders only. Two entries rather than one label that
 * changes, so the wording and the icon both read correctly per kind.
 */
export function downloadZipContextAction(): ContextAction {
	return {
		key: 'download_zip',
		label: t('files.download_zip', 'Download as ZIP'),
		icon: 'download',
		visible: (item) => !isFile(item),
		run: downloadItem
	};
}
