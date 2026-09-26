/**
 * Shared drive-policy definitions.
 *
 * Consumed by two surfaces:
 *   - Admin "Manage policies" modal (`routes/admin/+page.svelte`) — read+write.
 *   - Drive settings page (`routes/config/drive/[uuid]/+page.svelte`) — read-only,
 *     so drive members can see which policies an admin has set.
 *
 * Kept in a plain `.ts` module (not a component) so both consumers import the
 * same array and the definition of "one policy" lives in exactly one place.
 * Adding a sixth policy is a single push here + one migration + the
 * `DrivePolicies` interface extension in `types.ts`. See
 * `docs/plan/drive.md` §8 (forbid_* gates) + §15 (include_in_*_index scope).
 */
import { t } from '$lib/i18n/index.svelte';
import type { DrivePoliciesPartial } from '$lib/api/types';

/**
 * `impliedBy` captures the semantic dependency between policies: when the
 * named parent policy is on, this subordinate gate is moot (its enforcement
 * is already covered by the broader rule). The admin modal disables the
 * child toggle and shows `impliedHint` so the admin understands the
 * hierarchy without our having to mutate the stored value — their
 * preference is preserved for the moment they relax the parent. The
 * read-only config surface uses the same signal to dim implied rows.
 */
export interface PolicyDef {
	key: keyof Required<DrivePoliciesPartial>;
	label: () => string;
	help: () => string;
	impliedBy?: keyof Required<DrivePoliciesPartial>;
	impliedHint?: () => string;
	/**
	 * Control shape. Every policy was a checkbox until `max_public_link_days`
	 * arrived; declaring the kind here keeps `PolicyList` switching on one
	 * field instead of every consumer special-casing a knob by name.
	 * Defaults to `'bool'` when absent.
	 */
	control?: 'bool' | 'days';
	/**
	 * Can this knob be set as a per-kind DEFAULT?
	 *
	 * `read_only` cannot: it is an operational state — freeze this drive, for
	 * this reason, usually for a duration — not a standing posture. A default
	 * that froze every drive from one toggle has no legitimate use, and the
	 * drift finding it would produce ("writable while the default says
	 * frozen") is noise rather than a problem. The admin defaults cards render
	 * it as an explicit exclusion rather than hiding it, so nobody wonders
	 * whether it was forgotten. Mirrors `NON_DEFAULTABLE_KNOBS` in
	 * `src/domain/entities/drive.rs`.
	 */
	defaultable?: false;
	/**
	 * Drive kinds the knob has no meaning for.
	 *
	 * `forbid_owner_role_change` is moot on a personal drive: membership there
	 * is immutable via a hardcoded guard that fires before any policy is read,
	 * so the flag changes nothing either way. Comparing it would produce drift
	 * findings no admin can act on. Mirrors `knob_applies_to_kind` in
	 * `src/domain/entities/drive.rs`.
	 */
	notApplicableTo?: ReadonlyArray<'personal' | 'shared'>;
}

/** Default when a def omits `control`. */
export function policyControl(def: PolicyDef): 'bool' | 'days' {
	return def.control ?? 'bool';
}

/** Whether a knob may appear on the admin defaults card for a kind. */
export function isDefaultable(def: PolicyDef, kind: 'personal' | 'shared'): boolean {
	return def.defaultable !== false && !(def.notApplicableTo ?? []).includes(kind);
}

/**
 * Order is the READING order of the panel, not the entity field order in
 * `src/domain/entities/drive.rs` — the two diverged when the public-link
 * knobs were grouped. `forbid_public_links` and its two refinements sit
 * together because they answer questions about the same object, and both
 * refinements grey out when the parent is on; splitting them would leave
 * a dependency the reader has to infer.
 *
 * A new policy is still one literal-array push, placed wherever it reads
 * best.
 */
