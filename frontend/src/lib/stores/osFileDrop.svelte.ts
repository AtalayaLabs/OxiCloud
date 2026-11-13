import { untrack } from 'svelte';

/**
 * Does the mounted route handle OS file drops itself?
 *
 * The app-wide guard (`$lib/utils/osFileDropGuard`, wired on `window` in the
 * root layout) swallows an unhandled OS drop so the browser cannot navigate
 * away to the dropped file, and toasts "uploads only work in Files". Its
 * other protocol — stand down when `e.defaultPrevented` — covers any target
 * with its own handler: a folder row, a crumb, a sidebar drive, `.rl-root`.
 *
 * It does NOT cover `/files`'s own window-level handler, and that is a
 * genuine failure, not a nicety: `/files` accepts a drop anywhere on the
 * page, including the title strip and the margins around the list, by
 * listening on `window` as well. Both listeners are on the same object, so
 * the one registered first wins — and that is always the layout's, which
 * mounts before any page. A drop on the `/files` margin therefore got the
 * "wrong drop zone" toast while the page's handler, running second, saw
 * `defaultPrevented` and skipped the upload. Caught by
 * `tests/e2e/spa/upload-drop.spec.ts`'s two `drop:` cases, which drop on the
 * page container rather than on the list.
 *
 * Hence an explicit claim instead of a listener-ordering race: a route that
 * owns OS drops says so, and the guard leaves them entirely alone — no
 * `preventDefault`, no toast. The owner then carries the guard's whole
 * contract, `preventDefault` included.
 *
 * A counter, not a boolean: during a route change the incoming page's
 * `$effect` can run before the outgoing one's teardown, and a boolean would
 * end up false with a page still mounted.
 */
let claims = $state(0);

export const osFileDrop = {
	/** Is a mounted route handling OS file drops itself? */
	get claimed(): boolean {
		return claims > 0;
	},

	/**
	 * Claim ownership of OS file drops; returns the release function, so the
	 * caller is an `$effect` that just returns it:
	 *
	 *     $effect(() => osFileDrop.claim());
	 *
	 * Idempotent on release — calling it twice does not decrement twice.
	 */
	claim(): () => void {
		untrack(() => {
			claims += 1;
		});
		let released = false;
		return () => {
			if (released) return;
			released = true;
			untrack(() => {
				claims = Math.max(0, claims - 1);
			});
		};
	}
};
