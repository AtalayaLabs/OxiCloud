/**
 * UI preferences store — typed view over `session.user.ui_preferences`.
 *
 * The bag itself lives on the server (`auth.users.ui_preferences` JSONB
 * column), so it persists across devices without any localStorage
 * ceremony. This store just:
 *   • hydrates typed reactive fields from `session.user.ui_preferences`
 *     whenever the session changes,
 *   • debounces user-driven writes and PATCHes them back with a shallow
 *     merge,
 *   • rolls back on network failure and surfaces a toast.
 *
 * # Adding a new preference
 *
 * 1. Add a field to `UiPreferences` below with its type + default.
 * 2. Add a getter/setter pair (see `hideDotfiles` for the pattern).
 * 3. That's it. No backend changes — the server treats the bag as
 *    opaque JSON.
 *
 * If a preference ever needs to influence server behaviour (locale did),
 * promote it to a typed column on `auth.users` in a follow-up
 * migration and drop it from this bag.
 */
import { updateProfile } from '$lib/api/endpoints/profile';
import { session } from '$lib/stores/session.svelte';
import { ui } from '$lib/stores/ui.svelte';
import { t } from '$lib/i18n/index.svelte';

/**
 * Typed shape of the SPA-known keys inside `ui_preferences`. The bag
 * itself is `Record<string, unknown>` on the wire — this interface is
 * the SPA's contract with its own future self. Unknown keys are
 * preserved by the shallow merge; obsolete keys are silently ignored
 * on read.
 */
export interface UiPreferences {
	/**
	 * Hide files/folders whose name starts with a dot (Unix-style hide
	 * convention). Default `false` — show everything. Cross-platform
	 * hide is name-based only; Windows HIDDEN attribute is not
	 * preserved on upload, matching Nextcloud / ownCloud / Seafile.
	 */
	hide_dotfiles?: boolean;
	// NOTE: view_mode (grid/list) DELIBERATELY stays in localStorage
	// (`oxi-view-mode` on `filesStore`). Making it server-persistent
	// caused a real Playwright regression: `favorites.spec.ts` clicks
	// the list-view toggle, and on the server-backed store that
	// preference would then leak into every downstream test's fresh
	// browser context — Playwright's default context isolation
	// relies on localStorage being fresh per test, which the server
	// bag can't provide. Result: files-extra's `Zip-*` folder fell
	// outside list view's smaller virtualisation window (~25 vs ~75
	// grid items) and `getByTestId` timed out. Google Drive / Finder
	// / Dropbox also keep view mode per-device — the sync-across-
	// devices UX isn't a strongly-requested pattern.
}

/** Reasonable default for an empty bag or a missing key. */
const DEFAULTS: Required<UiPreferences> = {
	hide_dotfiles: false
};

/**
 * Milliseconds to wait after the last local mutation before PATCHing.
 * Fires under fast successive toggles (keyboard shortcut, mis-click,
 * settings-page checkbox drag) and coalesces into one wire write.
 */
const PATCH_DEBOUNCE_MS = 500;

class PreferencesStore {
	/**
	 * The typed view of the bag. Derived from `session.me?.ui_preferences`
	 * (moved from public `User.ui_preferences` to `SelfUser.ui_preferences`
	 * as part of the three-layer UserDto refactor — the bag is self-only
	 * state, not something other authenticated callers should see).
	 * Signing in / out / refresh flips it in lockstep with the session.
	 * Reads pass through DEFAULTS for any missing key.
	 */
	private bag = $derived<Record<string, unknown>>(
		(session.me?.ui_preferences as Record<string, unknown> | undefined) ?? {}
	);

	// ── Typed accessors ──────────────────────────────────────────

	hideDotfiles = $derived<boolean>(
		typeof this.bag.hide_dotfiles === 'boolean'
			? (this.bag.hide_dotfiles as boolean)
			: DEFAULTS.hide_dotfiles
	);

	// ── Mutations ─────────────────────────────────────────────────

	private patchTimer: ReturnType<typeof setTimeout> | null = null;
	private pendingPatch: Record<string, unknown> = {};

	/**
	 * Apply one or more key updates. Optimistic: the in-memory
	 * `session.user.ui_preferences` is updated synchronously so the UI
	 * flips right away; the wire PATCH is debounced. On PATCH failure,
	 * we roll back to the last server-observed bag and toast.
	 *
	 * A value of `null` deletes the key server-side (mirrors the SQL
	 * `jsonb_strip_nulls` after the merge).
	 */
	set(patch: Partial<Record<keyof UiPreferences, unknown>>): void {
		if (!session.me) return;

		// Optimistic local write — mutate the reactive me shallowly.
		// `ui_preferences` lives on `SelfUser` (self-only), not on the
		// public `User` slice, so the mutation stays at the SelfUser
		// level. The nested `full` / `full.user` blocks are untouched.
		const nextBag = {
			...((session.me.ui_preferences as Record<string, unknown> | undefined) ?? {}),
			...patch
		};
		// Strip any explicit-null locally so the derived getters see the
		// same shape the server will end up with. Server's
		// `jsonb_strip_nulls` handles the persisted side; this keeps
		// UI in sync between optimistic write and confirmation.
		for (const [k, v] of Object.entries(patch)) {
			if (v === null) delete (nextBag as Record<string, unknown>)[k];
		}
		session.me = { ...session.me, ui_preferences: nextBag };

		// Accumulate keys so successive `set` calls before the debounce
		// fires collapse into a single PATCH body — matters for
		// mass-toggle sequences (e.g. bulk settings-page save).
		this.pendingPatch = { ...this.pendingPatch, ...patch };

		if (this.patchTimer !== null) clearTimeout(this.patchTimer);
		this.patchTimer = setTimeout(() => this.flush(), PATCH_DEBOUNCE_MS);
	}

	private async flush(): Promise<void> {
		this.patchTimer = null;
		const patch = this.pendingPatch;
		this.pendingPatch = {};
		if (Object.keys(patch).length === 0) return;

		// `session.user` is a derived read-through on `session.me.full.user`
		// — the source of truth is `session.me: SelfUser`. Snapshot + assign
		// there so the optimistic update / rollback matches the store shape
		// (see `docs/plan/userdto-refactor.md` for the layering).
		const previousMe = session.me;
		try {
			const updated = await updateProfile({ ui_preferences: patch });
			session.me = updated;
		} catch {
			// Roll back to whatever the server last confirmed. The
			// optimistic local mutation is discarded and the derived
			// `hideDotfiles` / other getters snap back on the next
			// reactivity tick.
			session.me = previousMe;
			ui.notify(
				t('preferences.save_failed', "Couldn't save your preference. Please try again."),
				'error'
			);
		}
	}

	// ── Convenience wrappers ─────────────────────────────────────

	setHideDotfiles(value: boolean): void {
		this.set({ hide_dotfiles: value });
	}

	toggleHideDotfiles(): void {
		this.setHideDotfiles(!this.hideDotfiles);
	}
}

export const preferences = new PreferencesStore();
