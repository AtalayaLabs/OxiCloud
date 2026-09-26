<script lang="ts">
	/**
	 * Reusable list-of-policy-toggles.
	 *
	 * Consumed by:
	 *   - Admin "Manage policies" modal — `readonly=false`, admin edits the
	 *     bound `values` in place.
	 *   - Drive settings page (`/config/drive/{uuid}`) — `readonly=true`,
	 *     drive members read the currently-in-effect state.
	 *
	 * The shared `policyDefs` in `$lib/utils/drivePolicies` is the single
	 * source of truth for label + help text + implied-by relations. Adding
	 * a policy is one push there + one row in `DrivePolicies` in
	 * `types.ts`; the two consuming surfaces update automatically.
	 */
	import type { DrivePoliciesPartial } from '$lib/api/types';
	import { t } from '$lib/i18n/index.svelte';
	import {
		isPolicyImplied,
		policyControl,
		policyDefs,
		type PolicyDef
	} from '$lib/utils/drivePolicies';

	interface Props {
		/** Current values displayed on each row. */
		values: Required<DrivePoliciesPartial>;
		/** `true` = display only, disables the checkboxes so members can see the
		 *  live state without a mutation affordance. When `true`, `onchange`
		 *  is ignored — the component never emits. */
		readonly?: boolean;
		/** Additional disable signal (used by the admin modal during save). */
		busy?: boolean;
		/** Prefix for the `data-testid` on each checkbox
		 *  (e.g. `admin-policy-…` on the admin page, `drive-policy-…` on
		 *  the config page). Keeps test selectors stable per surface. */
		testIdPrefix?: string;
		/** Fired when the user toggles a checkbox (mutable surface only).
		 *  The parent owns the storage and applies the change. Not called
		 *  in `readonly` mode. */
		onchange?: (key: PolicyDef['key'], next: boolean | number | null) => void;
		/**
		 * Render only the knobs that can be a per-kind DEFAULT, for the given
		 * kind. Omit for the per-drive surfaces, which show everything.
		 *
		 * Excluded knobs are still listed, greyed, with the reason — an admin
		 * seeing a gap wonders whether it was forgotten, whereas a disabled
		 * row with "set per drive" answers the question on the spot.
		 */
		defaultsForKind?: 'personal' | 'shared';
		/**
		 * The kind's defaults, for a PER-DRIVE surface.
		 *
		 * `values` shows the effective policy, which is the right thing to
		 * enforce against but hides where each value came from: a drive
		 * following its default and one that deliberately decided the same
		 * thing look identical. Passing the defaults lets each row say
		 * which it is — and, when they differ, what the default was.
		 *
		 * That matters because the two behave differently in future: an
		 * inherited knob follows the next change to the default, an
		 * overridden one does not.
		 *
		 * Omit on the admin defaults cards, where the values ARE the
		 * defaults and the comparison is meaningless.
		 */
		compareTo?: Required<DrivePoliciesPartial> | null;
	}

	let {
		values,
		readonly = false,
		busy = false,
		testIdPrefix = 'policy',
		onchange,
		defaultsForKind,
		compareTo = null
	}: Props = $props();

	/**
	 * Human-readable value, for the "default was X" hint.
	 *
	 * The day-cap needs words rather than a bare number: an empty field
	 * means NO cap, and rendering that as blank beside "default:" would
	 * read as missing data instead of as the value it is.
	 */
	function describe(def: PolicyDef, v: unknown): string {
		if (policyControl(def) === 'days') {
			return typeof v === 'number' && v > 0
				? t('admin.drive_policy.n_days', { n: v }, '{{n}} days')
				: t('admin.drive_policy.no_cap', 'no cap');
		}
		return v === true ? t('admin.drive_policy.on', 'on') : t('admin.drive_policy.off', 'off');
	}

	/** The default for this knob, when it differs from what is in force. */
	function overriddenFrom(def: PolicyDef): string | null {
		if (!compareTo) return null;
		const mine = values[def.key];
		const theirs = compareTo[def.key];
		return mine === theirs ? null : describe(def, theirs);
	}

	/** Why a row is disabled, or null when it is editable. */
	function excludedReason(def: PolicyDef): string | null {
		if (!defaultsForKind) return null;
		if (def.defaultable === false) return 'not_defaultable';
		if ((def.notApplicableTo ?? []).includes(defaultsForKind)) return 'not_applicable';
		return null;
	}
</script>

