<!--
  AdminJobsPanel — the Jobs tab of the admin panel.

  Table of registered jobs, row-click expands to show run history for
  recoverable jobs, and within a run row a further expansion shows
  findings. Coordinator jobs (`consistency_batch`) get a "Run deep"
  variant that propagates `?deep=true` to every child. See
  `docs/plan/job-registry.md` §Admin UI for the mock this implements.

  Data flow:
  - `listJobs()` populates the table; auto-polled every 5s while the
    tab is visible so an operator watching a running job sees the
    `running` flag flip without manual refresh.
  - `triggerJob(name, {deep})` invoked by the "Run" / "Run deep" buttons.
  - `cancelJob(name)` invoked by "Cancel" (only shown when a run is
    currently running).
  - `listRuns(name)` lazy-loaded when a row is expanded.
  - `listFindings(name, runId)` lazy-loaded when a run is expanded.
-->
<script lang="ts">
	import { SvelteSet } from 'svelte/reactivity';
	import Icon from '$lib/icons/Icon.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import { ui } from '$lib/stores/ui.svelte';
	import {
		listJobs,
		listRuns,
		listFindings,
		pauseJob,
		triggerJob,
		cancelJob,
		purgeJobRuns
	} from '$lib/api/endpoints/adminJobs';
	import type { Finding, JobSummary, RunSummary, RunStatus } from '$lib/api/types';

	// ─── State ────────────────────────────────────────────────────────

	let jobs = $state<JobSummary[] | null>(null);
	let loadError = $state<string | null>(null);

	// One expanded job (by name) at a time — matches the plan mock
	// (drawer-style). Multiple simultaneous expansions would fight for
	// vertical space in the shared admin page and confuse the polling
	// story (runs list stale-detection).
	let expandedJob = $state<string | null>(null);
	let runsByJob = $state<Record<string, RunSummary[]>>({});
	let runsErrorByJob = $state<Record<string, string>>({});
	let runsLoadingByJob = $state<Record<string, boolean>>({});

	// One expanded run per job — same reasoning.
	let expandedRunByJob = $state<Record<string, string | null>>({});
	let findingsByRun = $state<Record<string, Finding[]>>({});
	let findingsErrorByRun = $state<Record<string, string>>({});
	let findingsLoadingByRun = $state<Record<string, boolean>>({});

	// Actions in flight — key by "trigger:<name>" / "cancel:<name>" so
	// buttons disable the moment they're clicked, no double-fires.
	// SvelteSet mutates in place with per-key reactivity — cheaper than
	// rebuilding a plain Set on every click, and required by the
	// svelte/prefer-svelte-reactivity lint.
	const busyKeys = new SvelteSet<string>();
	function markBusy(key: string, on: boolean) {
		if (on) busyKeys.add(key);
		else busyKeys.delete(key);
	}

	// Per-job "Run" split-button menu state. Keyed by job name so
	// two rows can open their menus independently (though the
	// outside-click handler below closes all on any click outside
	// any menu — matching the /files upload dropdown pattern). Only
	// rows with `supportsDeep` OR `supportsRepair` render a chevron;
	// the plain-Run rows (drives/folders/files/backend/… consistency,
	// trash_cleanup, dedup_gc, …) show a bare "Run" button with no
	// menu, keeping the common case one-click.
	let runMenuOpen = $state<Record<string, boolean>>({});
	function toggleRunMenu(name: string) {
		runMenuOpen = { ...runMenuOpen, [name]: !runMenuOpen[name] };
	}
	function closeAllRunMenus() {
		runMenuOpen = {};
	}
	// Global outside-click + Escape dismiss. Only registered while at
	// least one menu is open — a background admin tab doesn't hold
	// listeners.
	$effect(() => {
		const anyOpen = Object.values(runMenuOpen).some((v) => v);
		if (!anyOpen) return;
		const onDown = (e: MouseEvent) => {
			if (!(e.target as HTMLElement).closest('.jobs-panel__split')) {
				closeAllRunMenus();
			}
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') closeAllRunMenus();
		};
		window.addEventListener('pointerdown', onDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onDown);
			window.removeEventListener('keydown', onKey);
		};
	});

	// Purge-modal state. Null = closed; otherwise carries the
	// draft retention days the operator's picking. Kept separate
	// from the top-bar action state so mouse-away doesn't lose
	// the value.
	let purgeModal = $state<{ days: number } | null>(null);
	function openPurge() {
		purgeModal = { days: 30 };
	}
	function closePurge() {
		purgeModal = null;
	}
	async function confirmPurge() {
		if (!purgeModal) return;
		const days = Math.max(1, Math.floor(purgeModal.days));
		markBusy('purge', true);
		try {
			const res = await purgeJobRuns(days);
			ui.notify(
				t(
					'admin.jobs.purge_done',
					{ n: res.purged, days: res.retention_days },
					'{{n}} old run(s) purged (retention {{days}} days)'
				),
				'success'
			);
			purgeModal = null;
			await loadJobs();
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		} finally {
			markBusy('purge', false);
		}
	}

	// ─── Loading + polling ─────────────────────────────────────────────

	/**
	 * Stable render order for the jobs table. The backend snapshot
	 * iterates a HashMap so its order is non-deterministic —
	 * refreshing shuffles rows and hurts orientation.
	 *
	 * Two-tier sort:
	 *   0. `*_consistency` tenants — alphabetical.
	 *   1. All other jobs — alphabetical.
	 *
	 * The `consistency_batch` coordinator is DELIBERATELY excluded
	 * from the table (see the filter in `loadJobs`). The top-bar
	 * "Run all consistency checks" + "Run deep" buttons already
	 * dispatch it — showing it as a table row too was pure
	 * duplication.
	 */
	function sortKey(name: string): [number, string] {
		if (name.endsWith('_consistency')) return [0, name];
		return [1, name];
	}

	async function loadJobs() {
		try {
			const fetched = await listJobs();
			jobs = fetched
				.slice()
				// `consistency_batch` is served by the top-bar
				// action buttons; hiding it here removes the
				// duplicate table row. `batchJob` still reads from the
				// full fetched list so the top buttons only render
				// when the coordinator is actually registered.
				.filter((j) => j.name !== 'consistency_batch')
				.sort((a, b) => {
					const [ga, na] = sortKey(a.name);
					const [gb, nb] = sortKey(b.name);
					if (ga !== gb) return ga - gb;
					return na.localeCompare(nb);
				});
			// Track whether the coordinator is registered so the
			// top-bar buttons can gate on it without checking `jobs`
			// (which now filters it out).
			batchJob = fetched.find((j) => j.name === 'consistency_batch') ?? null;
			loadError = null;
		} catch (e) {
			loadError = errorMessage(e);
		}
	}

	// Poll while the tab is visible. Stops on component destroy AND on
	// tab-hidden so a background admin tab doesn't hammer the API.
	// 5s cadence — the plan's recommended default; a running job's
	// state flips within a poll interval.
	const POLL_MS = 5000;
	let pollTimer: ReturnType<typeof setInterval> | null = null;

	function startPolling() {
		stopPolling();
		pollTimer = setInterval(() => {
			if (document.visibilityState !== 'visible') return;
			void loadJobs();
			// Refresh runs for the expanded job too — the whole point of
			// having it expanded is watching progress. Findings we DON'T
			// refresh automatically: they only grow within a run, and
			// the operator can hit refresh manually when they want to
			// see new ones. Auto-refreshing findings during a big scan
			// would rewrite the scroll position under them.
			if (expandedJob) void loadRuns(expandedJob);
		}, POLL_MS);
	}

	function stopPolling() {
		if (pollTimer) {
			clearInterval(pollTimer);
			pollTimer = null;
		}
	}

	$effect(() => {
		void loadJobs();
		startPolling();
		return () => stopPolling();
	});

	async function loadRuns(name: string) {
		runsLoadingByJob = { ...runsLoadingByJob, [name]: true };
		runsErrorByJob = { ...runsErrorByJob, [name]: '' };
		try {
			const runs = await listRuns(name, 20);
			runsByJob = { ...runsByJob, [name]: runs };
		} catch (e) {
			runsErrorByJob = { ...runsErrorByJob, [name]: errorMessage(e) };
		} finally {
			runsLoadingByJob = { ...runsLoadingByJob, [name]: false };
		}
	}

	async function loadFindings(name: string, runId: string) {
		findingsLoadingByRun = { ...findingsLoadingByRun, [runId]: true };
		findingsErrorByRun = { ...findingsErrorByRun, [runId]: '' };
		try {
			const findings = await listFindings(name, runId, { limit: 500 });
			findingsByRun = { ...findingsByRun, [runId]: findings };
		} catch (e) {
			findingsErrorByRun = { ...findingsErrorByRun, [runId]: errorMessage(e) };
		} finally {
			findingsLoadingByRun = { ...findingsLoadingByRun, [runId]: false };
		}
	}

	// ─── Expansion toggles ─────────────────────────────────────────────

	function toggleJob(job: JobSummary) {
		if (expandedJob === job.name) {
			expandedJob = null;
		} else {
			expandedJob = job.name;
			// Lazy-load on first open, refresh on subsequent opens. Only
			// recoverable jobs have runs to load — the others expand purely
			// to show their description.
			if (isRecoverable(job)) void loadRuns(job.name);
		}
	}

	function toggleRun(jobName: string, runId: string) {
		const current = expandedRunByJob[jobName];
		if (current === runId) {
			expandedRunByJob = { ...expandedRunByJob, [jobName]: null };
		} else {
			expandedRunByJob = { ...expandedRunByJob, [jobName]: runId };
			void loadFindings(jobName, runId);
		}
	}

	// ─── Actions ───────────────────────────────────────────────────────

	async function onTrigger(name: string, opts: { deep?: boolean; repair?: boolean } = {}) {
		// Key suffix has to keep every dispatched variant distinct so the
		// button-disabled state of one doesn't lock out another mid-flight.
		const key = `trigger:${name}${opts.deep ? ':deep' : ''}${opts.repair ? ':repair' : ''}`;
		markBusy(key, true);
		try {
			// Fire the trigger + a follow-up loadJobs after a short delay
			// in parallel. Long jobs (backend_migration) come back 202
			// immediately; short jobs (consistency checks) come back on
			// completion. Either way, the `running` badge / Pause button
			// should appear within a render cycle rather than waiting
			// for the next 5s poll tick.
			const triggerPromise = triggerJob(name, opts);
			// Give the backend a moment to register the run's
			// `current_run_start` before we ask "is it running?" — this
			// races against the trigger acknowledgment for detached
			// jobs. 300 ms is well under the 5 s poll cadence and
			// invisible to the operator.
			setTimeout(() => {
				void loadJobs();
				if (expandedJob === name) void loadRuns(expandedJob);
			}, 300);

			const res = await triggerPromise;
			// The trigger envelope carries the child's outcome — surface
			// its pass/fail immediately so operators don't have to click
			// through to see whether the run completed cleanly. Detached
			// jobs come back with `dispatched: true` and no outcome —
			// silence the notify for those (the "started" state is
			// already visible via the badge).
			if (!res.outcome) {
				// dispatched (detached) — no outcome to render
			} else if (res.outcome.outcome === 'ok') {
				// Repair runs surface a rollup so the operator sees
				// whether corrective UPDATEs actually fired. `extra`
				// carries `repaired_count` on the two refcount tenants
				// directly, and nested under `per_check[*].extra` when
				// dispatched via `consistency_batch`. Sum across the
				// per_check dict if present, else read the top-level.
				let repairedTotal = 0;
				let sawRepair = false;
				const extra = (res.outcome.extra ?? {}) as {
					repair_requested?: boolean;
					repaired_count?: number;
					per_check?: Record<
						string,
						{ extra?: { repair_requested?: boolean; repaired_count?: number } }
					>;
				};
				if (extra.repair_requested) {
					sawRepair = true;
					repairedTotal += extra.repaired_count ?? 0;
				}
				if (extra.per_check) {
					for (const child of Object.values(extra.per_check)) {
						if (child?.extra?.repair_requested) {
							sawRepair = true;
							repairedTotal += child.extra.repaired_count ?? 0;
						}
					}
				}
				if (sawRepair) {
					ui.notify(
						t(
							'admin.jobs.triggered_ok_repair',
							{ name, n: repairedTotal },
							'{{name}}: {{n}} counter(s) repaired'
						),
						'success'
					);
				} else {
					ui.notify(
						t('admin.jobs.triggered_ok', { name }, '{{name}} triggered successfully'),
						'success'
					);
				}
			} else {
				ui.notify(
					t(
						'admin.jobs.triggered_err',
						{ name, message: res.outcome.message },
						'{{name}} failed: {{message}}'
					),
					'error'
				);
			}
			await loadJobs();
			if (expandedJob === name) await loadRuns(name);
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		} finally {
			markBusy(key, false);
		}
	}

	async function onPause(name: string) {
		const key = `pause:${name}`;
		markBusy(key, true);
		try {
			const res = await pauseJob(name);
			if (res.paused) {
				ui.notify(
					t(
						'admin.jobs.pause_requested',
						{ name },
						'Pause requested — {{name}} will pause at the next checkpoint (progress preserved)'
					),
					'info'
				);
			} else {
				ui.notify(
					t('admin.jobs.pause_noop', { name }, 'Nothing to pause — {{name}} is not running'),
					'info'
				);
			}
			await loadJobs();
			if (expandedJob === name) await loadRuns(name);
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		} finally {
			markBusy(key, false);
		}
	}

	async function onCancel(name: string) {
		// Terminal cancel confirmation — this is destructive (marks the
		// run as Cancelled, cursor preserved for post-mortem but not
		// resumable). Skip the confirm for non-recoverable jobs since
		// there's no persistent state to lose there today.
		if (
			!window.confirm(
				t(
					'admin.jobs.cancel_confirm',
					{ name },
					'Cancel run of {{name}}? The run will be marked as Cancelled and cannot be resumed. Progress bytes on disk stay put — this only affects the run row.'
				)
			)
		) {
			return;
		}
		const key = `cancel:${name}`;
		markBusy(key, true);
		try {
			const res = await cancelJob(name);
			if (res.cancelled) {
				ui.notify(
					t(
						'admin.jobs.cancel_requested',
						{ name },
						'Cancel requested — {{name}} will land in Cancelled at the next batch boundary (Paused rows flip immediately)'
					),
					'info'
				);
			} else {
				ui.notify(
					t(
						'admin.jobs.cancel_noop',
						{ name },
						'Nothing to cancel — {{name}} has no non-terminal run'
					),
					'info'
				);
			}
			await loadJobs();
			if (expandedJob === name) await loadRuns(name);
		} catch (e) {
			ui.notify(errorMessage(e), 'error');
		} finally {
			markBusy(key, false);
		}
	}

	// ─── Formatters (pure, no I/O) ─────────────────────────────────────

	function cadenceLabel(job: JobSummary): string {
		if (job.interval_ms === undefined) return t('admin.jobs.on_demand', 'on-demand');
		const secs = Math.round(job.interval_ms / 1000);
		if (secs % 3600 === 0) return t('admin.jobs.every_h', { n: secs / 3600 }, 'every {{n}} h');
		if (secs % 60 === 0) return t('admin.jobs.every_min', { n: secs / 60 }, 'every {{n}} min');
		return t('admin.jobs.every_sec', { n: secs }, 'every {{n}} s');
	}

	/**
	 * Per-severity finding counts from `last_outcome.extra.severity_counts`
	 * (a JSON object populated by `run_or_resume`). Missing / older
	 * runs return an empty record — callers should tolerate absent keys.
	 * Severity values emitted today: `data_loss`, `inconsistent`,
	 * `anomaly`. The set is open (the column is TEXT), so unknown keys
	 * must degrade rather than throw.
	 */
	function lastSeverityCounts(job: JobSummary): Record<string, number> {
		if (!job.last_outcome || job.last_outcome.outcome !== 'ok') return {};
		const extra = job.last_outcome.extra as
			| { severity_counts?: Record<string, number> }
			| undefined;
		return extra?.severity_counts ?? {};
	}

	/**
	 * Actionable findings = `data_loss + inconsistent`. Those are what
	 * turn the outer outcome pill amber ("issues") and get the red
	 * badge on the outer job row. `anomaly` findings are informational
	 * and render as a blue notice instead — they don't count here.
	 */
	function actionableFindingCount(job: JobSummary): number {
		const s = lastSeverityCounts(job);
		return (s.data_loss ?? 0) + (s.inconsistent ?? 0);
	}

	/**
	 * Informational findings. `anomaly` is the wire value; "notice" is
	 * what the panel calls it — there is no separate `notice` severity.
	 * A job that acted on what it found (a repair run deleting an
	 * orphaned sidecar) records the same severity and says so in the
	 * finding's `detail`.
	 */
	function anomalyFindingCount(job: JobSummary): number {
		return lastSeverityCounts(job).anomaly ?? 0;
	}

	/**
	 * Pill CSS modifier for a finding's severity — extracted so the
	 * findings-table cell and any future summary render share one
	 * source of truth.
	 *  - `data_loss` → red (`err`)
	 *  - `inconsistent` → amber (`paused`)
	 *  - `anomaly` → blue (`notice`)
	 *  - unknown → neutral grey
	 */
	function severityPillModifier(severity: string): string {
		switch (severity) {
			case 'data_loss':
				return 'err';
			case 'inconsistent':
				return 'paused';
			case 'anomaly':
				return 'notice';
			default:
				return 'neutral';
		}
	}

	function outcomeLabel(job: JobSummary): string {
		if (!job.last_outcome) return t('admin.jobs.never', 'never');
		if (job.last_outcome.outcome === 'ok') {
			// `ok` on the wire = dispatch completed. If any actionable
			// findings surfaced, we flip to "issues" (amber). If only
			// anomalies (informational), we flip to "notices" (blue).
			// Clean run stays green.
			if (actionableFindingCount(job) > 0) {
				return t('admin.jobs.outcome_issues', 'issues');
			}
			if (anomalyFindingCount(job) > 0) {
				return t('admin.jobs.outcome_notices', 'notices');
			}
			return t('admin.jobs.outcome_ok', 'ok');
		}
		return t('admin.jobs.outcome_err', 'err');
	}

	function outcomeClass(job: JobSummary): string {
		if (!job.last_outcome) return 'jobs-panel__pill jobs-panel__pill--neutral';
		if (job.last_outcome.outcome !== 'ok') {
			return 'jobs-panel__pill jobs-panel__pill--err';
		}
		if (actionableFindingCount(job) > 0) {
			return 'jobs-panel__pill jobs-panel__pill--paused';
		}
		if (anomalyFindingCount(job) > 0) {
			return 'jobs-panel__pill jobs-panel__pill--notice';
		}
		return 'jobs-panel__pill jobs-panel__pill--ok';
	}

	function statusClass(status: RunStatus): string {
		switch (status) {
			case 'Running':
				return 'jobs-panel__pill jobs-panel__pill--running';
			case 'Paused':
				return 'jobs-panel__pill jobs-panel__pill--paused';
			case 'CancelRequested':
				return 'jobs-panel__pill jobs-panel__pill--paused';
			case 'Completed':
				return 'jobs-panel__pill jobs-panel__pill--ok';
			case 'Failed':
				return 'jobs-panel__pill jobs-panel__pill--err';
			case 'Cancelled':
				return 'jobs-panel__pill jobs-panel__pill--neutral';
			default:
				return 'jobs-panel__pill jobs-panel__pill--neutral';
		}
	}

	/**
	 * Human-facing label for a `RunStatus`. Translates the internal
	 * DB status enum into text an operator can read at a glance —
	 * notably renders `CancelRequested` as "Pausing" for the
	 * recoverable-run case (the mechanism is a cancel flag, but the
	 * user intent is pause). Non-recoverable cancels aren't a thing
	 * today because non-recoverable jobs run to completion inline,
	 * so `CancelRequested` here is always the pause path.
	 */
	function statusLabel(status: RunStatus): string {
		switch (status) {
			case 'Running':
				return t('admin.jobs.status_running', 'Running');
			case 'Paused':
				return t('admin.jobs.status_paused', 'Paused');
			case 'CancelRequested':
				return t('admin.jobs.status_ending', 'Ending');
			case 'Completed':
				return t('admin.jobs.status_completed', 'Completed');
			case 'Failed':
				return t('admin.jobs.status_failed', 'Failed');
			case 'Cancelled':
				return t('admin.jobs.status_cancelled', 'Cancelled');
			default:
				return status;
		}
	}

	/** Coarse "3 min ago" / "2 h ago" — same shape as the parent
	 *  admin page's timeAgo(). Duplicated locally so the component
	 *  stays self-contained; extract if a third caller emerges. */
	function timeAgo(iso?: string): string {
		if (!iso) return t('admin.jobs.never', 'never');
		const then = new Date(iso).getTime();
		if (!Number.isFinite(then)) return '—';
		const secs = Math.round((Date.now() - then) / 1000);
		if (secs < 60) return t('admin.jobs.just_now', 'just now');
		const mins = Math.round(secs / 60);
		if (mins < 60) return t('admin.jobs.n_min_ago', { n: mins }, '{{n}} min ago');
		const hours = Math.round(mins / 60);
		if (hours < 24) return t('admin.jobs.n_h_ago', { n: hours }, '{{n}} h ago');
		const days = Math.round(hours / 24);
		return t('admin.jobs.n_d_ago', { n: days }, '{{n}} d ago');
	}

	function runDurationLabel(run: RunSummary): string {
		const start = new Date(run.started_at).getTime();
		const end = new Date(run.completed_at ?? run.last_progress_at).getTime();
		if (!Number.isFinite(start) || !Number.isFinite(end)) return '';
		const ms = Math.max(0, end - start);
		if (ms < 1000) return `${ms}ms`;
		if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
		const mins = Math.floor(ms / 60_000);
		const secs = Math.round((ms % 60_000) / 1000);
		return `${mins}m${secs}s`;
	}

	function statNumber(run: RunSummary, key: string): number | null {
		const v = run.stats?.[key];
		if (typeof v === 'number') return v;
		if (typeof v === 'string') {
			const n = Number(v);
			return Number.isFinite(n) ? n : null;
		}
		return null;
	}

	// Jobs that respect `?deep=true`:
	//   * `consistency_batch` — propagates deep to every child that
	//     understands it
	//   * `backend_consistency` — deep mode re-reads + re-hashes every
	//     matched blob for silent bit-rot detection (severity
	//     `data_loss`). Full read of storage; can take hours on big
	//     installs — the "Run" button on the same row does the
	//     enumeration merge-join only. This was `blobs_consistency`
	//     until that tenant became database-only.
	function supportsDeep(name: string): boolean {
		return name === 'consistency_batch' || name === 'backend_consistency';
	}

	// Whether `?repair=true` does anything for this job — declared by the
	// handler itself via `repair_description()`, not by a name allowlist
	// here. The allowlist this replaces named only the two ref_count
	// tenants and silently omitted every repair-capable job added since,
	// so the thumbnail imports could not be run in repair mode from the
	// panel at all despite supporting it.
	function supportsRepair(job: JobSummary): boolean {
		return !!job.repair_description;
	}

	// What the repair adds, in the handler's own words. The backend owns
	// this string precisely because the wording differs per job: correcting
	// a counter and unlinking files off disk are not the same warning, and
	// the frontend has no way to tell them apart.
	async function onTriggerWithRepairConfirm(job: JobSummary) {
		const ok = await confirmDialog({
			title: t(
				'admin.jobs.run_repair_confirm_title_scoped',
				{ name: job.name },
				'Run {{name}} in repair mode?'
			),
			message: job.repair_description ?? '',
			confirmText: t('admin.jobs.run_repair_confirm', 'Repair'),
			danger: true
		});
		if (ok) await onTrigger(job.name, { repair: true });
	}

	// Confirmation before a plain run of a job that writes. `never` jobs
	// trigger straight through — that is the point of the flag — and
	// `on_repair_only` jobs are read-only until the repair variant is
	// picked, which carries its own confirm.
	async function onTriggerGuarded(job: JobSummary) {
		if (job.mutates === 'always') {
			const ok = await confirmDialog({
				title: t('admin.jobs.run_mutating_confirm_title', { name: job.name }, 'Run {{name}}?'),
				message:
					job.description ||
					t('admin.jobs.run_mutating_confirm_body', 'This job changes stored state when it runs.'),
				confirmText: t('admin.jobs.run', 'Run'),
				danger: true
			});
			if (!ok) return;
		}
		await onTrigger(job.name);
	}

	// Row badge. `never` is the one worth stating outright — it is the
	// answer to "is it safe to click this on production?", and it is the
	// question an operator asks before every trigger.
	function mutatesLabel(job: JobSummary): string | null {
		switch (job.mutates) {
			case 'never':
				return t('admin.jobs.mutates_never', 'read-only');
			case 'on_repair_only':
				return t('admin.jobs.mutates_on_repair_only', 'read-only unless repaired');
			default:
				return null;
		}
	}

	function isRunning(job: JobSummary): boolean {
		return job.running;
	}

	function isRecoverable(job: JobSummary): boolean {
		// Backend authoritative source: the `recoverable` flag on
		// `JobSummary` is set at registration time by
		// `RecoverableAdapter::is_recoverable() -> true`. Every tenant
		// registered via `register_recoverable_job` flips it
		// automatically. No name-based allowlists — a new recoverable
		// tenant is expandable in the UI as soon as it's registered.
		return job.recoverable;
	}

	// Consistency batch shortcut — top button. Only shown when the
	// coordinator is registered (should always be true post-Slice 5,
	// but check defensively so the button doesn't appear on an old
	// deployment before this component is upgraded).
	// Held as the whole summary rather than a boolean because the
	// top-bar buttons need its `repair_description` — the coordinator
	// describes its own repair semantics, same as every table row.
	// Set imperatively in `loadJobs` because `jobs` no longer contains
	// the `consistency_batch` row (filtered out to avoid duplicating
	// the top-bar action buttons).
	let batchJob = $state<JobSummary | null>(null);
