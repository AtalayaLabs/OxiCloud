/**
 * Admin JobRegistry endpoints — `/api/admin/jobs*` (see
 * `docs/plan/job-registry.md`). Powers the "Jobs" tab of the admin panel.
 *
 * Every mutation goes through the standard admin auth path (Bearer JWT
 * + admin-middleware role check). Read endpoints are cheap enough to
 * poll while the panel is open.
 */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { Finding, JobOutcome, JobParamValues, JobSummary, RunSummary } from '$lib/api/types';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/**
 * Envelope wrapping the outcome from `POST /api/admin/jobs/{name}/trigger`.
 * `ok: true` means "dispatch reached the handler"; the handler's own
 * pass/fail is in `outcome.outcome`. For `consistency_batch`, per-child
 * outcomes are inside `outcome.extra.per_check`.
 *
 * `outcome` is absent for detached jobs (currently only
 * `backend_migration`) — the endpoint returns `202 Accepted` with
 * `dispatched: true` immediately and the run continues in the
 * background. Progress polling shows the state; there's no synchronous
 * outcome to surface.
 */
export interface TriggerResponse {
	ok: boolean;
	outcome?: JobOutcome;
	dispatched?: boolean;
	detached?: boolean;
}

/** Envelope from `POST /api/admin/jobs/{name}/cancel` — terminal
 *  cancel. `run_id` populated iff a non-terminal row was flipped
 *  (Running/CancelRequested get the intent stamp; Paused gets a
 *  direct DB flip to Cancelled). */
export interface CancelResponse {
	cancelled: boolean;
	run_id?: string;
	reason?: string;
	note?: string;
}

/** Envelope from `POST /api/admin/jobs/{name}/pause` — soft pause. */
export interface PauseResponse {
	paused: boolean;
	run_id?: string;
	reason?: string;
	note?: string;
}

/**
 * `GET /api/admin/jobs` — full registry snapshot. One row per registered
 * job (periodic + recoverable + coordinators like `consistency_batch`,
 * which register as plain JobHandlers).
 */
export function listJobs(): Promise<JobSummary[]> {
	return apiJson<JobSummary[]>('/api/admin/jobs', { credentials: 'same-origin' });
}

/**
 * `POST /api/admin/jobs/{name}/trigger` — dispatch a job on-demand with
 * whichever parameters it declares.
 *
 * **Which parameters are valid is the job's answer, not this
 * function's.** Read them from `JobSummary.parameters` (each carries a
 * `type`, a `default` and the handler's own description) and pass the
 * ones the operator chose. Anything undeclared comes back as a 400
 * naming what the job does accept.
 *
 * This used to take fixed `force` / `deep` / `storage` / `repair`
 * options, which meant callers could pass a flag to a job that ignored
 * it and get a silent no-op — the panel offered exactly that on several
 * jobs.
 *
 * Omitted parameters take their declared defaults server-side, so `{}`
 * is a plain run.
 *
 * Throws on 4xx / 5xx with the backend's error message when present.
 * A 404 means the job name isn't registered — surface that specifically
 * so callers can distinguish "typo" from "handler blew up".
 */
export async function triggerJob(
	name: string,
	opts: JobParamValues = {}
): Promise<TriggerResponse> {
	// Free-form, because the accepted set is the job's to declare
	// (`JobSummary.parameters`) — not this function's to enumerate. The
	// backend validates: an undeclared name is a 400 listing what the
	// job does accept, rather than being silently ignored the way the
	// old fixed `force/deep/storage/repair` options were on jobs that
	// read none of them.
	const params = new URLSearchParams();
	for (const [key, value] of Object.entries(opts)) {
		// Skip `false` so a URL carries only what was asked for — the
		// backend applies each parameter's declared default for the rest,
		// and an explicit `force=false` would read identically while
		// making the audit line noisier.
		if (value === false || value === undefined || value === '') continue;
		params.set(key, String(value));
	}
	const q = params.toString();
	const url = `/api/admin/jobs/${encodeURIComponent(name)}/trigger${q ? `?${q}` : ''}`;
	const res = await apiFetch(url, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() }
	});
	if (!res.ok) {
		let msg = `trigger failed: ${res.status}`;
		try {
			const body = (await res.json()) as { error?: string; message?: string };
			msg = body.error ?? body.message ?? msg;
		} catch {
			/* no JSON body */
		}
		throw new Error(msg);
	}
	return (await res.json()) as TriggerResponse;
}

