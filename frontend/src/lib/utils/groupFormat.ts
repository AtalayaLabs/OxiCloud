/**
 * Group (ReBAC) display helpers.
 *
 * Pure UI/i18n helpers; no network. Lives in `utils/` so the Phase 4
 * retirement of `$lib/api/endpoints/groups.ts` doesn't drag the
 * display logic with it. The `VIRTUAL_*_KEYS` tables stay module-
 * private — virtual groups are a closed set that the FE and server
 * both ship, so flipping a key here means re-running `npm run build`
 * either way.
 */
import { t } from '$lib/i18n/index.svelte';
import { INTERNAL_GROUP_ID, type GroupItem } from '$lib/api/endpoints/groups';

/**
 * Map of well-known virtual-group UUIDs → i18n key for a human-readable
 * display name. The server stores a bootstrap English name on these rows
 * (so a backend-only client sees something sensible); the FE prefers the
 * localized label.
 */
const VIRTUAL_NAME_KEYS: Record<string, string> = {
	[INTERNAL_GROUP_ID]: 'groups.virtual_internal_name'
};

/**
 * Map of well-known virtual-group UUIDs → i18n key for a human-readable
 * description. Virtual groups are server-seeded and their `description` column
 * carries internal schema notes; those are useful on the backend but should
 * not leak to the group list in the UI. The mapping below means those virtual
 * groups display a localized blurb instead of their raw `description`.
 */
const VIRTUAL_DESC_KEYS: Record<string, string> = {
	[INTERNAL_GROUP_ID]: 'groups.virtual_internal_explanation'
};

/**
 * Human-readable display name for a group row. Virtual (system-wide) groups
 * use a localized label via the well-known UUID mapping; user-defined groups
 * display their raw name. Ported from `components/groupDisplay.js`.
 */
export function groupDisplayName(group: GroupItem): string {
	if (group.is_virtual) {
		const key = VIRTUAL_NAME_KEYS[group.id];
		if (key) return t(key, group.name);
	}
	return group.name;
}

/**
 * Human-readable description for a group row. Virtual groups render a localized
 * blurb (via the well-known UUID mapping) so their internal DB schema notes
 * never leak to the UI; a virtual group without a mapped key shows nothing.
 * User-defined groups display their raw `description` verbatim.
 */
export function groupDescription(group: GroupItem): string | null {
	if (group.is_virtual) {
		const key = VIRTUAL_DESC_KEYS[group.id];
		return key ? t(key, group.name) : null;
	}
	return group.description ?? null;
}

/**
 * Pick the icon registry name for a group avatar. Virtual (system-wide)
 * groups use `people-roof`; user-defined groups use `user-group`. Ported from
 * `components/groupDisplay.js`.
 */
export function groupIconName(group: Pick<GroupItem, 'is_virtual'>): string {
	return group.is_virtual ? 'people-roof' : 'user-group';
}
