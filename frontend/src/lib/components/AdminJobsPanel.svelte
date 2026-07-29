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
	import { t } from '$lib/i18n/index.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import { ui } from '$lib/stores/ui.svelte';
	import {
		listJobs,
		listRuns,
		listFindings,
		triggerJob,
		cancelJob
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

	// ─── Loading + polling ─────────────────────────────────────────────

	async function loadJobs() {
		try {
			jobs = await listJobs();
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

	function toggleJob(name: string) {
		if (expandedJob === name) {
			expandedJob = null;
		} else {
			expandedJob = name;
			// Lazy-load on first open, refresh on subsequent opens.
			void loadRuns(name);
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

	async function onTrigger(name: string, opts: { deep?: boolean } = {}) {
		const key = `trigger:${name}${opts.deep ? ':deep' : ''}`;
		markBusy(key, true);
		try {
			const res = await triggerJob(name, opts);
			// The trigger envelope carries the child's outcome — surface
			// its pass/fail immediately so operators don't have to click
			// through to see whether the run completed cleanly.
			if (res.outcome.outcome === 'ok') {
				ui.notify(
					t('admin.jobs.triggered_ok', { name }, '{{name}} triggered successfully'),
					'success'
				);
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

	async function onCancel(name: string) {
		const key = `cancel:${name}`;
		markBusy(key, true);
		try {
			const res = await cancelJob(name);
			if (res.run_id) {
				ui.notify(
					t(
						'admin.jobs.cancel_requested',
						{ name },
						'Cancel requested — {{name}} will pause at the next safe boundary'
					),
					'info'
				);
			} else {
				ui.notify(
					t('admin.jobs.cancel_noop', { name }, 'Nothing to cancel — {{name}} is not running'),
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
	 * Number of findings the last completed run surfaced, from
	 * `last_outcome.extra.finding_count` (populated by `run_or_resume`
	 * on Completed/Paused). Returns 0 for jobs without a recoverable
	 * shape, jobs that haven't run yet, or runs pre-dating the field.
	 */
	function lastFindingCount(job: JobSummary): number {
		if (!job.last_outcome || job.last_outcome.outcome !== 'ok') return 0;
		const extra = job.last_outcome.extra as { finding_count?: number } | undefined;
		return extra?.finding_count ?? 0;
	}

	function outcomeLabel(job: JobSummary): string {
		if (!job.last_outcome) return t('admin.jobs.never', 'never');
		if (job.last_outcome.outcome === 'ok') {
			// `ok` on the wire = dispatch completed. But if findings
			// were surfaced, "ok" reads as "all good" to the operator,
			// which is misleading — flip the label + colour to warn.
			return lastFindingCount(job) > 0
				? t('admin.jobs.outcome_issues', 'issues')
				: t('admin.jobs.outcome_ok', 'ok');
		}
		return t('admin.jobs.outcome_err', 'err');
	}

	function outcomeClass(job: JobSummary): string {
		if (!job.last_outcome) return 'jobs-panel__pill jobs-panel__pill--neutral';
		if (job.last_outcome.outcome !== 'ok') {
			return 'jobs-panel__pill jobs-panel__pill--err';
		}
		return lastFindingCount(job) > 0
			? 'jobs-panel__pill jobs-panel__pill--paused'
			: 'jobs-panel__pill jobs-panel__pill--ok';
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
			default:
				return 'jobs-panel__pill jobs-panel__pill--neutral';
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

	// The `consistency_batch` coordinator gets a "Run deep" variant —
	// the only job that respects `?deep=true` today (via propagation to
	// `storage_consistency` once it lands).
	function supportsDeep(name: string): boolean {
		return name === 'consistency_batch';
	}

	function isRunning(job: JobSummary): boolean {
		return job.running;
	}

	function isRecoverable(job: JobSummary): boolean {
		// Heuristic: recoverable jobs are the ones that publish runs via
		// `jobs.recoverable_runs`. There's no direct flag on JobSummary
		// (Part 1 handlers and Part 2 adapters share the same summary
		// shape by design). Name-based recognition is fine for now — the
		// admin panel is the only consumer; broader use would call for
		// a `recoverable: bool` field in JobSummary.
		return name_is_recoverable(job.name);
	}
	function name_is_recoverable(name: string): boolean {
		return name.endsWith('_consistency') || name === 'storage_migration';
	}

	// Consistency batch shortcut — top button. Only shown when the
	// coordinator is registered (should always be true post-Slice 5,
	// but check defensively so the button doesn't appear on an old
	// deployment before this component is upgraded).
	const hasBatch = $derived(jobs?.some((j) => j.name === 'consistency_batch') ?? false);
</script>

<section class="jobs-panel">
	<header class="jobs-panel__header">
		<div class="jobs-panel__header-text">
			<h2>{t('admin.jobs.title', 'Jobs')}</h2>
			<p class="jobs-panel__hint">
				{t(
					'admin.jobs.hint',
					'Fires periodic + on-demand jobs. Consistency checks are safe to run at any time — they are read-only.'
				)}
			</p>
		</div>
		{#if hasBatch}
			<div class="jobs-panel__header-actions">
				<button
					class="jobs-panel__btn jobs-panel__btn--primary"
					disabled={busyKeys.has('trigger:consistency_batch')}
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
					<Icon name="search" />
					{t('admin.jobs.run_deep', 'Run deep')}
				</button>
			</div>
		{/if}
	</header>

	{#if loadError}
		<p class="jobs-panel__status jobs-panel__status--error">{loadError}</p>
	{:else if !jobs}
		<p class="jobs-panel__status">{t('common.loading', 'Loading…')}</p>
	{:else if jobs.length === 0}
		<p class="jobs-panel__status">{t('admin.jobs.none_registered', 'No jobs registered.')}</p>
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
					{@const canExpand = isRecoverable(job)}
					<tr class="jobs-panel__row" class:jobs-panel__row--expanded={expandedJob === job.name}>
						<td>
							{#if canExpand}
								<button
									type="button"
									class="jobs-panel__expand"
									aria-expanded={expandedJob === job.name}
									onclick={() => toggleJob(job.name)}
								>
									<Icon name={expandedJob === job.name ? 'chevron-down' : 'chevron-right'} />
									<span class="jobs-panel__name">{job.name}</span>
								</button>
							{:else}
								<span class="jobs-panel__name jobs-panel__name--flat">{job.name}</span>
							{/if}
						</td>
						<td class="jobs-panel__muted">{cadenceLabel(job)}</td>
						<td class="jobs-panel__muted">{timeAgo(job.last_run_at)}</td>
						<td>
							<div class="jobs-panel__outcome-cell">
								<span class={outcomeClass(job)}>{outcomeLabel(job)}</span>
								{#if lastFindingCount(job) > 0}
									{@const findings = lastFindingCount(job)}
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
							<button
								class="jobs-panel__btn jobs-panel__btn--small"
								disabled={busyKeys.has(`trigger:${job.name}`)}
								onclick={() => onTrigger(job.name)}
							>
								{t('admin.jobs.run', 'Run')}
							</button>
							{#if supportsDeep(job.name)}
								<button
									class="jobs-panel__btn jobs-panel__btn--small"
									disabled={busyKeys.has(`trigger:${job.name}:deep`)}
									onclick={() => onTrigger(job.name, { deep: true })}
								>
									{t('admin.jobs.run_deep', 'Run deep')}
								</button>
							{/if}
							{#if isRunning(job) && canExpand}
								<button
									class="jobs-panel__btn jobs-panel__btn--small jobs-panel__btn--danger"
									disabled={busyKeys.has(`cancel:${job.name}`)}
									onclick={() => onCancel(job.name)}
								>
									{t('admin.jobs.cancel', 'Cancel')}
								</button>
							{/if}
						</td>
					</tr>

					{#if expandedJob === job.name}
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
																{run.status}
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
																									class="jobs-panel__pill jobs-panel__pill--{f.severity ===
																									'data_loss'
																										? 'err'
																										: 'paused'}"
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

	.jobs-panel__header-text h2 {
		margin: 0 0 0.25rem;
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

	.jobs-panel__pill--neutral {
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
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
		overflow-x: auto;
		font-size: 0.8rem;
		margin: 0.5rem 0 0;
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