export const policyDefs: PolicyDef[] = [
	{
		key: 'forbid_sharing',
		label: () => t('admin.drive_policy.forbid_sharing', 'Forbid per-resource sharing'),
		help: () =>
			t(
				'admin.drive_policy.forbid_sharing_help',
				'Block per-file / per-folder grants (covers public links and external sharing as well). Drive-level membership still works.'
			)
	},
	{
		key: 'forbid_public_links',
		label: () => t('admin.drive_policy.forbid_public_links', 'Forbid public links'),
		help: () =>
			t(
				'admin.drive_policy.forbid_public_links_help',
				'Block anonymous share links on resources in this drive.'
			),
		impliedBy: 'forbid_sharing',
		impliedHint: () =>
			t(
				'admin.drive_policy.implied_by_forbid_sharing',
				'Already enforced by Forbid per-resource sharing.'
			)
	},
	// The two refinements of `forbid_public_links` sit directly beneath
	// it: all three answer questions about the same object — may a link
	// exist, how long may it live, and may it be handed out without a
	// secret. Both grey out when the parent is on, so keeping them
	// adjacent makes the dependency visible instead of implied.
	{
		key: 'max_public_link_days',
		control: 'days',
		label: () => t('admin.drive_policy.max_public_link_days', 'Cap public link lifetime'),
		help: () =>
			t(
				'admin.drive_policy.max_public_link_days_help',
				'Longest a public link in this drive may live, in days. Leave empty for no cap. Links created beyond the cap are refused; links that already outlive it keep working and are reported by the policy compliance scan.'
			),
		impliedBy: 'forbid_public_links',
		impliedHint: () =>
			t(
				'admin.drive_policy.implied_by_forbid_public_links',
				'Already enforced by Forbid public links.'
			)
	},
	{
		key: 'require_public_link_password',
		label: () =>
			t('admin.drive_policy.require_public_link_password', 'Require a password on public links'),
		help: () =>
			t(
				'admin.drive_policy.require_public_link_password_help',
				'Public links in this drive must carry a password. Links created without one are refused; existing password-less links keep working and are reported by the policy compliance scan.'
			),
		impliedBy: 'forbid_public_links',
		impliedHint: () =>
			t(
				'admin.drive_policy.implied_by_forbid_public_links',
				'Already enforced by Forbid public links.'
			)
	},
	{
		key: 'forbid_external_sharing',
		label: () => t('admin.drive_policy.forbid_external_sharing', 'Forbid external sharing'),
		help: () =>
			t(
				'admin.drive_policy.forbid_external_sharing_help',
				'Block grants to external users (email invitations and pre-existing external accounts).'
			),
		impliedBy: 'forbid_sharing',
		impliedHint: () =>
			t(
				'admin.drive_policy.implied_by_forbid_sharing',
				'Already enforced by Forbid per-resource sharing.'
			)
	},
	{
		key: 'forbid_cross_drive_move',
		label: () => t('admin.drive_policy.forbid_cross_drive_move', 'Forbid cross-drive move'),
		help: () =>
			t(
				'admin.drive_policy.forbid_cross_drive_move_help',
				'Block moving files or folders out to another drive. Does not stop download + re-upload.'
			)
	},
	{
		key: 'forbid_owner_role_change',
		label: () => t('admin.drive_policy.forbid_owner_role_change', 'Lock Owner roster'),
		help: () =>
			t(
				'admin.drive_policy.forbid_owner_role_change_help',
				'Only admin can add, remove, or demote drive Owners while this is on.'
			),
		// Membership on a personal drive is immutable regardless — the guard
		// fires before any policy is read.
		notApplicableTo: ['personal']
	},
	{
		key: 'include_in_photo_index',
		label: () => t('admin.drive_policy.include_in_photo_index', 'Include in Photos'),
		help: () =>
			t(
				'admin.drive_policy.include_in_photo_index_help',
				'Show image and video files from this drive in the Photos timeline and on the Places map. Default personal drives are opted in automatically; turn on for shared drives that genuinely hold photos (e.g. "Family Photos").'
			)
	},
	{
		key: 'include_in_music_index',
		label: () => t('admin.drive_policy.include_in_music_index', 'Include in Music'),
		help: () =>
			t(
				'admin.drive_policy.include_in_music_index_help',
				'Include audio files from this drive in the Music library. Default personal drives are opted in automatically; turn on for shared drives that genuinely hold a music collection (e.g. "Family Music", "Band Collaboration").'
			)
	},
	{
		key: 'read_only',
		label: () => t('admin.drive_policy.read_only', 'Read-only (freeze)'),
		help: () =>
			t(
				'admin.drive_policy.read_only_help',
				'Freeze the drive entirely — every mutation is refused (uploads, edits, deletes, renames, sharing, membership changes). Reads and downloads keep working. The trash-retention janitor also pauses. Use for archives, legal holds, or account wind-downs. Only an admin can un-freeze.'
			),
		// Operational state, not a standing posture — see `defaultable`.
		defaultable: false
	}
];

/**
 * True when `def` is subordinate to another policy whose value is currently
 * `true` in `values`. Both surfaces use this to gray out implied rows.
 */
export function isPolicyImplied(def: PolicyDef, values: Required<DrivePoliciesPartial>): boolean {
	// Walks the WHOLE chain, not one link of it.
	//
	// `forbid_sharing` covers public links, which in turn cover the two
	// link refinements — so with `forbid_sharing` on, the cap and the
	// password requirement are moot even though `forbid_public_links`
	// may still read `false` in the stored values. Checking a single
	// level would leave them enabled and imply an admin could still
	// change something that no longer has any effect.
	//
	// `=== true` rather than truthiness: the value map now also carries
	// the day-cap number, and a non-zero cap must never read as "this
	// parent is on".
	const seen = new Set<string>();
	let parent = def.impliedBy;
	while (parent != null && !seen.has(parent)) {
		if (values[parent] === true) return true;
		seen.add(parent);
		parent = policyDefs.find((d) => d.key === parent)?.impliedBy;
	}
	return false;
}

/**
 * JSONB reader — the backend may hold a raw `Record<string, unknown>` bag
 * (unknown keys preserved verbatim), so any missing / non-bool key resolves
 * to `false`. Shared between the admin modal (initialising the edit draft)
 * and the config/drive page (reading the current state for display).
 */
export function readPolicyBool(p: Record<string, unknown>, key: string): boolean {
	const v = p[key];
	return typeof v === 'boolean' ? v : false;
}

/**
 * Day-cap reader. `null` means no cap — deliberately distinct from `0`,
 * which would mean "expire immediately". Anything non-numeric or negative
 * reads as no cap, matching `readPolicyBool`'s lenient contract: a
 * malformed bag must not break the editor.
 */
export function readPolicyDays(p: Record<string, unknown>, key: string): number | null {
	const v = p[key];
	return typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.floor(v) : null;
}

/**
 * Populate a full `Required<DrivePoliciesPartial>` from the JSONB bag by
 * reading each known key with `readPolicyBool`. Both admin and config
 * surfaces call this on load; the admin edits the returned object in
 * place while the config surface renders it read-only.
 */
export function readAllPolicies(p: Record<string, unknown>): Required<DrivePoliciesPartial> {
	const out = {} as Required<DrivePoliciesPartial>;
	for (const def of policyDefs) {
		if (policyControl(def) === 'days') {
			// Narrow per control kind rather than per key name, so a second
			// scalar knob needs no change here.
			(out as Record<string, unknown>)[def.key] = readPolicyDays(p, def.key);
		} else {
			(out as Record<string, unknown>)[def.key] = readPolicyBool(p, def.key);
		}
	}
	return out;
}
