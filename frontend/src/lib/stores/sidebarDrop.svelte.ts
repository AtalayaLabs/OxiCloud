import { untrack } from 'svelte';

/**
 * Which sidebar row, if any, is armed as the destination of the drag in
 * flight — a nav link (keyed by href) or a drive (keyed by drive id, which
 * cannot collide with an href).
 *
 * Shared state rather than two `$state` locals in `AppShell` for two
 * reasons:
 *
 *   * Exactly ONE thing in the sidebar may be armed at a time. With a field
 *     per kind that is a convention; with one field it is the data model.
 *   * A PAGE has to know. `/files` frames the viewport and highlights the
 *     current folder's crumb to say "this drop lands here" — both of which
 *     are wrong the moment the pointer is over a drive, because the drop
 *     lands THERE instead. The page cannot see `AppShell`'s locals, and the
 *     drag events it listens to at window level fire for the sidebar too.
 */
let armedKey = $state<string | null>(null);

export const sidebarDrop = {
	/** Key of the armed row (href or drive id), or null. */
	get key(): string | null {
		return armedKey;
	},

	/** Is any sidebar row claiming the current drag? */
	get armed(): boolean {
		return armedKey !== null;
	},

	/** Arm one row, disarming whatever held it. */
	arm(key: string): void {
		untrack(() => {
			armedKey = key;
		});
	},

	/**
	 * Disarm `key` — a no-op if something else has since armed, which is what
	 * makes the dragenter/dragleave crossfire between adjacent rows settle on
	 * the row the pointer actually ended on.
	 */
	release(key: string): void {
		untrack(() => {
			if (armedKey === key) armedKey = null;
		});
	},

	/** Disarm unconditionally — the drag is over (drop / dragend / route change). */
	clear(): void {
		untrack(() => {
			armedKey = null;
		});
	}
};
