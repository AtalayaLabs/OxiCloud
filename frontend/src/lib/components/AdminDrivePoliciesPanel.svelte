<script lang="ts">
	/**
	 * Admin › Drive Policies.
	 *
	 * Two independent default cards — Personal and Shared — plus the
	 * compliance reports that a default change makes relevant.
	 *
	 * Both cards render the same `policyDefs` array the per-drive editor
	 * uses, which is what guarantees the defaults cover every knob including
	 * ones added later: one push to that array and the knob appears here, in
	 * the per-drive modal, and on the read-only drive view together.
	 *
	 * Changes save as you make them. There is no review-then-confirm step:
	 * it existed to forecast the blast radius, and the drift list below now
	 * shows the real one live, a moment after the toggle. Seeing which
	 * drives did not follow beats predicting how many would.
	 *
	 * The two lists below are deliberately different in kind:
	 *
	 *   - **Drift** is computed live on every load. Two small tables, no
	 *     joins, and being live is what makes it correct — a drive leaves
	 *     the list the instant its override is fixed.
	 *   - **Share and grant violations** come from the
	 *     `drive_policies_consistency` job, because those are real joins
	 *     over tables that grow, and the findings are worth keeping.
	 */
	import { t } from '$lib/i18n/index.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import PolicyList from '$lib/components/PolicyList.svelte';
	import {
		getDrivePolicyDefaults,
		getDrivePolicyDrift,
		listAllDrives,
		setDrivePolicyDefaults
	} from '$lib/api/endpoints/admin';
	import DrivePoliciesModal from '$lib/components/DrivePoliciesModal.svelte';
	import { listFindings, listRuns, triggerJob } from '$lib/api/endpoints/adminJobs';
	import {
		isDefaultable,
		policyControl,
		policyDefs,
		readAllPolicies,
		type PolicyDef
	} from '$lib/utils/drivePolicies';
	import type { Drive, DrivePoliciesPartial, Finding, PolicyDrift } from '$lib/api/types';

	/** The scan behind the share/grant report. Drift does not come from it. */
	const JOB = 'drive_policies_consistency';

	type Kind = 'personal' | 'shared';
	const KINDS: Kind[] = ['personal', 'shared'];

	interface KindState {
		draft: Required<DrivePoliciesPartial>;
		loaded: boolean;
		busy: boolean;
		error: string | null;
		saved: boolean;
	}

	function emptyState(): KindState {
		return {
			draft: readAllPolicies({}),
			loaded: false,
			busy: false,
			error: null,
			saved: false
		};
	}

	let states = $state<Record<Kind, KindState>>({
		personal: emptyState(),
		shared: emptyState()
	});

	let findings = $state<Finding[] | null>(null);
	let findingsError = $state<string | null>(null);
	let scanning = $state(false);

	/** Live drift — `null` until the first load. */
	let drift = $state<PolicyDrift[] | null>(null);
	let driftError = $state<string | null>(null);

	/** Scroll target for the pinned summary. */
	let driftCardEl = $state<HTMLElement | null>(null);

	function showDrift() {
		// Smooth unless the reader has asked for less motion. `scrollIntoView`
		// takes its behaviour from the option, not from the CSS media query,
		// so the preference has to be read here.
		const still = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
		driftCardEl?.scrollIntoView({ behavior: still ? 'auto' : 'smooth', block: 'start' });
	}

	/**
	 * Only knobs the admin actually set travel to the server.
	 *
	 * Omitting a key is how "no default for this" is expressed, and the
	 * backend REPLACES rather than merges — so sending every key with its
	 * falsy value would turn "unset" into a deliberate `false` for the whole
	 * estate, which is precisely the drift the reports exist to catch.
	 */
	function toBag(draft: Required<DrivePoliciesPartial>, kind: Kind): Record<string, unknown> {
		const bag: Record<string, unknown> = {};
		for (const def of policyDefs) {
			if (!isDefaultable(def, kind)) continue;
			const v = draft[def.key];
			if (policyControl(def) === 'days') {
				if (typeof v === 'number' && v > 0) bag[def.key] = v;
			} else if (v === true) {
				bag[def.key] = true;
			}
		}
		return bag;
	}

	async function load(kind: Kind) {
		try {
			const current = await getDrivePolicyDefaults(kind);
			states[kind].draft = readAllPolicies(current as unknown as Record<string, unknown>);
			states[kind].loaded = true;
		} catch (e) {
			states[kind].error = errorMessage(e);
			states[kind].loaded = true;
		}
	}

	/**
	 * Save on change, rather than behind a review-then-confirm step.
	 *
	 * The two-step existed to forecast the blast radius before committing.
	 * It is no longer needed: the drift list below is computed live, so the
	 * admin sees the actual consequence — which drives did not follow — a
	 * moment after the toggle, on the same screen. Showing what happened
	 * beats predicting what would, and there is no modal in between.
	 *
	 * Safe to do so because a default is cheap to reverse: toggling back
	 * restores it, and enforcement is creation-time, so nothing already
	 * granted is revoked by the write itself.
	 *
	 * Debounced because `PUT` REPLACES the whole bag. Two clicks in quick
	 * succession would otherwise race, and the loser would silently undo
	 * the winner. Coalescing to one write of the current state makes the
	 * order irrelevant.
	 */
	const SAVE_DEBOUNCE_MS = 600;
	const timers: Record<Kind, ReturnType<typeof setTimeout> | null> = {
		personal: null,
		shared: null
	};

	function scheduleSave(kind: Kind) {
		states[kind].saved = false;
		states[kind].error = null;
		if (timers[kind]) clearTimeout(timers[kind]);
		timers[kind] = setTimeout(() => void save(kind), SAVE_DEBOUNCE_MS);
	}

	async function save(kind: Kind) {
		states[kind].busy = true;
		states[kind].error = null;
		try {
			const stored = await setDrivePolicyDefaults(kind, toBag(states[kind].draft, kind));
			states[kind].draft = readAllPolicies(stored as unknown as Record<string, unknown>);
			states[kind].saved = true;
			// The whole point of saving immediately: the consequence appears
			// right below, without the admin asking for it.
			await loadDrift();
		} catch (e) {
			states[kind].error = errorMessage(e);
		} finally {
			states[kind].busy = false;
		}
	}

	/**
	 * Findings from the most recent run of the compliance scan.
	 *
	 * Reuses the generic `Finding` shape the jobs panel already renders, so
	 * both reports needed no new wire types.
	 */
	async function loadFindings() {
		findingsError = null;
		try {
			// Findings hang off a RUN, so the most recent run has to be
			// resolved first. `listRuns` returns newest-first.
			const runs = await listRuns(JOB, 1);
			const runId = runs[0]?.id;
			if (!runId) {
				// Never scanned. Empty rather than an error — "no findings yet"
				// and "no problems" look the same here, and the Run button is
				// right there.
				findings = [];
				return;
			}
			findings = await listFindings(JOB, runId, { limit: 500 });
		} catch (e) {
			findingsError = errorMessage(e);
		}
	}

	async function rescan() {
		scanning = true;
		findingsError = null;
		try {
			await triggerJob(JOB);
			// The run is asynchronous; give it a beat before reading, and let
			// the explicit Refresh cover a slow scan rather than polling.
			await new Promise((r) => setTimeout(r, 1200));
			await loadFindings();
		} catch (e) {
			findingsError = errorMessage(e);
		} finally {
			scanning = false;
		}
	}

	/**
	 * Drives laxer than their kind's default — live, on arrival.
	 *
	 * Not a scan result. This is `storage.drives` against two rows of
	 * defaults, cheap enough to read on every page load, and reading it live
	 * is what keeps it honest: a job finding is a snapshot, so a drive stayed
	 * on the report after the admin had already fixed it.
	 */
	async function loadDrift() {
		driftError = null;
		try {
			drift = await getDrivePolicyDrift();
		} catch (e) {
			driftError = errorMessage(e);
		}
	}

	/** Everything the scan reports is now a share or grant violation — drift
	 *  left this job and is computed live, so no filtering is needed. */
	const shareFindings = $derived(findings ?? []);

	$effect(() => {
		for (const k of KINDS) if (!states[k].loaded) void load(k);
		if (drift === null) void loadDrift();
		if (findings === null) void loadFindings();
	});

	function knobLabel(key: string): string {
		const def: PolicyDef | undefined = policyDefs.find((d) => d.key === key);
		return def ? def.label() : key;
	}

	function detailOf(f: Finding): Record<string, unknown> {
		return (f.detail ?? {}) as Record<string, unknown>;
	}

	/**
	 * The drive whose policies are being edited, opened from either
	 * weaker-than-default list.
	 *
	 * The editor opens HERE rather than navigating to the Drives tab: the
	 * reason to be reading these lists is to decide whether to correct an
	 * override, and sending the admin to another tab loses the list that
	 * told them to. `/config/drive/{uuid}` is not an option either — it
	 * shows policies read-only.
	 */
	let policyDrive = $state<Drive | null>(null);
	let policyDriveError = $state<string | null>(null);

	/**
	 * Resolve a drive id to the full row the editor needs.
	 *
	 * The lists carry only id + name (findings) or id + name + knobs
	 * (preview); the modal needs `kind` and the raw `policies` bag. Fetched
	 * on demand rather than held: this is a click on one row, not a list
	 * render, so one admin-scoped call at the moment of need is cheaper than
	 * keeping every drive in memory for a panel that usually shows none.
	 */
	async function openDrivePolicies(driveId: string): Promise<void> {
		policyDriveError = null;
		try {
			const all = await listAllDrives();
			const found = all.find((d) => d.id === driveId);
			if (!found) {
				// Deleted since the scan ran — say so rather than opening an
				// empty editor.
				policyDriveError = t(
					'admin.drive_policies.drive_gone',
					'That drive no longer exists — re-run the scan.'
				);
				return;
			}
			policyDrive = found;
		} catch (e) {
			policyDriveError = errorMessage(e);
		}
	}