<ul class="policy-list">
	{#each policyDefs as def (def.key)}
		{@const implied = isPolicyImplied(def, values)}
		{@const excluded = excludedReason(def)}
		{@const locked = readonly || busy || implied || excluded !== null}
		{@const overridden = overriddenFrom(def)}
		<li
			class="policy-row"
			class:policy-row--implied={implied || excluded !== null}
			class:policy-row--overridden={overridden !== null}
		>
			<label class="policy-row__label">
				<span class="policy-row__head">
					{#if policyControl(def) === 'days'}
						<!-- Empty means no cap, which is NOT the same as 0 (expire
						     immediately). `min=1` keeps that distinction typable. -->
						<input
							type="number"
							class="policy-row__days"
							min="1"
							step="1"
							placeholder="—"
							data-testid={`${testIdPrefix}-${def.key}`}
							value={typeof values[def.key] === 'number' ? values[def.key] : ''}
							disabled={locked}
							onchange={(e) => {
								const raw = (e.currentTarget as HTMLInputElement).value.trim();
								const n = raw === '' ? null : Number(raw);
								onchange?.(def.key, n !== null && Number.isFinite(n) && n > 0 ? n : null);
							}}
						/>
					{:else}
						<input
							type="checkbox"
							data-testid={`${testIdPrefix}-${def.key}`}
							checked={values[def.key] === true}
							disabled={locked}
							onchange={(e) => onchange?.(def.key, (e.currentTarget as HTMLInputElement).checked)}
						/>
					{/if}
					<span class="policy-row__title">{def.label()}</span>
					{#if overridden !== null}
						<!-- Names the value it departs from, not just "changed".
						     "Overridden" alone tells an admin something is
						     different; saying the default was `no cap` tells
						     them what they would get back by clearing it. -->
						<span
							class="policy-row__badge"
							data-testid={`${testIdPrefix}-${def.key}-overridden`}
							title={t(
								'admin.drive_policy.overridden_title',
								'Set on this drive. It will not follow future changes to the default.'
							)}
						>
							{t(
								'admin.drive_policy.overridden',
								{ value: overridden },
								'overridden · default: {{value}}'
							)}
						</span>
					{/if}
				</span>
				<span class="policy-row__help muted">
					{def.help()}
					{#if def.impliedHint}
						<!-- Always in the layout, hidden rather than removed.
						     Toggling the parent gate flips several of these at
						     once, and adding/removing nodes would reflow every
						     row below — the list would jump under the cursor of
						     someone mid-click. `visibility` keeps the box. -->
						<span class="policy-row__implied" class:is-hidden={!implied}>
							{def.impliedHint()}
						</span>
					{/if}
					{#if defaultsForKind}
						<!-- Always rendered on the defaults cards, hidden when the
						     knob IS defaultable. The Personal and Shared cards sit
						     side by side showing the same knob list, and a note
						     that appears on one but not the other pushes every
						     later row down on that side — the two columns stop
						     lining up and the same knob no longer sits at the same
						     height. Reserving the line keeps them in step. -->
						<span
							class="policy-row__implied"
							class:is-hidden={excluded === null}
							data-testid={`${testIdPrefix}-${def.key}-excluded`}
						>
							{excluded === 'not_defaultable'
								? t(
										'admin.drive_policies.not_defaultable',
										'Set per drive — this is an operational state, not a default.'
									)
								: t('admin.drive_policies.not_applicable', 'Does not apply to this kind of drive.')}
						</span>
					{/if}
				</span>
			</label>
		</li>
	{/each}
</ul>

<style>
	/* Ported from the admin modal's original block so the visual stays
	   identical when the modal switches to this component; the read-only
	   surface on `/config/drive/{uuid}` gets the same look for free. */
	.policy-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.policy-row {
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
	}

	.policy-row__label {
		/* Column layout: head (checkbox + title inline) on top, help
		   text underneath. The checkbox + title share a row via
		   `.policy-row__head` so the title sits beside the checkbox
		   instead of wrapping to its own line. */
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		cursor: pointer;
		margin: 0;
	}

	.policy-row__head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}

	.policy-row__head input[type='checkbox'] {
		margin: 0;
		flex-shrink: 0;
	}

	.policy-row__title {
		font-weight: 600;
	}

	.policy-row__help {
		/* Indent the help text under the title so the relationship is
		   visually obvious. Width = checkbox width + the head's gap. */
		padding-left: calc(1rem + var(--space-2));
	}

	/* Implied state — the row's gate is already covered by a broader
	   policy (e.g. forbid_public_links when forbid_sharing is on).
	   Visually dimmed so the admin understands they don't need to
	   toggle it; the stored value is preserved for the moment they
	   relax the parent policy. Same treatment used on the read-only
	   surface so subordinate rules read as visually secondary. */
	.policy-row--implied {
		opacity: 0.55;
	}

	.policy-row--implied .policy-row__label {
		cursor: not-allowed;
	}

	.policy-row__implied {
		display: block;
		margin-top: var(--space-1);
		font-style: italic;
	}

	/* Hidden but still occupying its box. Flipping a parent gate toggles
	   several of these at once; removing them from flow would reflow
	   every row below and move the list under the pointer. */
	.policy-row__implied.is-hidden {
		visibility: hidden;
	}

	/* Marks a knob this drive decided for itself, rather than inherited.
	   Deliberately quiet — it is context, not a warning: overriding is a
	   legitimate thing to have done. */
	.policy-row__badge {
		margin-left: var(--space-2);
		padding: 0 0.4em;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm, 4px);
		font-size: 0.8em;
		font-style: normal;
		color: var(--color-text-muted);
		white-space: nowrap;
	}
</style>