/**
 * `POST /api/admin/jobs/{name}/cancel` — TERMINAL cancel. Abandons
 * the run: Running/CancelRequested rows get stamped with the intent
 * flag and land as `Cancelled` when the handler yields; Paused rows
 * get flipped directly to `Cancelled`. Not resumable. Use `pauseJob`
 * for interruption-with-resume semantics.
 */
export async function cancelJob(name: string): Promise<CancelResponse> {
	const res = await apiFetch(`/api/admin/jobs/${encodeURIComponent(name)}/cancel`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() }
	});
	if (!res.ok) {
		let msg = `cancel failed: ${res.status}`;
		try {
			const body = (await res.json()) as { error?: string; message?: string };
			msg = body.error ?? body.message ?? msg;
		} catch {
			/* no JSON body */
		}
		throw new Error(msg);
	}
	return (await res.json()) as CancelResponse;
}

/**
 * `POST /api/admin/jobs/{name}/pause` — cooperative pause. Row lands
 * as `Paused` when the handler yields; a subsequent trigger click
 * resumes from the cursor via `run_or_resume`. Use `cancelJob` to
 * abandon terminally.
 */
export async function pauseJob(name: string): Promise<PauseResponse> {
	const res = await apiFetch(`/api/admin/jobs/${encodeURIComponent(name)}/pause`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() }
	});
	if (!res.ok) {
		let msg = `pause failed: ${res.status}`;
		try {
			const body = (await res.json()) as { error?: string; message?: string };
			msg = body.error ?? body.message ?? msg;
		} catch {
			/* no JSON body */
		}
		throw new Error(msg);
	}
	return (await res.json()) as PauseResponse;
}

/**
 * `GET /api/admin/jobs/{name}/runs?limit=N` — history of recoverable
 * runs for `name`, newest first. Backend caps `limit` at 100.
 */
export function listRuns(name: string, limit = 20): Promise<RunSummary[]> {
	return apiJson<RunSummary[]>(`/api/admin/jobs/${encodeURIComponent(name)}/runs?limit=${limit}`, {
		credentials: 'same-origin'
	});
}

/** Envelope from `POST /api/admin/jobs/runs/purge`. `purged` is
 *  the count of terminal-run rows deleted (findings cascade with
 *  their parent run via the FK, no separate counter). */
export interface PurgeResponse {
	purged: number;
	retention_days: number;
}

/**
 * `POST /api/admin/jobs/runs/purge?days=N` — operator-triggered
 * retention cleanup. Deletes terminal runs (`Completed`, `Failed`)
 * with `completed_at` older than `days` days ago; associated
 * `jobs.run_findings` rows drop with them via CASCADE. Non-terminal
 * runs (`Running`, `Paused`, `CancelRequested`) are ALWAYS
 * preserved regardless of age.
 *
 * Backend enforces a minimum of 1 day defensively.
 */
export async function purgeJobRuns(days = 30): Promise<PurgeResponse> {
	const res = await apiFetch(`/api/admin/jobs/runs/purge?days=${days}`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() }
	});
	if (!res.ok) {
		let msg = `purge failed: ${res.status}`;
		try {
			const body = (await res.json()) as { error?: string; message?: string };
			msg = body.error ?? body.message ?? msg;
		} catch {
			/* no JSON body */
		}
		throw new Error(msg);
	}
	return (await res.json()) as PurgeResponse;
}

/**
 * `GET /api/admin/jobs/{name}/runs/{id}/findings?limit=N&offset=M` —
 * paginated findings for a specific run. Empty list = clean run,
 * 404 = unknown run id.
 */
export function listFindings(
	name: string,
	runId: string,
	opts: { limit?: number; offset?: number } = {}
): Promise<Finding[]> {
	const params = new URLSearchParams();
	params.set('limit', String(opts.limit ?? 100));
	if (opts.offset) params.set('offset', String(opts.offset));
	return apiJson<Finding[]>(
		`/api/admin/jobs/${encodeURIComponent(name)}/runs/${encodeURIComponent(runId)}/findings?${params}`,
		{ credentials: 'same-origin' }
	);
}
