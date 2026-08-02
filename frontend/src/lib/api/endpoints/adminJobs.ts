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
import type { Finding, JobOutcome, JobSummary, RunSummary } from '$lib/api/types';

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

/** Envelope from `POST /api/admin/jobs/{name}/cancel`. `run_id` is
 *  the id of the run whose `Running` status was flipped to
 *  `CancelRequested` (null when nothing was in flight to cancel). */
export interface CancelResponse {
	ok: boolean;
	run_id: string | null;
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
 * `POST /api/admin/jobs/{name}/trigger?force=X&deep=X` — dispatch a job
 * on-demand. `force` bypasses per-tenant idempotency checks (e.g.
 * `trash_cleanup` skipping when nothing is due). `deep` opts into slow
 * variants (currently only `storage_consistency`, propagated by
 * `consistency_batch` to every child).
 *
 * Throws on 4xx / 5xx with the backend's error message when present.
 * A 404 means the job name isn't registered — surface that specifically
 * so callers can distinguish "typo" from "handler blew up".
 */
export async function triggerJob(
	name: string,
	opts: { force?: boolean; deep?: boolean; storage?: string } = {}
): Promise<TriggerResponse> {
	const params = new URLSearchParams();
	if (opts.force) params.set('force', 'true');
	if (opts.deep) params.set('deep', 'true');
	// `storage` scopes tenants that respect JobRunArgs.storage —
	// currently blobs_consistency / backend_consistency (probes the
	// named entry instead of the live backend). See
	// `docs/plan/storage-multi-entry.md` slice 7.
	if (opts.storage) params.set('storage', opts.storage);
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
 * `POST /api/admin/jobs/{name}/cancel` — cooperatively request cancel
 * of the currently running instance. The handler observes it on its
 * next `store.status()` poll and returns `RunOutcome::Paused` at the
 * next safe boundary. If nothing is running, this is a no-op that
 * returns `run_id: null`.
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
