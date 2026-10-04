import { describe, it, expect } from 'vitest';
import type { JobSummary } from '$lib/api/types';
import { jobVerdict, stoppedReason, actionableFindingCount } from './jobVerdict';

function job(over: Partial<JobSummary> = {}): JobSummary {
	return {
		name: 'backend_consistency',
		recoverable: true,
		running: false,
		...over
	} as JobSummary;
}

describe('a run that stopped', () => {
	// The defect: the engine reports `ok` for a retryable pause —
	// correctly, since a Resume continues it — and the panel rendered a
	// green "ok" for a job that gave up because DNS had died.
	it('is never reported as ok, however the dispatch described itself', () => {
		const j = job({
			last_run_status: 'Paused',
			last_run_error_reason: 'backend_unavailable',
			last_outcome: { outcome: 'ok', count: 0 }
		});
		expect(jobVerdict(j)).toBe('stopped');
	});

	it('outranks the findings it did manage to record', () => {
		// An incomplete sweep that found two problems has not told you
		// there are only two.
		const j = job({
			last_run_status: 'Paused',
			last_run_error_reason: 'backend_timeout',
			last_run_severity_counts: { data_loss: 2 },
			last_outcome: { outcome: 'ok', count: 2 }
		});
		expect(actionableFindingCount(j)).toBe(2);
		expect(jobVerdict(j)).toBe('stopped');
	});

	it('is read from the run row, so it survives a restart emptying memory', () => {
		const j = job({ last_run_status: 'Paused', last_run_error_reason: 'backend_unavailable' });
		expect(stoppedReason(j)).toBe('backend_unavailable');
		expect(jobVerdict(j)).toBe('stopped');
	});

	it('falls back to the dispatch for a job with no run row', () => {
		// Non-recoverable jobs have no row in `jobs.recoverable_runs`, so
		// the in-memory outcome is the only source there is.
		const j = job({
			recoverable: false,
			last_outcome: { outcome: 'ok', count: 0, extra: { reason: 'backend_unavailable' } }
		});
		expect(stoppedReason(j)).toBe('backend_unavailable');
	});
});

describe('a pause an operator asked for', () => {
	// The whole discriminator: both land as `Paused`, only one carries a
	// reason. Pausing a job by hand must not light the panel up.
	it('is not reported as stopped, because it records no reason', () => {
		const j = job({
			last_run_status: 'Paused',
			last_outcome: { outcome: 'ok', count: 0 }
		});
		expect(stoppedReason(j)).toBeUndefined();
		expect(jobVerdict(j)).toBe('ok');
	});
});

describe('findings decide the verdict when the run finished', () => {
	it('reports issues for actionable findings', () => {
		expect(
			jobVerdict(
				job({
					last_run_severity_counts: { inconsistent: 3 },
					last_outcome: { outcome: 'ok', count: 3 }
				})
			)
		).toBe('issues');
	});

	it('reports notices for informational findings only', () => {
		expect(
			jobVerdict(
				job({ last_run_severity_counts: { anomaly: 1 }, last_outcome: { outcome: 'ok', count: 1 } })
			)
		).toBe('notices');
	});

	it('reports ok for a run that found nothing', () => {
		expect(
			jobVerdict(job({ last_run_severity_counts: {}, last_outcome: { outcome: 'ok', count: 0 } }))
		).toBe('ok');
	});

	it('reports err when the dispatch itself errored', () => {
		expect(jobVerdict(job({ last_outcome: { outcome: 'err', message: 'boom' } }))).toBe('err');
	});
});

describe('when no outcome is in memory', () => {
	// A restart empties `last_outcome`. Findings are rows, so they must
	// still decide — a completed run that recorded data loss rendering as
	// a neutral "—" is what this whole path exists to prevent, and it
	// matters most for a scheduled detector nobody watched.
	it('findings still decide', () => {
		expect(
			jobVerdict(job({ last_run_status: 'Completed', last_run_severity_counts: { data_loss: 1 } }))
		).toBe('issues');
	});

	it('an empty findings list is unknown, not ok', () => {
		// "Found nothing" and "we did not record the outcome" are
		// different facts, and only one of them is reassuring.
		expect(jobVerdict(job({ last_run_status: 'Completed', last_run_severity_counts: {} }))).toBe(
			'unknown'
		);
	});

	it('a job with no run row at all has never run', () => {
		expect(jobVerdict(job())).toBe('never');
	});
});
