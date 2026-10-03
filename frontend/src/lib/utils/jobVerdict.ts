/**
 * How the last dispatch of a job turned out, as one discriminant.
 *
 * Extracted from `AdminJobsPanel` because the rules are subtle, each one
 * exists because of a specific wrong answer the panel once gave, and a
 * function inside a `.svelte` file cannot be unit-tested. The component
 * keeps the label and colour mapping; this decides *which* verdict.
 *
 * NOT the run's lifecycle state — that is `last_run_status`, rendered in
 * its own column. A job can be `Paused` and still have a meaningful
 * verdict about what the run managed to do before it stopped.
 */
import type { JobSummary } from '$lib/api/types';

export type JobVerdict =
	/** The run gave up: the backend was unreachable after its retries,
	 *  or it failed terminally. Outranks everything else — whatever it
	 *  did or did not find, the sweep is incomplete. */
	| 'stopped'
	/** Actionable findings: `data_loss` or `inconsistent`. */
	| 'issues'
	/** Informational findings only (`anomaly`). */
	| 'notices'
	/** Finished, found nothing. */
	| 'ok'
	/** The dispatch itself errored. */
	| 'err'
	/** Ran, but no outcome is in memory and no findings — a restart
	 *  emptied it. Distinct from `never`. */
	| 'unknown'
	/** No run row and no dispatch: this job has never run. */
	| 'never';

/**
 * Why the last run stopped, or `undefined` if it did not.
 *
 * Prefers the run row over the in-memory outcome. Both matter: the row
 * survives a restart, and a *non-recoverable* job has no row at all, so
 * its dispatch's `extra.reason` is the only source.
 *
 * The defect this exists for: a retryable pause reports `outcome: "ok"`
 * on the wire — correctly, since the run did not fail and a Resume
 * continues it — so a panel trusting that rendered a green "ok" for a
 * job that gave up because its backend had vanished.
 */
export function stoppedReason(job: JobSummary): string | undefined {
	if (job.last_run_error_reason) return job.last_run_error_reason;
	if (job.last_outcome?.outcome !== 'ok') return undefined;
	const reason = (job.last_outcome.extra as { reason?: unknown } | undefined)?.reason;
	return typeof reason === 'string' ? reason : undefined;
}

/**
 * Per-severity finding counts for the last run, from the durable source
 * first.
 *
 * `last_run_severity_counts` is counted from `jobs.run_findings` by the
 * list handler; `last_outcome.extra.severity_counts` is the same numbers
 * from the dispatch that produced them, in memory. The row wins because
 * memory does not survive a restart, and the finding pill is the only
 * place a finding is visible without deliberately opening a drawer.
 *
 * Severity values are an open set (the column is TEXT), so unknown keys
 * must degrade rather than throw.
 */
export function lastSeverityCounts(job: JobSummary): Record<string, number> {
	if (job.last_run_severity_counts) return job.last_run_severity_counts;
	if (!job.last_outcome || job.last_outcome.outcome !== 'ok') return {};
	const extra = job.last_outcome.extra as { severity_counts?: Record<string, number> } | undefined;
	return extra?.severity_counts ?? {};
}

/** `data_loss + inconsistent` — what turns the pill amber. */
export function actionableFindingCount(job: JobSummary): number {
	const s = lastSeverityCounts(job);
	return (s.data_loss ?? 0) + (s.inconsistent ?? 0);
}

/** `anomaly` — informational, rendered as a blue notice. */
export function anomalyFindingCount(job: JobSummary): number {
	return lastSeverityCounts(job).anomaly ?? 0;
}

export function jobVerdict(job: JobSummary): JobVerdict {
	// First, and ahead of the findings counts: a run that gave up did
	// not finish its sweep, so "ok" is wrong and even "issues"
	// understates it — the answer is incomplete, not clean or dirty. A
	// paused `backend_migration` is also still holding
	// `migration_readonly` and refusing writes across the whole app.
	if (stoppedReason(job)) return 'stopped';

	if (!job.last_outcome) {
		// No dispatch in memory — but findings are rows, so they still
		// decide. Showing "—" for a run that recorded data loss is the
		// defect this path exists to prevent, and it is precisely the
		// restarted-instance case: nobody watched the dispatch, which is
		// normal for a scheduled detector.
		if (actionableFindingCount(job) > 0) return 'issues';
		if (anomalyFindingCount(job) > 0) return 'notices';
		// Deliberately NOT 'ok' on an empty findings list: that would say
		// the sweep found nothing, when what we know is only that no
		// outcome was recorded. Whether it finished is the State column's
		// answer. A job with a run row DID run, so 'never' there would be
		// a lie the run history immediately contradicts.
		return job.last_run_status ? 'unknown' : 'never';
	}

	if (job.last_outcome.outcome === 'ok') {
		if (actionableFindingCount(job) > 0) return 'issues';
		if (anomalyFindingCount(job) > 0) return 'notices';
		return 'ok';
	}
	return 'err';
}