</script>

<!--
	One row shape for both weaker-than-default lists.

	The preview answers "what would this default do" and the scan answers
	"what is true now" — different questions, so both stay. But they were
	reporting the same verdict at different fidelity: the scan named the
	drive, the preview gave a bare count. Rendering both through this snippet
	is what keeps them from drifting apart again.
-->
{#snippet weakerRow(id: string | null, name: string | null, knobs: string[])}
	<li>
		{#if id}
			<button
				type="button"
				class="dp__finding-link"
				onclick={() => openDrivePolicies(id)}
				data-testid={`admin-policy-drift-${id}`}
			>
				{name ?? id}
			</button>
			<span class="muted"> — {knobs.map(knobLabel).join(', ')}</span>
		{:else}
			<strong>{name ?? '—'}</strong>
			<span class="muted"> — {knobs.map(knobLabel).join(', ')}</span>
		{/if}
	</li>
{/snippet}

<section class="dp">
	<p class="muted">
		{t(
			'admin.drive_policies.hint',
			'Defaults apply to every drive of that kind. A drive follows the default for each setting it has not explicitly overridden, so tightening one here reaches the whole estate except where someone deliberately decided otherwise.'
		)}
	</p>

	<div class="dp__cards">
		{#each KINDS as kind (kind)}
			<div class="card dp__card" data-testid={`admin-policy-defaults-${kind}`}>
				<h2>
					{kind === 'personal'
						? t('admin.drive_policies.personal', 'Personal drives')
						: t('admin.drive_policies.shared', 'Shared drives')}
				</h2>

				{#if !states[kind].loaded}
					<p class="status">{t('common.loading', 'Loading…')}</p>
				{:else}
					<!-- Deliberately NOT disabled while saving. Under auto-save a
					     write is in flight after every toggle, so binding `busy`
					     here would grey the whole list on each click. The write
					     replaces the bag wholesale and is debounced, so a toggle
					     landing mid-flight is safe — the next save carries it. -->
					<PolicyList
						values={states[kind].draft}
						testIdPrefix={`admin-default-${kind}`}
						defaultsForKind={kind}
						onchange={(key, next) => {
							(states[kind].draft as Record<string, unknown>)[key] = next;
							scheduleSave(kind);
						}}
					/>

					{#if states[kind].error}
						<p class="status--error">{states[kind].error}</p>
					{/if}
					<!-- Always in the layout, never appearing and disappearing.
					     This line changes on every toggle, and a node that comes
					     and goes would shift the knob list under the pointer of
					     someone working through several in a row. -->
					<p
						class="dp__savestate"
						class:is-hidden={!states[kind].busy && !states[kind].saved}
						data-testid={`admin-policy-savestate-${kind}`}
						aria-live="polite"
					>
						{states[kind].busy
							? t('common.saving', 'Saving…')
							: t(
									'admin.drive_policies.saved',
									'Saved — applied to every drive of this kind that has not overridden it.'
								)}
					</p>
				{/if}
			</div>
		{/each}
	</div>

	<div class="card" bind:this={driftCardEl}>
		<h2>
			{t(
				'admin.drive_policies.drift_title',
				{ n: drift?.length ?? 0 },
				'Drives less restrictive than their default ({{n}})'
			)}
		</h2>
		<p class="muted">
			{t(
				'admin.drive_policies.drift_hint',
				'Only a drive with an explicit override can appear here — anything left unset follows the default by construction. So this is a list of deliberate decisions to review, and it updates as soon as one is corrected.'
			)}
		</p>

		{#if driftError}
			<p class="status--error">{driftError}</p>
		{:else if drift === null}
			<p class="status">{t('common.loading', 'Loading…')}</p>
		{:else if drift.length === 0}
			<p class="status">
				{t(
					'admin.drive_policies.drift_none',
					'None — every drive is at least as strict as its default.'
				)}
			</p>
		{:else}
			<ul class="dp__findings" data-testid="admin-policy-drift">
				{#each drift as d (d.id)}
					{@render weakerRow(d.id, d.name, d.knobs)}
				{/each}
			</ul>
		{/if}
	</div>

	<div class="card">
		<h2>{t('admin.drive_policies.reports', 'Compliance')}</h2>
		<p class="muted">
			{t(
				'admin.drive_policies.reports_hint',
				'Policies are checked when something is created, never retroactively — so tightening one never breaks a link that already exists. This scan is how those become visible. It only reports; nothing is changed.'
			)}
		</p>

		<div class="dp__actions">
			<button
				class="btn"
				type="button"
				disabled={scanning}
				data-testid="admin-policy-rescan"
				onclick={rescan}
			>
				{scanning
					? t('admin.drive_policies.scanning', 'Scanning…')
					: t('admin.drive_policies.rescan', 'Run the scan')}
			</button>
			<button class="btn" type="button" onclick={loadFindings}>
				{t('common.refresh', 'Refresh')}
			</button>
		</div>

		{#if findingsError}
			<p class="status--error">{findingsError}</p>
		{:else if findings === null}
			<p class="status">{t('common.loading', 'Loading…')}</p>
		{:else}
			<h3 class="dp__sub">
				{t(
					'admin.drive_policies.shares_title',
					{ n: shareFindings.length },
					'Shares that violate their drive policy ({{n}})'
				)}
			</h3>
			{#if shareFindings.length === 0}
				<p class="status">
					{t(
						'admin.drive_policies.shares_none',
						'None — every link and grant matches its drive policy.'
					)}
				</p>
			{:else}
				<ul class="dp__findings" data-testid="admin-policy-shares">
					{#each shareFindings as f (f.id)}
						{@const d = detailOf(f)}
						<li>
							<strong>{(d.token_name as string) ?? f.resource_id ?? '—'}</strong>
							<span class="muted">
								— {(d.drive_name as string) ?? ''}
								{#if f.kind === 'share_missing_required_password'}
									· {t('admin.drive_policies.v_no_password', 'no password')}
								{:else if f.kind === 'share_outlives_policy_cap'}
									· {d.never_expires
										? t('admin.drive_policies.v_never_expires', 'never expires')
										: t('admin.drive_policies.v_over_cap', 'outlives the cap')}
								{:else}
									· {knobLabel(d.knob as string)}
								{/if}
							</span>
						</li>
					{/each}
				</ul>
			{/if}
		{/if}
	</div>

	{#if policyDriveError}
		<p class="status--error">{policyDriveError}</p>
	{/if}

	<!--
		The drift list sits below two tall cards of toggles, so the admin
		editing a default cannot see the consequence of the edit — which is
		the whole reason the list is live. This pins a one-line summary to
		the bottom of the viewport so the count is visible from the toggles.

		Rendered ONLY when something has actually drifted. A bar permanently
		reading "(0)" would spend screen space on the healthy case, which is
		the usual one; appearing on the first drift is also what makes it
		read as a consequence of the toggle just made.
	-->
	{#if drift && drift.length > 0}
		<div class="dp__pinned" data-testid="admin-policy-drift-pinned">
			<span aria-live="polite">
				{t(
					'admin.drive_policies.drift_pinned',
					{ n: drift.length },
					'{{n}} drive(s) less restrictive than their default'
				)}
			</span>
			<button class="btn" type="button" onclick={showDrift}>
				{t('admin.drive_policies.drift_review', 'Review')}
			</button>
		</div>
	{/if}
</section>

<DrivePoliciesModal
	drive={policyDrive}
	onclose={() => (policyDrive = null)}
	onsaved={() => {
		// The lists were computed by a scan that ran before this edit, so
		// they now describe a state that no longer holds. Re-read rather
		// than leave a row claiming a drift the admin just fixed.
		void loadFindings();
	}}
/>

<style>
	/*
	 * Spacing between the panel's top-level sections.
	 *
	 * `.card` is NOT inherited here: the admin route defines one, but Svelte
	 * scopes it to that route's own markup, so these sections arrive with no
	 * margin of their own and the defaults, the drift list and the
	 * compliance report ran into each other. Only the spacing is set —
	 * `> .card` so the two kind cards inside `.dp__cards` keep taking their
	 * separation from the grid gap instead of stacking two rules.
	 */
	.dp > .card {
		margin-bottom: var(--space-6);
	}

	.dp > .card h2 {
		margin-top: 0;
		margin-bottom: var(--space-2);
	}

	.dp__cards {
		display: grid;
		grid-template-columns: 1fr;
		gap: 1rem;
		margin-bottom: var(--space-6);
	}

	@media (width >= 60rem) {
		.dp__cards {
			grid-template-columns: 1fr 1fr;
		}
	}

	.dp__actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
		margin-top: 0.75rem;
	}

	.dp__sub {
		margin: 1rem 0 0.35rem;
		font-size: 1rem;
	}

	.dp__findings {
		margin: 0;
		padding-left: 1.1rem;
	}

	.dp__findings li {
		margin-bottom: 0.2rem;
	}

	/* `--color-accent` is tuned for fills (buttons, bars), and reading it as
	   text on this panel's grey surface is genuinely hard. `--color-accent-text`
	   is the contrast-adjusted variant for exactly this, and the underline
	   carries the affordance without relying on colour at all. */
	.dp__finding-link {
		background: none;
		border: 0;
		padding: 0;
		font: inherit;
		color: var(--color-accent-text);
		text-decoration: underline;
		cursor: pointer;
	}

	.dp__finding-link:hover,
	.dp__finding-link:focus-visible {
		color: var(--color-accent-hover);
	}

	.dp__savestate {
		margin: var(--space-2) 0 0;
		font-size: 0.875rem;
		color: var(--color-text-muted);
	}

	/* Hidden but still holding its box. Under auto-save this line appears on
	   every toggle, and removing it from flow would shift the knobs below it
	   under the pointer of someone working through several in a row. */
	.dp__savestate.is-hidden {
		visibility: hidden;
	}

	/* Pinned to the bottom of the viewport while the panel is scrolled, so
	   the drift count stays readable from the toggles that cause it.
	   `sticky` rather than `fixed`: it belongs to this panel and should
	   settle into place at the end of it, not float over the rest of admin. */
	.dp__pinned {
		position: sticky;
		bottom: 0;
		z-index: 1;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		margin-top: var(--space-3);
		padding: var(--space-2) var(--space-3);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
	}
</style>
