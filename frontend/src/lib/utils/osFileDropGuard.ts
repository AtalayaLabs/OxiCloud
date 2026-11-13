/**
 * App-wide guard for files dragged in from the OPERATING SYSTEM.
 *
 * ## Why this is a correctness fix, not a nicety
 *
 * A browser's default action for a file dropped on a page is to NAVIGATE to
 * that file — replacing the SPA. Unsaved state goes with it: an in-progress
 * search, a collab editor buffer, a half-filled dialog. Any page that does not
 * handle the drop itself is one mis-aimed drag away from that, so the guard
 * belongs to the APPLICATION rather than to the pages that remembered.
 *
 * Attached once, in the root `+layout.svelte`, so it covers every route —
 * including `/login` and the public `/s/[token]` viewer, which never render
 * `AppShell` and were previously unprotected.
 *
 * ## How a page says "I handle drops"
 *
 * Two ways, and both are needed.
 *
 * **1. `e.defaultPrevented`** — for any ELEMENT that is a drop zone. Calling
 * `preventDefault()` on `dragover` is MANDATORY for `drop` to fire at all, so
 * every real drop zone already does it, and the flag survives the rest of the
 * event's propagation to `window`. `/files`' folder rows, its breadcrumb
 * crumbs, the sidebar's drive rows and `.rl-root` all qualify without
 * registering anything.
 *
 * **2. `osFileDrop.claim()`** — for a page whose drop zone IS the window.
 * `/files` accepts a drop anywhere on the page, margins and title strip
 * included, by listening on `window` too; and two `window` listeners run in
 * registration order, which the layout always wins because it mounts first.
 * So the guard would swallow the drop and toast "wrong drop zone", and the
 * page's handler, running second, would see `defaultPrevented` and skip the
 * upload. A claim is checked BEFORE `preventDefault` here, which hands the
 * whole contract — navigation protection included — to the owner.
 *
 * This was shipped with only (1) and the two `drop:` cases in
 * `tests/e2e/spa/upload-drop.spec.ts` caught it, because they drop on the
 * page container rather than on the list.
 */
import { goto } from '$app/navigation';
import { resolve } from '$app/paths';
import { t } from '$lib/i18n/index.svelte';
import { osFileDrop } from '$lib/stores/osFileDrop.svelte';
import { session } from '$lib/stores/session.svelte';
import { ui } from '$lib/stores/ui.svelte';

/**
 * True only for a drag carrying OS files.
 *
 * Internal drags (moving items between folders, onto a drive, onto Favorites)
 * use their own MIME types and must pass through untouched — hijacking them
 * would break every in-app drag in the product.
 */
export function isOsFileDrag(e: DragEvent): boolean {
	return !!e.dataTransfer?.types?.includes('Files');
}

/**
 * `dragover`: accept at the pointer level so `drop` fires.
 *
 * `dropEffect = 'none'` would be the honest cursor on a page that cannot
 * accept uploads — but it makes the browser REJECT the drop before `drop`
 * runs, so the explanation below would never be shown. `ResourceList` made
 * the same trade for the same reason: accept the pointer, decide on drop.
 */
export function onOsFileDragOver(e: DragEvent): void {
	if (!isOsFileDrag(e)) return;
	if (e.defaultPrevented) return;
	// The owning page sets its own `dropEffect` and may want a different one;
	// leave the pointer entirely to it, same as the drop below.
	if (osFileDrop.claimed) return;
	e.preventDefault();
	if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
}

/**
 * `drop`: swallow it, and — when there is somewhere to send the user — say
 * where uploads live.
 *
 * The `preventDefault` is unconditional for an UNCLAIMED drop, because
 * stopping the navigation is the part that protects data. The toast is not:
 * "Go to Files" is meaningless to someone on the login screen or viewing a
 * public share link, so an unauthenticated drop is refused silently.
 *
 * A claimed drop is left completely untouched — preventing the default here
 * would make the owner's own `window` handler, which runs after this one,
 * skip the upload as "already handled".
 */
export function onOsFileDrop(e: DragEvent): void {
	if (!isOsFileDrag(e)) return;
	if (e.defaultPrevented) return;
	if (osFileDrop.claimed) return;
	e.preventDefault();
	if (!session.isAuthenticated) return;
	ui.notify(
		t(
			'resource_list.wrong_drop_zone_msg',
			'Uploads only work in Files — open the Files section and drop there.'
		),
		'warning',
		6000,
		true,
		{
			action: {
				label: t('resource_list.wrong_drop_zone_action', 'Go to Files'),
				// Best-effort recovery. The drop's `DataTransfer` is discarded
				// once this handler returns — browsers do not persist it across
				// a navigation — so the files cannot be forwarded. Landing the
				// user in `/files` is the most that can be offered without a
				// second drag.
				onClick: () => goto(resolve('/files'))
			}
		}
	);
}
