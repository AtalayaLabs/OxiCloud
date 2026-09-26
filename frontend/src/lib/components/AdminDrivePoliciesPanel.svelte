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
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { formatDate } from '$lib/utils/display';
	import PolicyList from '$lib/components/PolicyList.svelte';
	import {
		getDrivePolicyDefaults,
		getDrivePolicyDrift,
		listAllDrives,
		setDrivePolicyDefaults
	} from '$lib/api/endpoints/admin';
	import DrivePoliciesModal from '$lib/components/DrivePoliciesModal.svelte';
	import { resolveOwnerName } from '$lib/api/endpoints/favorites';
	import { SvelteMap } from 'svelte/reactivity';
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
	/** Drive id whose revocation is in flight, or null. */
	let revoking = $state<string | null>(null);

	/** Live drift — `null` until the first load. */
	let drift = $state<PolicyDrift[] | null>(null);
	let driftError = $state<string | null>(null);

	/**
	 * Every drive, by id — the report's vocabulary.
	 *
	 * Findings carry `drive_id` and little else about the drive, and a bare
	 * uuid names nothing to a human. One admin-scoped list turns every
	 * finding into something readable, and is also what `openDrivePolicies`
	 * resolves against rather than re-fetching per click.
	 */
	const drivesById = new SvelteMap<string, Drive>();
	/** Resolved owner display names, keyed by user id. */
	const ownerNames = new SvelteMap<string, string>();

	async function loadDrives() {
		try {
			const all = await listAllDrives();
			drivesById.clear();
			for (const d of all) drivesById.set(d.id, d);

			// Personal drives are ALL named "Personal", so the name identifies
			// nothing — the owner is the only thing that tells them apart.
			// Resolved once per owner, not once per finding.
			const owners = new Set(
				all
					.filter((d) => d.kind === 'personal' && d.default_for_user)
					.map((d) => d.default_for_user!)
			);
			for (const uid of owners) {
				if (ownerNames.has(uid)) continue;
				try {
					const n = await resolveOwnerName(uid);
					if (n) ownerNames.set(uid, n);
				} catch {
					// Leave it unresolved; the row falls back to the drive name.
				}
			}
		} catch {
			// Non-fatal: the report still renders, just with less context.
		}
	}

	/** How a drive should be named in a report heading. */
	function driveLabel(driveId: string | undefined): string {
		const d = driveId ? drivesById.get(driveId) : undefined;
		if (!d) return driveId ?? '—';
		if (d.kind === 'personal') {
			const owner = d.default_for_user ? ownerNames.get(d.default_for_user) : undefined;
			// "Personal — ed" rather than a twentieth row reading "Personal".
			return owner
				? t('admin.drive_policies.personal_of', { owner }, 'Personal — {{owner}}')
				: d.name;
		}
		return d.name;
	}

	function driveKindLabel(driveId: string | undefined): string {
		const d = driveId ? drivesById.get(driveId) : undefined;
		if (!d) return '';
		return d.kind === 'personal'
			? t('admin.drive_kind_personal', 'Personal')
			: t('admin.drive_kind_shared', 'Shared');
	}

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

	/**
	 * Delete the violations listed under one drive.
	 *
	 * Lives here rather than on the jobs panel because the job requires a
	 * `drive` and refuses without one — and this is the only screen that
	 * knows which drive the admin means. The jobs panel renders boolean
	 * parameters only, so its repair toggle would send no drive and be
	 * refused; that refusal is the misclick guard working, not a gap to
	 * route around.
	 *
	 * Confirmed first, with the count and the drive named. This revokes
	 * access and there is no undo, so the dialog states what will stop
	 * working rather than asking a generic "are you sure".
	 */
	async function revokeDriveShares(driveId: string, count: number) {
		// Two-button confirm, always — this withdraws access from real people
		// and cannot be undone, so it is never a single-click action. Both
		// buttons are labelled explicitly: a verb on the destructive one says
		// what will happen, which "Yes" does not.
		const ok = await confirmDialog({
			title: t(
				'admin.drive_policies.revoke_confirm_title',
				{ name: driveLabel(driveId) },
				'Revoke {{name}}’s non-compliant shares?'
			),
			message: t(
				'admin.drive_policies.revoke_confirm_body',
				{ n: count },
				'{{n}} link(s) and grant(s) on this drive will be revoked. Anyone using them loses access immediately, and this cannot be undone. Other drives are untouched.'
			),
			confirmText: t('admin.drive_policies.revoke_confirm', 'Revoke'),
			cancelText: t('common.cancel', 'Cancel'),
			danger: true
		});
		if (!ok) return;

		revoking = driveId;
		findingsError = null;
		try {
			const res = await triggerJob(JOB, { repair: true, drive: driveId });
			// The job reports a failed run in its body rather than as a non-2xx,
			// so a silent success here would be a lie.
			if (res.outcome?.outcome === 'err') {
				findingsError = res.outcome.message ?? 'repair failed';
			}
			await loadFindings();
		} catch (e) {
			findingsError = errorMessage(e);
		} finally {
			revoking = null;
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

	/**
	 * Findings grouped by the drive they belong to.
	 *
	 * Ungrouped, the report was a flat wall of rows that each repeated the
	 * same drive and the same violated knob — twenty lines of "Personal ·
	 * Forbid per-resource sharing" that said nothing about which drive or
	 * which file. Grouping states the drive once, as a heading, and leaves
	 * each row to say what is actually specific to it.
	 */
	const sharesByDrive = $derived.by(() => {
		// Plain object as the accumulator rather than a Map: the grouping is
		// local to this computation and never mutated once it escapes, and
		// `prefer-svelte-reactivity` cannot distinguish that from reactive
		// state that needs SvelteMap.
		const groups: Record<string, Finding[]> = {};
		for (const f of shareFindings) {
			const id = (detailOf(f).drive_id as string) ?? '';
			(groups[id] ??= []).push(f);
		}
		return Object.entries(groups)
			.map(([driveId, items]) => ({ driveId, items }))
			.sort((a, b) => driveLabel(a.driveId).localeCompare(driveLabel(b.driveId)));
	});

	/**
	 * What is being shared, as a row label.
	 *
	 * Deliberately never the finding's `resource_id`: for a link that is the
	 * share row's uuid and for a grant it is the grant's, neither of which
	 * means anything to a reader. When the scan gives no name — which is the
	 * case for user and group grants — the subject is more informative than
	 * an id, so the row leads with who it is shared WITH instead.
	 */
	function subjectLabel(f: Finding): string {
		const d = detailOf(f);
		const item = d.token_name as string | undefined;
		if (item) return item;

		const username = d.username as string | undefined;
		if (username) return username;
		if (d.subject_type === 'group') {
			return t('admin.drive_policies.a_group', 'a group');
		}
		return t('admin.drive_policies.unnamed_resource', 'unnamed');
	}

	/** The kind of thing shared — file, folder — when the scan says. */
	function resourceKind(f: Finding): string {
		const d = detailOf(f);
		const kind = (d.item_type as string) ?? (d.resource_type as string) ?? '';
		if (kind === 'file') return t('admin.drive_policies.r_file', 'file');
		if (kind === 'folder') return t('admin.drive_policies.r_folder', 'folder');
		return kind;
	}

	/**
	 * Why this row is a violation, in the reader's terms.
	 *
	 * Says WHAT is wrong with WHICH kind of thing, not just the missing
	 * attribute. "no password" left the reader to infer that the subject was
	 * a public link and that a password was required of it; "public link with
	 * no password" states it. The cap cases go further and name the numbers,
	 * because "outlives the cap" is a verdict while "expires 12 Mar 2027,
	 * past the 30-day cap" is the evidence for it — and the admin deciding
	 * whether to revoke the link needs the evidence.
	 */
	function violationLabel(f: Finding): string {
		const d = detailOf(f);
		if (f.kind === 'share_missing_required_password') {
			return t('admin.drive_policies.v_no_password', 'public link with no password');
		}
		if (f.kind === 'share_outlives_policy_cap') {
			const cap = typeof d.cap_days === 'number' ? d.cap_days : null;
			if (d.never_expires) {
				return cap === null
					? t('admin.drive_policies.v_never_expires', 'public link that never expires')
					: t(
							'admin.drive_policies.v_never_expires_cap',
							{ n: cap },
							'public link that never expires, past the {{n}}-day cap'
						);
			}
			const until = formatDate(d.expires_at as string | undefined);
			if (cap !== null && until) {
				return t(
					'admin.drive_policies.v_over_cap_detail',
					{ date: until, n: cap },
					'public link expiring {{date}}, past the {{n}}-day cap'
				);
			}
			return t('admin.drive_policies.v_over_cap', 'public link outliving the cap');
		}
		return knobLabel(d.knob as string);
	}

	/** For a grant, who holds it and with what role. */
	function grantSuffix(f: Finding): string | null {
		const d = detailOf(f);
		if (f.kind !== 'grant_violates_drive_policy') return null;
		const role = d.role as string | undefined;
		const external = d.is_external === true;
		const bits: string[] = [];
		if (role) bits.push(role);
		if (external) bits.push(t('admin.drive_policies.external', 'external'));
		return bits.length ? bits.join(' · ') : null;
	}

	$effect(() => {
		for (const k of KINDS) if (!states[k].loaded) void load(k);
		if (drift === null) void loadDrift();
		if (findings === null) void loadFindings();
		// The report names drives, so it needs them before it can render
		// anything but uuids.
		if (drivesById.size === 0) void loadDrives();
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
			// Served from the map the report already loaded; re-read only if a
			// click somehow lands before it.
			if (drivesById.size === 0) await loadDrives();
			const found = drivesById.get(driveId);
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
				{t('admin.jobs.refresh', 'Refresh')}
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
				<div data-testid="admin-policy-shares">
					{#each sharesByDrive as group (group.driveId)}
						<!--
							The drive is stated once, here, instead of being repeated on
							every row. Personal drives resolve to their owner because the
							name is "Personal" for all of them.

							Foldable, and collapsed by default: an account can hold many
							drives, and the first question this report answers is WHICH
							drives have violations — the headings and their counts answer
							that on their own. Expanding is for the drive you then decide
							to look at.

							`<details>` rather than a JS open/closed map: the browser owns
							the state, keyboard and screen-reader behaviour come for free,
							and nothing has to be kept in sync across a re-render.

							A single group opens itself — collapsing the only drive there
							is would be pure friction.
						-->
						<details class="dp__drive" open={sharesByDrive.length === 1}>
							<summary class="dp__drive-summary">
								{#if driveKindLabel(group.driveId)}
									<span class="dp__kind">{driveKindLabel(group.driveId)}</span>
								{/if}
								<span class="dp__drive-name">{driveLabel(group.driveId)}</span>
								<span class="dp__count">{group.items.length}</span>
								<!-- A trailing action rather than the row itself: the
								     summary's job is to expand, so editing gets its own
								     target. `preventDefault` stops the click toggling too. -->
								<button
									type="button"
									class="dp__finding-link dp__drive-edit"
									onclick={(e) => {
										e.preventDefault();
										e.stopPropagation();
										void openDrivePolicies(group.driveId);
									}}
									data-testid={`admin-policy-shares-drive-${group.driveId}`}
								>
									{t('admin.drive_manage_policies', 'Manage policies')}
								</button>
								<!-- Scoped repair. The job refuses to run without a drive,
								     and this is the only screen that knows which one the
								     admin means — the jobs panel renders boolean
								     parameters only, so its repair toggle cannot supply
								     one. Destructive, so it confirms first. -->
								<button
									type="button"
									class="btn dp__drive-revoke"
									disabled={revoking !== null}
									onclick={(e) => {
										e.preventDefault();
										e.stopPropagation();
										void revokeDriveShares(group.driveId, group.items.length);
									}}
									data-testid={`admin-policy-revoke-${group.driveId}`}
								>
									{revoking === group.driveId
										? t('admin.drive_policies.revoking', 'Revoking…')
										: t('admin.drive_policies.revoke', 'Revoke')}
								</button>
							</summary>
							<ul class="dp__findings">
								{#each group.items as f (f.id)}
									{@const suffix = grantSuffix(f)}
									<li>
										<strong>{subjectLabel(f)}</strong>
										<span class="muted">
											{#if resourceKind(f)}({resourceKind(f)}){/if}
											· {violationLabel(f)}
											{#if suffix}· {suffix}{/if}
										</span>
									</li>
								{/each}
							</ul>
						</details>
					{/each}
				</div>
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
		// Overriding a drive's policy changes DRIFT — whether that drive is
		// still laxer than its default — so the live drift list is what has
		// to be re-read. It was calling `loadFindings()`, which re-reads the
		// same finished job run and therefore could not change: a row the
		// admin had just fixed stayed on screen.
		//
		// The share/grant findings are deliberately NOT refreshed. They come
		// from a completed scan, and tightening a drive's policy can make
		// existing shares non-compliant without any scan having noticed yet —
		// that needs a re-run, which is the button above, not a silent
		// re-read that would return the same rows.
		void loadDrift();
		// The modal wrote new policies, so the cached drive rows the report
		// names things from are now stale.
		void loadDrives();
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

	/* One foldable block per drive, so the drive is stated once instead of
	   being repeated on every row. */
	.dp__drive {
		border-top: 1px solid var(--color-border);
	}

	.dp__drive:last-of-type {
		border-bottom: 1px solid var(--color-border);
	}

	.dp__drive-summary {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) 0;
		cursor: pointer;
		font-size: 0.9375rem;
		font-weight: 600;
	}

	/* Takes the free space so the count and the action sit at the far edge,
	   lining up down the list however long the names are. */
	.dp__drive-name {
		flex: 1 1 auto;
		min-width: 0;
		overflow-wrap: anywhere;
	}

	/* The number of violations on this drive — the reason to expand it, so
	   it has to be legible while collapsed. */
	.dp__count {
		flex: 0 0 auto;
		min-width: 1.75em;
		padding: 0 0.45em;
		border-radius: 999px;
		background: var(--color-bg-subtle, var(--color-bg-surface));
		border: 1px solid var(--color-border);
		font-size: 0.8125rem;
		font-weight: 400;
		text-align: center;
		color: var(--color-text-muted);
	}

	.dp__drive-edit {
		flex: 0 0 auto;
		font-size: 0.8125rem;
		font-weight: 400;
	}

	/* Destructive, and styled to look it — this withdraws access with no
	   undo, so it must not read as just another link beside "Manage
	   policies".
	   Background and border are BOTH set explicitly: the shared `.btn`
	   declares `border: none` and no background at all, so a bare button
	   fell back to the user agent's grey `buttonface` — and danger-coloured
	   text on that grey is the contrast problem. Setting only
	   `border-color` was inert for the same reason.
	   `--color-danger-alt` is the danger hue as TEXT; `--color-danger-text`
	   is white, meant as a foreground on a danger FILL, and would be
	   invisible here. */
	.dp__drive-revoke {
		flex: 0 0 auto;
		padding: 0.15rem 0.5rem;
		background: transparent;
		border: 1px solid var(--color-danger-alt);
		border-radius: var(--radius-md);
		color: var(--color-danger-alt);
		font-size: 0.8125rem;
		font-weight: 400;
	}

	.dp__drive-revoke:hover:not(:disabled) {
		background: var(--color-danger-light-bg);
	}

	.dp__drive-revoke:disabled {
		opacity: 0.55;
		cursor: not-allowed;
	}

	/* Indent the violations under the drive they belong to. */
	.dp__drive > .dp__findings {
		margin: 0 0 var(--space-3);
		padding-left: var(--space-4);
	}

	/* Personal vs Shared — the distinction changes what the name beside it
	   means, so it reads as a label rather than as part of the name. */
	.dp__kind {
		padding: 0 0.4em;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm, 4px);
		font-size: 0.75rem;
		font-weight: 400;
		color: var(--color-text-muted);
		white-space: nowrap;
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