</script>

<section class="jobs-panel">
	<header class="jobs-panel__header">
		<div class="jobs-panel__header-text">
			<p class="jobs-panel__hint">
				{t(
					'admin.jobs.hint',
					'This section concerns internal background tasks: periodic + on-demand tasks. Consistency checks are safe to run at any time — they are read-only.'
				)}
			</p>
		</div>
		<div class="jobs-panel__header-actions">
			{#if batchJob}
				{@const batch = batchJob}
				<button
					class="jobs-panel__btn jobs-panel__btn--primary"
					disabled={busyKeys.has('trigger:consistency_batch')}
					title={batch.description || undefined}
					onclick={() => onTrigger('consistency_batch')}
				>
					<Icon name="play" />
					{t('admin.jobs.run_all_consistency', 'Run all consistency checks')}
				</button>
				<button
					class="jobs-panel__btn"
					disabled={busyKeys.has('trigger:consistency_batch:deep')}
					title={t(
						'admin.jobs.run_deep_hint',
						'Also runs slow variants (blob re-hash, bitrot detection).'
					)}
					onclick={() => onTrigger('consistency_batch', { deep: true })}
				>
					<Icon name="play" />
					{t('admin.jobs.run_deep', 'Run deep')}
				</button>
				<!-- Repair goes behind a confirm because it fans `?repair=true`
				     out to every sub-check that acts on it. The confirmation
				     text comes from the coordinator's own
				     `repair_description` rather than being written here —
				     what repair means changes as tenants gain repair arms,
				     and this button would otherwise keep describing only the
				     two refcount ones. -->
				{#if batch.repair_description}
					<button
						class="jobs-panel__btn jobs-panel__btn--warn"
						disabled={busyKeys.has('trigger:consistency_batch:repair')}
						title={batch.repair_description}
						onclick={() => onTriggerWithRepairConfirm(batch)}
					>
						<Icon name="cog" />
						{t('admin.jobs.run_repair', 'Repair ref_counts')}
					</button>
				{/if}
			{/if}
			<!-- Purge is orthogonal to consistency — it works even
			     when the batch coordinator isn't registered, so it
			     lives outside the batch block. Opens a modal so
			     the operator picks a retention window with intent
			     (no accidental delete-all). -->
			<button
				class="jobs-panel__btn"
				title={t(
					'admin.jobs.purge_hint',
					'Delete completed and failed run history older than the chosen retention window. Findings drop with their parent runs. Non-terminal runs are always preserved.'
				)}
				onclick={openPurge}
				disabled={busyKeys.has('purge')}
			>
				<Icon name="trash-alt" />
				{t('admin.jobs.purge', 'Purge old runs')}
			</button>
		</div>
	</header>

	{#if loadError}
		<p class="jobs-panel__status jobs-panel__status--error">{loadError}</p>
	{:else if !jobs}
		<p class="jobs-panel__status">{t('common.loading', 'Loading…')}</p>
	{:else if jobs.length === 0}
		<p class="jobs-panel__status">
			{t('admin.jobs.none_registered', 'No background tasks registered.')}
		</p>
	{:else}
		<table class="jobs-panel__table">
			<thead>
				<tr>
					<th class="jobs-panel__col-name">{t('admin.jobs.col_name', 'Name')}</th>
					<th>{t('admin.jobs.col_cadence', 'Cadence')}</th>
					<th>{t('admin.jobs.col_last_run', 'Last run')}</th>
					<th>{t('admin.jobs.col_outcome', 'Outcome')}</th>
					<th>{t('admin.jobs.col_state', 'State')}</th>
					<th class="jobs-panel__col-actions">{t('admin.jobs.col_actions', 'Actions')}</th>
				</tr>
			</thead>
			<tbody>
				{#each jobs as job (job.name)}
					{@const runs = runsByJob[job.name]}
					{@const runsErr = runsErrorByJob[job.name]}
					{@const runsLoading = runsLoadingByJob[job.name]}
					{@const expandedRun = expandedRunByJob[job.name] ?? null}
					<!-- Expandable if there is anything to show: a run history,
					     a description, or both. Gating on `recoverable` alone
					     would leave the plain periodic jobs (dedup_gc,
					     trash_cleanup, …) with no way to reach their
					     description at all. -->
					{@const canExpand = isRecoverable(job) || !!job.description}
					<tr class="jobs-panel__row" class:jobs-panel__row--expanded={expandedJob === job.name}>
						<td>
							{#if canExpand}
								<button
									type="button"
									class="jobs-panel__expand"
									aria-expanded={expandedJob === job.name}
									onclick={() => toggleJob(job)}
								>
									<Icon name={expandedJob === job.name ? 'chevron-down' : 'chevron-right'} />
									<span class="jobs-panel__name">{job.name}</span>
								</button>
							{:else}
								<span class="jobs-panel__name jobs-panel__name--flat">{job.name}</span>
							{/if}
							{#if mutatesLabel(job)}
								<span class="jobs-panel__pill jobs-panel__pill--readonly">
									{mutatesLabel(job)}
								</span>
							{/if}
							<!-- Configured to fire at boot. Called out because the
							     row is otherwise silent about the most consequential
							     fact on it: with repair on, this job deletes files
							     on every restart, not only when someone clicks Run.
							     Warn-coloured in that case, neutral otherwise. -->
							{#if job.startup}
								<span
									class="jobs-panel__pill"
									class:jobs-panel__pill--paused={job.startup.repair}
									class:jobs-panel__pill--neutral={!job.startup.repair}
									title={job.startup.repair
										? t(
												'admin.jobs.startup_repair_tooltip',
												'Configured in OXICLOUD_STARTUP_JOBS to run in repair mode at every boot.'
											)
										: t(
												'admin.jobs.startup_tooltip',
												'Configured in OXICLOUD_STARTUP_JOBS to run at every boot.'
											)}
								>
									{job.startup.repair
										? t('admin.jobs.startup_repair', 'at boot · repair')
										: t('admin.jobs.startup', 'at boot')}
								</span>
							{/if}
						</td>
						<td class="jobs-panel__muted">{cadenceLabel(job)}</td>
						<td class="jobs-panel__muted">{timeAgo(job.last_run_at)}</td>
						<td>
							<div class="jobs-panel__outcome-cell">
								<span class={outcomeClass(job)}>{outcomeLabel(job)}</span>
								{#if actionableFindingCount(job) > 0}
									{@const findings = actionableFindingCount(job)}
									<span
										class="jobs-panel__pill jobs-panel__pill--err"
										title={t(
											'admin.jobs.findings_present_tooltip',
											'Expand this run to see per-finding detail.'
										)}
									>
										{t('admin.jobs.n_findings', { n: findings }, '{{n}} findings')}
									</span>
								{/if}
								{#if anomalyFindingCount(job) > 0}
									{@const notices = anomalyFindingCount(job)}
									<span
										class="jobs-panel__pill jobs-panel__pill--notice"
										title={t(
											'admin.jobs.notices_present_tooltip',
											'Informational findings — no action required. Expand for detail.'
										)}
									>
										{t('admin.jobs.n_notices', { n: notices }, '{{n}} notices')}
									</span>
								{/if}
							</div>
						</td>
						<td>
							{#if isRunning(job)}
								<span class="jobs-panel__pill jobs-panel__pill--running">
									{t('admin.jobs.state_running', 'running')}
								</span>
							{:else}
								<span class="jobs-panel__muted">—</span>
							{/if}
						</td>
						<td class="jobs-panel__actions">
							{#if job.paused_run}
								<!-- Paused row: [Resume (X/Y)] to continue, [Cancel]
								     to abandon the checkpoint (marks run Cancelled). -->
								{@const p = job.paused_run}
								{@const label =
									p.total && p.total > 0
										? t(
												'admin.jobs.resume_progress',
												{ scanned: p.scanned, total: p.total },
												'Resume ({{scanned}}/{{total}})'
											)
										: t('admin.jobs.resume', 'Resume')}
								<button
									class="jobs-panel__btn jobs-panel__btn--small jobs-panel__btn--primary"
									disabled={busyKeys.has(`trigger:${job.name}`)}
									onclick={() => onTrigger(job.name)}
									title={t(
										'admin.jobs.resume_title',
										'Continue the paused run from its last checkpoint.'
									)}
								>
									{label}
								</button>
								<button
									class="jobs-panel__btn jobs-panel__btn--small jobs-panel__btn--danger"
									disabled={busyKeys.has(`cancel:${job.name}`)}
									onclick={() => onCancel(job.name)}
									title={t(
										'admin.jobs.cancel_paused_title',
										'Abandon the paused run — marks it as Cancelled. Not resumable.'
									)}
								>
									{t('admin.jobs.cancel', 'Cancel')}
								</button>
							{:else}
								<!-- Split-button: primary "Run" fires the default
								     trigger; the chevron opens a menu with the
								     tenant-specific variants (Run deep / Repair).
								     Rows without any variant render a bare Run
								     button — no chevron, no menu, no extra
								     width. Preserves one-click discovery for
								     the common case. -->
								{@const hasRunVariants = supportsDeep(job.name) || supportsRepair(job)}
								<span class="jobs-panel__split">
									<button
										class="jobs-panel__btn jobs-panel__btn--small"
										class:jobs-panel__split-main={hasRunVariants}
										disabled={busyKeys.has(`trigger:${job.name}`)}
										title={job.description || undefined}
										onclick={() => {
											closeAllRunMenus();
											void onTriggerGuarded(job);
										}}
									>
										{t('admin.jobs.run', 'Run')}
									</button>
									{#if hasRunVariants}
										<button
											class="jobs-panel__btn jobs-panel__btn--small jobs-panel__split-toggle"
											aria-haspopup="menu"
											aria-expanded={runMenuOpen[job.name] ?? false}
											aria-label={t('admin.jobs.run_variants_menu', 'Run variants menu')}
											onclick={() => toggleRunMenu(job.name)}
										>
											<Icon name="caret-down" />
										</button>
										{#if runMenuOpen[job.name]}
											<div class="jobs-panel__run-menu" role="menu">
												{#if supportsDeep(job.name)}
													<button
														type="button"
														class="jobs-panel__run-menu-item"
														role="menuitem"
														disabled={busyKeys.has(`trigger:${job.name}:deep`)}
														title={t(
															'admin.jobs.run_deep_hint',
															'Also runs slow variants (blob re-hash, bitrot detection).'
														)}
														onclick={() => {
															closeAllRunMenus();
															void onTrigger(job.name, { deep: true });
														}}
													>
														<Icon name="search" />
														<span>{t('admin.jobs.run_deep', 'Run deep')}</span>
													</button>
												{/if}
												{#if supportsRepair(job)}
													<button
														type="button"
														class="jobs-panel__run-menu-item jobs-panel__run-menu-item--warn"
														role="menuitem"
														disabled={busyKeys.has(`trigger:${job.name}:repair`)}
														title={job.repair_description}
														onclick={() => {
															closeAllRunMenus();
															void onTriggerWithRepairConfirm(job);
														}}
													>
														<Icon name="cog" />
														<span>{t('admin.jobs.run_repair', 'Repair')}</span>
													</button>
												{/if}
											</div>
										{/if}
									{/if}
								</span>
							{/if}
							<!-- `isRecoverable`, not `canExpand`: the latter now also
							     covers rows that expand only to show a description,
							     and those must not gain a Cancel button they never
							     had. -->
							{#if isRunning(job) && isRecoverable(job)}
								{#if isRecoverable(job)}
									<!-- Recoverable running: [Pause] preserves cursor
									     for later resume; [Cancel] abandons terminally
									     (engine writes Cancelled when the handler yields). -->
									<button
										class="jobs-panel__btn jobs-panel__btn--small"
										disabled={busyKeys.has(`pause:${job.name}`)}
										onclick={() => onPause(job.name)}
										title={t(
											'admin.jobs.pause_title',
											'Signal a graceful pause at the next batch boundary. Run row stays as `Paused` — Resume picks up from the checkpoint.'
										)}
									>
										{t('admin.jobs.pause', 'Pause')}
									</button>
									<button
										class="jobs-panel__btn jobs-panel__btn--small jobs-panel__btn--danger"
										disabled={busyKeys.has(`cancel:${job.name}`)}
										onclick={() => onCancel(job.name)}
										title={t(
											'admin.jobs.cancel_running_title',
											'Abandon the run — marks it as Cancelled at the next batch boundary. Not resumable.'
										)}
									>
										{t('admin.jobs.cancel', 'Cancel')}
									</button>
								{:else}
									<button
										class="jobs-panel__btn jobs-panel__btn--small jobs-panel__btn--danger"
										disabled={busyKeys.has(`cancel:${job.name}`)}
										onclick={() => onCancel(job.name)}
									>
										{t('admin.jobs.cancel', 'Cancel')}
									</button>
								{/if}
							{/if}
						</td>
					</tr>

					<!-- First row of the expanded block, and only visible there:
					     the description answers "what is this job" before the
					     run history answers "what did it do", and keeping it
					     folded keeps the collapsed table scannable — 17 rows of
					     two-line prose is not a table any more. -->
					{#if expandedJob === job.name && job.description}
						<tr class="jobs-panel__desc-row">
							<td colspan="6">
								<p class="jobs-panel__description">{job.description}</p>
							</td>
						</tr>
					{/if}

					{#if expandedJob === job.name && isRecoverable(job)}
						<tr class="jobs-panel__runs">
							<td colspan="6">
								<div class="jobs-panel__runs-inner">
									<div class="jobs-panel__runs-header">
										<h3>{t('admin.jobs.runs_title', 'Recent runs')}</h3>
										<button
											type="button"
											class="jobs-panel__link"
											onclick={() => loadRuns(job.name)}
											disabled={!!runsLoading}
										>
											<Icon name="repeat" />
											{t('admin.jobs.refresh', 'Refresh')}
										</button>
									</div>
									{#if runsErr}
										<p class="jobs-panel__status jobs-panel__status--error">{runsErr}</p>
									{:else if !runs}
										<p class="jobs-panel__status">{t('common.loading', 'Loading…')}</p>
									{:else if runs.length === 0}
										<p class="jobs-panel__status">
											{t('admin.jobs.no_runs', 'No runs yet.')}
										</p>
									{:else}
										<table class="jobs-panel__inner-table">
											<thead>
												<tr>
													<th></th>
													<th>{t('admin.jobs.col_started_at', 'Started')}</th>
													<th>{t('admin.jobs.col_status', 'Status')}</th>
													<th>{t('admin.jobs.col_duration', 'Duration')}</th>
													<th class="jobs-panel__col-progress">
														{t('admin.jobs.col_progress', 'Progress')}
													</th>
													<th>{t('admin.jobs.col_findings', 'Findings')}</th>
													<th class="jobs-panel__col-error">
														{t('admin.jobs.col_error', 'Error')}
													</th>
												</tr>
											</thead>
											<tbody>
												{#each runs as run (run.id)}
													{@const scanned = statNumber(run, 'scanned_count')}
													{@const findingCount = statNumber(run, 'finding_count')}
													{@const isRunExpanded = expandedRun === run.id}
													<tr>
														<td>
															<button
																type="button"
																class="jobs-panel__expand jobs-panel__expand--small"
																aria-expanded={isRunExpanded}
																onclick={() => toggleRun(job.name, run.id)}
															>
																<Icon name={isRunExpanded ? 'chevron-down' : 'chevron-right'} />
															</button>
														</td>
														<td class="jobs-panel__muted">
															{timeAgo(run.started_at)}
														</td>
														<td>
															<span class={statusClass(run.status)}>
																{statusLabel(run.status)}
															</span>
														</td>
														<td class="jobs-panel__muted">
															{runDurationLabel(run)}
														</td>
														<td>
															{#if run.progress}
																{@const barPct = Math.min(
																	100,
																	Math.max(0, run.progress.fraction * 100)
																)}
																{@const pctLabel = (run.progress.fraction * 100).toFixed(1) + '%'}
																<div
																	class="jobs-panel__progress"
																	title={run.progress.kind === 'approximate'
																		? t(
																				'admin.jobs.progress_approx_tooltip',
																				{
																					pct: pctLabel,
																					scanned: run.progress.scanned,
																					total: run.progress.total
																				},
																				'{{pct}} ({{scanned}} / {{total}} — approximate, backend proxy)'
																			)
																		: t(
																				'admin.jobs.progress_exact_tooltip',
																				{
																					pct: pctLabel,
																					scanned: run.progress.scanned,
																					total: run.progress.total
																				},
																				'{{pct}} ({{scanned}} / {{total}})'
																			)}
																>
																	<div
																		class="jobs-panel__progress-bar"
																		class:jobs-panel__progress-bar--approx={run.progress.kind ===
																			'approximate'}
																	>
																		<div
																			class="jobs-panel__progress-fill"
																			style:width="{barPct}%"
																		></div>
																	</div>
																	<span class="jobs-panel__progress-label">
																		{run.progress.scanned}/{run.progress.total}
																	</span>
																</div>
															{:else if scanned != null}
																<span
																	class="jobs-panel__muted"
																	title={t(
																		'admin.jobs.progress_scanned_only_tooltip',
																		'No total available for this run (pre-progress-bar deploy or the tenant does not report a countable subject).'
																	)}
																>
																	{t(
																		'admin.jobs.progress_scanned_only',
																		{ n: scanned },
																		'{{n}} scanned'
																	)}
																</span>
															{:else}
																<span class="jobs-panel__muted">—</span>
															{/if}
														</td>
														<td class="jobs-panel__num">
															{#if findingCount && findingCount > 0}
																<span
																	class="jobs-panel__pill jobs-panel__pill--err"
																	title={t(
																		'admin.jobs.findings_present_tooltip',
																		'Expand this run to see per-finding detail.'
																	)}
																>
																	{findingCount}
																</span>
															{:else}
																<span class="jobs-panel__muted">0</span>
															{/if}
														</td>
														<td class="jobs-panel__err-cell">
															{#if run.error_message}
																<code>{run.error_message}</code>
															{:else}
																<span class="jobs-panel__muted">—</span>
															{/if}
														</td>
													</tr>
													{#if isRunExpanded}
														{@const findings = findingsByRun[run.id]}
														{@const findingsErr = findingsErrorByRun[run.id]}
														{@const fLoading = findingsLoadingByRun[run.id]}
														<tr class="jobs-panel__run-detail">
															<td colspan="7">
																<div class="jobs-panel__run-detail-inner">
																	<details class="jobs-panel__json">
																		<summary>
																			{t('admin.jobs.run_json', 'Run summary (JSON)')}
																		</summary>
																		<pre>{JSON.stringify(
																				{
																					id: run.id,
																					status: run.status,
																					started_at: run.started_at,
																					last_progress_at: run.last_progress_at,
																					completed_at: run.completed_at,
																					stats: run.stats,
																					params: run.params,
																					cursor_hex: run.cursor_hex,
																					error_message: run.error_message
																				},
																				null,
																				2
																			)}</pre>
																	</details>
																	<div class="jobs-panel__findings">
																		<div class="jobs-panel__findings-header">
																			<h4>
																				{t('admin.jobs.findings_title', 'Findings')}
																			</h4>
																			<button
																				type="button"
																				class="jobs-panel__link"
																				onclick={() => loadFindings(job.name, run.id)}
																				disabled={!!fLoading}
																			>
																				<Icon name="repeat" />
																				{t('admin.jobs.refresh', 'Refresh')}
																			</button>
																		</div>
																		{#if findingsErr}
																			<p class="jobs-panel__status jobs-panel__status--error">
																				{findingsErr}
																			</p>
																		{:else if !findings}
																			<p class="jobs-panel__status">
																				{t('common.loading', 'Loading…')}
																			</p>
																		{:else if findings.length === 0}
																			<p class="jobs-panel__status">
																				{t('admin.jobs.no_findings', 'No findings — clean run.')}
																			</p>
																		{:else}
																			<table
																				class="jobs-panel__inner-table jobs-panel__findings-table"
																			>
																				<thead>
																					<tr>
																						<th>
																							{t('admin.jobs.col_kind', 'Kind')}
																						</th>
																						<th>
																							{t('admin.jobs.col_severity', 'Severity')}
																						</th>
																						<th>
																							{t('admin.jobs.col_resource', 'Resource')}
																						</th>
																						<th>
																							{t('admin.jobs.col_detail', 'Detail')}
																						</th>
																					</tr>
																				</thead>
																				<tbody>
																					{#each findings as f (f.id)}
																						{@const detail = (f.detail ?? {}) as Record<
																							string,
																							unknown
																						>}
																						{@const label =
																							(detail.path as string | undefined) ??
																							(detail.name as string | undefined) ??
																							null}
																						<tr>
																							<td><code>{f.kind}</code></td>
																							<td>
																								<span
																									class="jobs-panel__pill jobs-panel__pill--{severityPillModifier(
																										f.severity
																									)}"
																								>
																									{f.severity}
																								</span>
																							</td>
																							<td>
																								{#if label}
																									<div class="jobs-panel__resource">
																										<code class="jobs-panel__resource-name"
																											>{label}</code
																										>
																										{#if f.resource_id}
																											<code class="jobs-panel__resource-uuid"
																												>{f.resource_id}</code
																											>
																										{/if}
																									</div>
																								{:else}
																									<code class="jobs-panel__muted"
																										>{f.resource_id ?? '—'}</code
																									>
																								{/if}
																							</td>
																							<td>
																								<code class="jobs-panel__detail"
																									>{JSON.stringify(f.detail)}</code
																								>
																							</td>
																						</tr>
																					{/each}
																				</tbody>
																			</table>
																		{/if}
																	</div>
																</div>
															</td>
														</tr>
													{/if}
												{/each}
											</tbody>
										</table>
									{/if}
								</div>
							</td>
						</tr>
					{/if}
				{/each}
			</tbody>
		</table>
	{/if}
</section>

<!-- Purge modal — pick a retention window, then confirm. -->
<Modal
	open={purgeModal !== null}
	title={t('admin.jobs.purge_title', 'Purge old task history')}
	onclose={closePurge}
>
	{#if purgeModal}
		<form
			class="jobs-panel__purge-form"
			onsubmit={(e) => {
				e.preventDefault();
				void confirmPurge();
			}}
		>
			<p>
				{t(
					'admin.jobs.purge_body',
					'Delete completed and failed run history older than the chosen number of days. Findings drop with their parent runs. Non-terminal runs (running, paused, cancel-requested) are always preserved.'
				)}
			</p>
			<label>
				<span>{t('admin.jobs.purge_days_label', 'Retention (days)')}</span>
				<input
					type="number"
					min="1"
					step="1"
					bind:value={purgeModal.days}
					disabled={busyKeys.has('purge')}
				/>
			</label>
			<div class="jobs-panel__purge-actions">
				<button
					type="button"
					class="jobs-panel__btn"
					onclick={closePurge}
					disabled={busyKeys.has('purge')}
				>
					{t('common.cancel', 'Cancel')}
				</button>
				<button
					type="submit"
					class="jobs-panel__btn jobs-panel__btn--danger"
					disabled={busyKeys.has('purge')}
				>
					<Icon name="trash-alt" />
					{t('admin.jobs.purge_confirm', 'Purge')}
				</button>
			</div>
		</form>
	{/if}
</Modal>

<style>
	.jobs-panel {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.jobs-panel__header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 1rem;
		flex-wrap: wrap;
	}

	.jobs-panel__hint {
		margin: 0;
		color: var(--color-text-muted);
		font-size: 0.9rem;
	}

	.jobs-panel__header-actions {
		display: flex;
		gap: 0.5rem;
	}

	.jobs-panel__status {
		padding: 0.75rem;
		background: var(--color-bg-subtle);
		border-radius: 4px;
		color: var(--color-text-muted);
	}

	.jobs-panel__status--error {
		background: var(--color-danger-bg);
		color: var(--color-danger-text);
	}

	.jobs-panel__table {
		width: 100%;
		border-collapse: collapse;
		background: var(--color-bg-surface);
	}

	.jobs-panel__table th,
	.jobs-panel__table td {
		text-align: left;
		padding: 0.6rem 0.75rem;
		border-bottom: 1px solid var(--color-border);
	}

	.jobs-panel__table th {
		font-weight: 600;
		font-size: 0.85rem;
		color: var(--color-text-muted);
		background: var(--color-bg-subtle);
	}

	.jobs-panel__col-name {
		width: 24%;
	}

	.jobs-panel__col-actions {
		width: 24%;
	}

	.jobs-panel__row--expanded {
		background: var(--color-bg-subtle);
	}

	.jobs-panel__row--expanded td {
		border-bottom-color: transparent;
	}

	.jobs-panel__expand {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		background: none;
		border: none;
		cursor: pointer;
		font: inherit;
		color: var(--color-text);
		padding: 0;
	}

	.jobs-panel__expand--small {
		padding: 0.25rem;
	}

	.jobs-panel__name {
		font-family: var(--font-mono, monospace);
	}

	.jobs-panel__name--flat {
		padding-left: 1.3rem; /* line up with the icon-affixed rows */
	}

	.jobs-panel__muted {
		color: var(--color-text-muted);
	}

	.jobs-panel__num {
		font-variant-numeric: tabular-nums;
		text-align: right;
	}

	.jobs-panel__actions {
		display: flex;
		gap: 0.4rem;
		flex-wrap: wrap;
	}

	.jobs-panel__btn {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		padding: 0.4rem 0.8rem;
		border: 1px solid var(--color-border);
		background: var(--color-bg-surface);
		border-radius: 4px;
		cursor: pointer;
		font: inherit;
		color: var(--color-text);
	}

	.jobs-panel__btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.jobs-panel__btn--primary {
		background: var(--color-primary);
		color: var(--color-on-accent);
		border-color: var(--color-primary);
	}

	.jobs-panel__btn--small {
		padding: 0.2rem 0.5rem;
		font-size: 0.85rem;
	}

	.jobs-panel__btn--danger {
		border-color: var(--color-danger-bg);
		color: var(--color-danger-text-alt);
	}

	/* Warn variant — used for actions that mutate data but are content-
	   safe / reversible-in-outcome (e.g. Repair ref_counts). Signals
	   "read the tooltip and the confirm before clicking" without the
	   danger red reserved for destructive delete-style buttons. */
	.jobs-panel__btn--warn {
		border-color: var(--color-warning-border);
		color: var(--color-warning-text);
	}

	/* Split-button — inline flex holding a primary "Run" (fires default
	   action) and a chevron (opens the variants menu). `position:
	   relative` anchors the menu below the toggle. Only rendered on
	   rows whose job supports at least one variant; plain-Run rows
	   sidestep this whole structure. */
	.jobs-panel__split {
		display: inline-flex;
		position: relative;
	}

	/* Attached-button trick: main loses its right border-radius, toggle
	   loses its left. Toggle also loses its left border so the two
	   don't render a double-thick divider. */
	.jobs-panel__split-main {
		border-top-right-radius: 0;
		border-bottom-right-radius: 0;
	}

	.jobs-panel__split-toggle {
		border-top-left-radius: 0;
		border-bottom-left-radius: 0;
		border-left: none;
		padding-left: 0.35rem;
		padding-right: 0.35rem;
	}

	/* The variants menu — dropdown below the toggle, right-aligned so
	   it doesn't overflow the Actions column edge into the next row's
	   badge cell. Shadow + surface bg mirror the /files upload
	   dropdown (`upload-dropdown-menu`); using local CSS here rather
	   than the ported class so the jobs-panel keeps its scoped styling. */
	.jobs-panel__run-menu {
		position: absolute;
		top: calc(100% + 2px);
		right: 0;
		z-index: 30;
		min-width: 10rem;
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md, 6px);
		box-shadow: var(--shadow-md);
		padding: 0.25rem 0;
	}

	.jobs-panel__run-menu-item {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		width: 100%;
		padding: 0.4rem 0.75rem;
		background: transparent;
		border: none;
		text-align: left;
		font: inherit;
		color: var(--color-text);
		cursor: pointer;
		white-space: nowrap;
	}

	.jobs-panel__run-menu-item:hover:not(:disabled) {
		background: var(--color-bg-hover);
	}

	.jobs-panel__run-menu-item:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	/* Warn colour on the menu item mirrors the button variant so the
	   Repair option carries the same "attention-worthy but not
	   destructive" visual weight as its top-bar counterpart. */
	.jobs-panel__run-menu-item--warn {
		color: var(--color-warning-text);
	}

	.jobs-panel__pill {
		display: inline-block;
		padding: 0.1rem 0.5rem;
		border-radius: 999px;
		font-size: 0.8rem;
		font-weight: 500;
	}

	.jobs-panel__pill--ok {
		background: var(--color-success-bg);
		color: var(--color-success-text);
	}

	.jobs-panel__pill--err {
		background: var(--color-danger-lighter);
		color: var(--color-danger-text-alt);
	}

	.jobs-panel__pill--running {
		background: var(--color-info-bg);
		color: var(--color-info-text);
	}

	.jobs-panel__pill--paused {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.jobs-panel__pill--notice {
		background: var(--color-info-bg);
		color: var(--color-info-text);
	}

	.jobs-panel__pill--neutral {
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
	}

	/* "read-only" sits beside the job name and answers the question an
	   operator asks before every trigger. Deliberately quiet — it marks
	   the safe case, so it should not compete with outcome pills. */
	.jobs-panel__pill--readonly {
		margin-left: 0.4rem;
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
		font-size: 0.72rem;
		font-weight: 400;
		vertical-align: middle;
	}

	/* Opens the expanded block, so it carries the drawer's background and
	   drops its own separator — the runs table below it is part of the
	   same block, not a new entry. */
	.jobs-panel__desc-row td {
		padding-left: 2rem; /* clears the chevron, lines up with the name */
		border-bottom-color: transparent;
		background: var(--color-bg-subtle);
	}

	.jobs-panel__description {
		margin: 0;
		color: var(--color-text-muted);
		font-size: 0.8rem;
		line-height: 1.4;
	}

	.jobs-panel__runs {
		background: var(--color-bg-subtle);
	}

	.jobs-panel__runs-inner {
		padding: 0.75rem 1rem;
	}

	.jobs-panel__runs-header,
	.jobs-panel__findings-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		margin-bottom: 0.5rem;
	}

	.jobs-panel__runs-header h3,
	.jobs-panel__findings-header h4 {
		margin: 0;
	}

	.jobs-panel__inner-table {
		width: 100%;
		border-collapse: collapse;
		background: var(--color-bg-surface);
		font-size: 0.9rem;
	}

	.jobs-panel__inner-table th,
	.jobs-panel__inner-table td {
		text-align: left;
		padding: 0.4rem 0.6rem;
		border-bottom: 1px solid var(--color-border);
	}

	.jobs-panel__inner-table th {
		background: var(--color-bg-subtle);
		font-weight: 600;
		color: var(--color-text-muted);
	}

	.jobs-panel__col-error {
		max-width: 24rem;
	}

	.jobs-panel__err-cell code {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}

	.jobs-panel__run-detail {
		background: var(--color-bg-subtle);
	}

	.jobs-panel__run-detail-inner {
		padding: 0.75rem;
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.jobs-panel__json summary {
		cursor: pointer;
		color: var(--color-text-muted);
		font-size: 0.85rem;
	}

	.jobs-panel__json pre {
		background: var(--color-bg-surface);
		padding: 0.5rem;
		border-radius: 4px;
		font-size: 0.8rem;
		margin: 0.5rem 0 0;
		/* Wrap long values (cursor_hex is 128 hex chars) instead of
		   expanding the table cell — the run-drawer sits inside a
		   `<td colspan>` that would otherwise grow horizontally past
		   the viewport and blow out the page layout. `pre-wrap`
		   preserves the multi-line JSON.stringify(…, 2) indent;
		   `word-break: break-all` breaks the long hex strings mid-run
		   without hyphens.

		   `overflow-x: auto` is kept as a defense-in-depth for any
		   future field that pre-wrap can't handle (e.g. a single
		   unbroken word longer than max-width). It only kicks in
		   when wrapping isn't enough. */
		white-space: pre-wrap;
		word-break: break-all;
		max-width: 100%;
		overflow-x: auto;
	}

	.jobs-panel__findings h4 {
		margin: 0 0 0.4rem;
	}

	.jobs-panel__findings-table code {
		font-size: 0.8rem;
	}

	.jobs-panel__detail {
		font-size: 0.75rem;
		word-break: break-all;
	}

	.jobs-panel__resource {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
	}

	.jobs-panel__resource-name {
		font-size: 0.85rem;
		color: var(--color-text);
		word-break: break-all;
	}

	.jobs-panel__resource-uuid {
		font-size: 0.7rem;
		color: var(--color-text-muted);
		word-break: break-all;
	}

	.jobs-panel__outcome-cell {
		display: flex;
		gap: 0.4rem;
		align-items: center;
		flex-wrap: wrap;
	}

	.jobs-panel__purge-form {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.jobs-panel__purge-form label {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.jobs-panel__purge-form input {
		max-width: 8rem;
	}

	.jobs-panel__purge-actions {
		display: flex;
		gap: 0.5rem;
		justify-content: flex-end;
	}

	.jobs-panel__link {
		display: inline-flex;
		align-items: center;
		gap: 0.3rem;
		background: none;
		border: none;
		color: var(--color-accent);
		cursor: pointer;
		font: inherit;
		font-size: 0.85rem;
		padding: 0;
	}

	.jobs-panel__link:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.jobs-panel__col-progress {
		min-width: 12rem;
	}

	.jobs-panel__progress {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.jobs-panel__progress-bar {
		flex: 1;
		height: 0.5rem;
		background: var(--color-bg-subtle);
		border-radius: 999px;
		overflow: hidden;
	}

	.jobs-panel__progress-fill {
		height: 100%;
		background: var(--color-accent);
		transition: width 0.25s ease-out;
	}

	.jobs-panel__progress-bar--approx .jobs-panel__progress-fill {
		background: repeating-linear-gradient(
			45deg,
			var(--color-accent),
			var(--color-accent) 6px,
			var(--color-accent-hover) 6px,
			var(--color-accent-hover) 12px
		);
	}

	.jobs-panel__progress-label {
		font-variant-numeric: tabular-nums;
		font-size: 0.8rem;
		color: var(--color-text-muted);
		white-space: nowrap;
	}
</style>
