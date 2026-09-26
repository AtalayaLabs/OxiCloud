<script lang="ts">
	/**
	 * Admin-only policy editor for one drive.
	 *
	 * Extracted from the Drives tab when the compliance report on
	 * Admin › Drive Policies needed the same editor: a drift row says "this
	 * drive is laxer than its default", and the only useful next action is
	 * to correct the override. Navigating to another tab to do that lost the
	 * report; opening the same modal in place keeps it.
	 *
	 * Two callers, one component — the save path (diff against the opened
	 * values, PATCH only what moved, refresh the shared drive store) is
	 * identical for both and must not be written twice.
	 */
	import Modal from '$lib/components/Modal.svelte';
	import PolicyList from '$lib/components/PolicyList.svelte';
	import { getDrivePolicyDefaults } from '$lib/api/endpoints/admin';
	import { updateDrivePolicies } from '$lib/api/endpoints/drives';
	import { resolveOwnerName } from '$lib/api/endpoints/favorites';
	import { drives as drivesStore } from '$lib/stores/drives.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import { readAllPolicies } from '$lib/utils/drivePolicies';
	import type { Drive, DrivePolicies, DrivePoliciesPartial } from '$lib/api/types';

	interface Props {
		/** The drive to edit; `null` keeps the modal closed. */
		drive: Drive | null;
		onclose: () => void;
		/** Fired after a successful save with the drive's new EFFECTIVE
		 *  policies, so the caller can patch whatever list it is showing. */
		onsaved?: (driveId: string, merged: DrivePolicies) => void;
	}

	let { drive, onclose, onsaved }: Props = $props();

	let draft = $state<Required<DrivePoliciesPartial> | null>(null);
	let initial: Required<DrivePoliciesPartial> | null = null;
	let defaults = $state<Required<DrivePoliciesPartial> | null>(null);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let ownerName = $state<string | null>(null);

	/**
	 * Re-seed whenever a different drive is passed in.
	 *
	 * Keyed on the id rather than the object so a caller re-rendering its
	 * list — which hands over a fresh object for the same drive — does not
	 * throw away edits in progress.
	 */
	let seededFor: string | null = null;
	$effect(() => {
		const d = drive;
		if (!d) {
			seededFor = null;
			return;
		}
		if (seededFor === d.id) return;
		seededFor = d.id;

		const values = readAllPolicies((d.policies ?? {}) as Record<string, unknown>);
		draft = values;
		// Baseline for the save-time diff — see `save()`.
		initial = { ...values };
		error = null;

		// The kind's defaults, for the inherited-vs-overridden badges.
		// Fire-and-forget: a failure leaves the badges off rather than
		// blocking an edit that does not depend on them.
		defaults = null;
		void getDrivePolicyDefaults(d.kind).then(
			(def) => {
				if (seededFor === d.id)
					defaults = readAllPolicies(def as unknown as Record<string, unknown>);
			},
			() => {
				defaults = null;
			}
		);

		// A personal drive is named for its purpose, not its owner ("Personal"
		// in every row), so the title alone cannot say whose it is. The owner
		// is `default_for_user` — never the name (memory
		// `feedback_home_via_default_for_user`).
		ownerName = null;
		if (d.kind === 'personal' && d.default_for_user) {
			const uid = d.default_for_user;
			void resolveOwnerName(uid).then(
				(n) => {
					if (seededFor === d.id) ownerName = n;
				},
				() => {
					ownerName = null;
				}
			);
		}
	});

	const title = $derived.by(() => {
		if (!drive) return t('admin.drive_manage_policies', 'Manage policies');
		if (ownerName) {
			return t(
				'admin.drive_manage_policies_for_owner',
				{ name: drive.name, owner: ownerName },
				'Manage policies — {{name}} ({{owner}})'
			);
		}
		return t('admin.drive_manage_policies_for', { name: drive.name }, 'Manage policies — {{name}}');
	});

	async function save() {
		if (!drive || !draft || !initial) return;
		busy = true;
		error = null;
		try {
			// Send ONLY what the admin actually changed.
			//
			// The values shown are EFFECTIVE — the kind's default with this
			// drive's overrides on top — so submitting the whole draft would
			// write every knob as an explicit override and silently detach
			// the drive from its default. One edit would freeze every
			// setting at today's values, and the drive would stop following
			// any future default change without anyone having decided that.
			const changed: Record<string, unknown> = {};
			const base = initial as Record<string, unknown>;
			for (const [k, v] of Object.entries(draft)) {
				if (v !== base[k]) changed[k] = v;
			}
			const merged: DrivePolicies = await updateDrivePolicies(drive.id, changed);
			onsaved?.(drive.id, merged);
			// The shared `drivesStore` (feeds `/config/drive/{uuid}`, the
			// sidebar picker, the breadcrumb) caches `GET /api/drives` with
			// `loaded=true` after the first fetch — without this refresh the
			// policy change wouldn't reach those surfaces until a full page
			// reload.
			//
			// Fire-and-forget: the modal closes immediately; the picker
			// re-renders in place when the promise settles a few ms later.
			void drivesStore.refresh();
			onclose();
		} catch (e) {
			error = errorMessage(e);
		} finally {
			busy = false;
		}
	}
</script>

<!-- Toggles for the known policy keys; unknown keys on the JSONB bag are
     preserved by the backend merge but not surfaced here (forward-compat
     is at the server). Save → PATCH /api/drives/{id}/policies. -->
<Modal open={drive !== null} {title} {onclose} size="lg">
	{#if drive && draft}
		<div class="form">
			<p class="muted">
				{t(
					'admin.drive_manage_policies_help',
					'Policies are admin-only — drive owners cannot mutate them. Each toggle controls one enforcement gate.'
				)}
			</p>
			<PolicyList
				values={draft}
				{busy}
				compareTo={defaults}
				testIdPrefix="admin-policy"
				onchange={(key, next) => {
					// `next` is widened to boolean|number|null by the day-cap
					// knob; the draft's per-key type is narrower, and only
					// PolicyList knows which control produced the value.
					(draft as Record<string, unknown>)[key] = next;
				}}
			/>
			{#if error}
				<p class="status--error">{error}</p>
			{/if}
		</div>
	{/if}
	{#snippet footer()}
		<button
			class="btn"
			data-testid="admin-manage-policies-cancel-btn"
			onclick={onclose}
			disabled={busy}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<button
			class="btn btn-primary"
			data-testid="admin-manage-policies-save-btn"
			onclick={save}
			disabled={busy}
		>
			{busy ? t('common.saving', 'Saving…') : t('common.save', 'Save')}
		</button>
	{/snippet}
</Modal>
