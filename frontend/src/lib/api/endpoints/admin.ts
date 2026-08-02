/**
 * Admin endpoints — ported from views/admin/admin.js. Covers users, plugins
 * (incl. logs/retention/live SSE tail), dashboard, settings (OIDC/storage/SMTP),
 * and storage migration (incl. the verify integrity check).
 */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type {
	AdminUsersPage,
	Drive,
	DriveMember,
	DriveMemberSubject,
	DriveRole,
	User
} from '$lib/api/types';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

async function mutate(url: string, method: string, body?: unknown): Promise<void> {
	const res = await apiFetch(url, {
		method,
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: body === undefined ? undefined : JSON.stringify(body)
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { message?: string };
		throw new Error(e.message || `${method} ${url} failed: ${res.status}`);
	}
}

/** POST with no request body that returns a JSON payload (throws on non-2xx). */
async function postJson<T>(url: string): Promise<T> {
	const res = await apiFetch(url, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() }
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { message?: string };
		throw new Error(e.message || `POST ${url} failed: ${res.status}`);
	}
	return (await res.json()) as T;
}

// ── Maintenance ───────────────────────────────────────────────────────────

/** Outcome of a bulk metadata re-extraction run. */
export interface ReextractResult {
	message: string;
	total: number;
	processed: number;
	failed: number;
}

/** Re-scan every audio file and backfill its tag metadata (idempotent). */
export function reextractAudioMetadata(): Promise<ReextractResult> {
	return postJson<ReextractResult>('/api/admin/audio/metadata/reextract');
}

/** Backfill EXIF / container capture dates for all media, re-bucketing the
 *  Photos timeline by real capture date (idempotent). */
export function reextractPhotoMetadata(): Promise<ReextractResult> {
	return postJson<ReextractResult>('/api/admin/photos/metadata/reextract');
}

/** A freshly generated AES-256 at-rest blob-encryption key (base64) plus a
 *  data-loss warning authored by the server. The `fingerprint` is the
 *  SSH-style colon-hex render the boot log / admin pair-chain / rotate
 *  reports all use — admins can paste the key into `.env`, restart, and
 *  check the fingerprint matches to confirm the key made it in intact. */
export interface GeneratedKey {
	key: string;
	fingerprint: string;
	warning: string;
}

/** Generate a random AES-256 key for at-rest blob encryption. */
export function generateEncryptionKey(): Promise<GeneratedKey> {
	return postJson<GeneratedKey>('/api/admin/settings/storage/generate-key');
}

// ── Drives ──────────────────────────────────────────────────────────────

/**
 * `GET /api/admin/drives` — every drive on the system, admin-only.
 *
 * Distinct from `listDrives()` in `$lib/api/endpoints/drives`, which is
 * the caller's own listing (filtered through `role_grants`). An admin
 * who creates a shared drive for someone else has no role on it, so
 * the user-facing listing would skip it — this endpoint returns
 * everything for the admin panel's "Drives" tab.
 */
export function listAllDrives(): Promise<Drive[]> {
	return apiJson<Drive[]>('/api/admin/drives', { credentials: 'same-origin' });
}

/**
 * `GET /api/admin/drives/{id}/members` — every role grant on a drive,
 * admin-only. The user-facing `/api/drives/{id}/members` requires
 * `Permission::Read` on the drive; an admin who created the drive
 * for someone else has no role on it and would hit a 404 there. This
 * endpoint reuses `list_grants_on_resource` with the admin guard at
 * the route edge, so the same `DriveMember` shape comes back.
 */
export function listDriveMembersAdmin(driveId: string): Promise<DriveMember[]> {
	return apiJson<DriveMember[]>(`/api/admin/drives/${encodeURIComponent(driveId)}/members`, {
		credentials: 'same-origin'
	});
}

/**
 * `POST /api/admin/drives/{id}/members` — add (or refresh) a member as
 * an admin, bypassing the per-drive `Manage` check. Personal-drive
 * guard + last-owner protection still apply. Throws on non-2xx.
 */
export async function addDriveMemberAdmin(
	driveId: string,
	subject: DriveMemberSubject,
	role: DriveRole,
	expiresAt?: string | null
): Promise<DriveMember> {
	const res = await apiFetch(`/api/admin/drives/${encodeURIComponent(driveId)}/members`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ subject, role, expires_at: expiresAt ?? null })
	});
	if (!res.ok) {
		let detail = '';
		try {
			const parsed = (await res.json()) as { error?: string; message?: string };
			detail = parsed.error ?? parsed.message ?? '';
		} catch {
			/* response body wasn't JSON */
		}
		throw new Error(detail || `add member failed: ${res.status}`);
	}
	return (await res.json()) as DriveMember;
}

/**
 * `DELETE /api/admin/drives/{id}/members/{kind}/{sid}` — remove a
 * member as an admin. Idempotent (removing a non-member returns 204).
 * Last-owner protection still applies (400 with `reason='last_owner'`).
 */
export async function removeDriveMemberAdmin(
	driveId: string,
	subject: DriveMemberSubject
): Promise<void> {
	const url =
		`/api/admin/drives/${encodeURIComponent(driveId)}/members/` +
		`${encodeURIComponent(subject.type)}/${encodeURIComponent(subject.id)}`;
	const res = await apiFetch(url, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) {
		let detail = '';
		try {
			const parsed = (await res.json()) as { error?: string; message?: string };
			detail = parsed.error ?? parsed.message ?? '';
		} catch {
			/* response body wasn't JSON */
		}
		throw new Error(detail || `remove member failed: ${res.status}`);
	}
}

/**
 * `DELETE /api/admin/drives/{id}` — admin-only drive delete (D3b).
 *
 * Bypasses the per-drive `Manage` check (the admin guard at the route
 * edge is the access control). The default-personal-drive guard and
 * the "drive must be empty" check still fire server-side — admins
 * can't accidentally wipe a populated drive or a user's home folder.
 * Throws on non-2xx so the caller can branch on `405` (default
 * personal) vs `409` (non-empty) when surfacing the failure.
 */
/**
 * `PATCH /api/drives/{id}/quota` — admin-only shared-drive quota
 * mutation (D4). `quotaBytes = null` or ≤ 0 → unlimited (the backend
 * normalises 0/negative to NULL).
 *
 * **Refuses personal drives** with HTTP 400 — the effective cap
 * comes from the owner user's `storage_quota_bytes` envelope, edit
 * via `setUserQuota` (`PUT /api/admin/users/{id}/quota`) instead.
 * Callers should gate the UI on `drive.kind === 'shared'` so users
 * never see the refusal.
 *
 * **Soft-quota semantic on shrink**: a new cap below current
 * `used_bytes` is accepted — the write-time gate then blocks new
 * writes until the drive shrinks back under. No existing content
 * is retroactively touched. Matches xfs/ext4 quota behaviour.
 *
 * Returns the persisted value (the backend's normalisation of the
 * input) so the caller can update local state without re-fetching.
 * Throws on non-2xx with the backend's error message when present.
 */
export async function updateDriveQuota(
	driveId: string,
	quotaBytes: number | null
): Promise<number | null> {
	const res = await apiFetch(`/api/drives/${encodeURIComponent(driveId)}/quota`, {
		method: 'PATCH',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ quota_bytes: quotaBytes })
	});
	if (!res.ok) {
		let detail = '';
		try {
			const parsed = (await res.json()) as { error?: string; message?: string };
			detail = parsed.error ?? parsed.message ?? '';
		} catch {
			/* response body wasn't JSON */
		}
		throw new Error(detail || `update drive quota failed: ${res.status}`);
	}
	const body = (await res.json()) as { quota_bytes: number | null };
	return body.quota_bytes;
}

export async function deleteDriveAdmin(driveId: string): Promise<void> {
	const res = await apiFetch(`/api/admin/drives/${encodeURIComponent(driveId)}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) {
		let detail = '';
		try {
			const parsed = (await res.json()) as { error?: string; message?: string };
			detail = parsed.error ?? parsed.message ?? '';
		} catch {
			/* response body wasn't JSON */
		}
		// 405 / 409 carry actionable messages from the backend; bubble them.
		throw new Error(detail || `delete drive failed: ${res.status}`);
	}
}

// ── Users ───────────────────────────────────────────────────────────────

/** List the compact rows rendered by the management table; full account
 * details remain available through {@link getUserAdmin}. */
export function listUsers(limit: number, offset: number): Promise<AdminUsersPage> {
	return apiJson<AdminUsersPage>(`/api/admin/users?limit=${limit}&offset=${offset}&summary=true`, {
		credentials: 'same-origin'
	});
}

/**
 * Admin-scoped single-user lookup — `GET /api/admin/users/{id}`.
 * Returns the full `User` DTO including `storage_quota_bytes` +
 * `storage_used_bytes` which the non-admin `/api/users/{id}`
 * response omits for privacy.
 *
 * Result promises are cached per id at module scope so multiple
 * callers for the same user (e.g. the admin drives table with N
 * personal drives owned by the same person) share one fetch. A
 * `null` result is cached too so a missing user isn't re-fetched
 * on every render.
 *
 * The cache is process-lifetime; a page navigation away and back
 * still sees the cached value. Callers that need to refresh (e.g.
 * after `setUserQuota`) should call `invalidateAdminUserCache`.
 */
const adminUserCache = new Map<string, Promise<User | null>>();

export function getUserAdmin(id: string): Promise<User | null> {
	const hit = adminUserCache.get(id);
	if (hit) return hit;
	const pending = (async (): Promise<User | null> => {
		try {
			return await apiJson<User>(`/api/admin/users/${encodeURIComponent(id)}`, {
				credentials: 'same-origin'
			});
		} catch {
			return null;
		}
	})();
	adminUserCache.set(id, pending);
	return pending;
}

/** Drop cached admin lookups so mutations (quota change, role change,
 *  delete) don't return stale data. Called with no arg = clear all,
 *  or with a specific user id to drop just that entry. */
export function invalidateAdminUserCache(userId?: string): void {
	if (userId) adminUserCache.delete(userId);
	else adminUserCache.clear();
}

export interface CreateUserInput {
	username: string;
	password: string;
	/** Optional — the backend auto-generates an address when null/empty. */
	email: string | null;
	role: string;
	quota_bytes: number;
}

export function createUser(input: CreateUserInput): Promise<void> {
	return mutate('/api/admin/users', 'POST', input);
}

export function setUserRole(userId: string, role: string): Promise<void> {
	return mutate(`/api/admin/users/${userId}/role`, 'PUT', { role });
}

export function setUserActive(userId: string, active: boolean): Promise<void> {
	return mutate(`/api/admin/users/${userId}/active`, 'PUT', { active });
}

export function setUserQuota(userId: string, quotaBytes: number): Promise<void> {
	return mutate(`/api/admin/users/${userId}/quota`, 'PUT', { quota_bytes: quotaBytes });
}

export function resetUserPassword(userId: string, newPassword: string): Promise<void> {
	return mutate(`/api/admin/users/${userId}/password`, 'PUT', { new_password: newPassword });
}

export function deleteUser(userId: string): Promise<void> {
	return mutate(`/api/admin/users/${userId}`, 'DELETE');
}

/**
 * Promote a currently-external (grant-only) user to an internal
 * account. The deployment must have magic-link login enabled — the
 * admin doesn't set the target's password, so the promoted user
 * needs some way to log in. Backend refuses with:
 *   * 400 — magic-link disabled deployment-wide
 *   * 403 — target is OIDC-linked
 *   * 404 — user not found
 *   * 409 — user is already internal
 */
export function promoteUserToInternal(userId: string): Promise<void> {
	return mutate(`/api/admin/users/${userId}/promote-to-internal`, 'POST');
}

// ── Dashboard ───────────────────────────────────────────────────────────

export interface DriveKindUsage {
	kind: 'personal' | 'shared';
	used_bytes: number;
	// null when there are no capped drives of this kind — the FE
	// hides the ratio and just renders "N unlimited"
	capped_quota_bytes: number | null;
	unlimited_count: number;
	capped_count: number;
}

export interface AdminDashboard {
	total_users: number;
	active_users: number;
	admin_users: number;
	server_version: string;
	drive_usage: DriveKindUsage[];
	auth_enabled: boolean;
	oidc_configured: boolean;
	quotas_enabled: boolean;
	registration_enabled?: boolean;
	users_over_80_percent: number;
	users_over_quota: number;
	// Backend physical accounting — omitted when the dedup service
	// is unavailable. Renders as "—" in that case.
	total_bytes_stored?: number;
	dedup_ratio?: number;
}

export function getDashboard(): Promise<AdminDashboard> {
	return apiJson<AdminDashboard>('/api/admin/dashboard', { credentials: 'same-origin' });
}

export function setRegistrationEnabled(enabled: boolean): Promise<void> {
	return mutate('/api/admin/settings/registration', 'PUT', { registration_enabled: enabled });
}

// ── SMTP ────────────────────────────────────────────────────────────────

export interface SmtpInfo {
	enabled: boolean;
	host: string;
	port: number;
	tls: string;
	from: string;
	user_state: string;
}

export function getSmtpInfo(): Promise<SmtpInfo> {
	return apiJson<SmtpInfo>('/api/admin/smtp/info', { credentials: 'same-origin' });
}

export interface SmtpTestResult {
	success: boolean;
	code?: string | number;
	message?: string;
	error?: string;
}

/**
 * Result of POST .../settings/storage/test. Combines reachability
 * (`connected` — HEAD bucket / statfs) with a full read/write round-
 * trip (`roundtrip_passed` — PUT + GET + verify + DELETE). Overall
 * pass = both true. Round-trip fields are absent when the round-trip
 * wasn't attempted (typically because reachability already failed).
 * `phase_reached` names the last successful round-trip step:
 * `initialize` | `put_ok` | `exists_ok` | `get_ok` | `verify_ok` |
 * `cleanup_ok`.
 */
export interface StorageTestResult {
	connected?: boolean;
	success?: boolean;
	backend_type?: string;
	available_bytes?: number | null;
	message?: string;
	roundtrip_passed?: boolean;
	phase_reached?: string;
	bytes_written?: number;
	bytes_read?: number;
	roundtrip_elapsed_ms?: number;
	cleanup_ok?: boolean;
}

export async function sendSmtpTest(to: string): Promise<SmtpTestResult> {
	const res = await apiFetch('/api/admin/smtp/test', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ to })
	});
	if (res.status === 503)
		return { success: false, message: 'SMTP is not configured on this server.' };
	return (await res.json().catch(() => ({ success: false }))) as SmtpTestResult;
}

// ── OIDC settings ─────────────────────────────────────────────────────────

export interface OidcSettings {
	enabled: boolean;
	issuer_url: string;
	client_id: string;
	scopes: string | null;
	auto_provision: boolean;
	admin_groups: string | null;
	disable_password_login: boolean;
	provider_name: string | null;
	callback_url?: string;
	client_secret_set?: boolean;
	env_overrides?: string[];
}

export interface OidcTestResult {
	success: boolean;
	message: string;
	issuer?: string;
	authorization_endpoint?: string;
	provider_name_suggestion?: string;
}

export function getOidcSettings(): Promise<OidcSettings> {
	return apiJson<OidcSettings>('/api/admin/settings/oidc', { credentials: 'same-origin' });
}

export async function testOidc(issuerUrl: string): Promise<OidcTestResult> {
	const res = await apiFetch('/api/admin/settings/oidc/test', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ issuer_url: issuerUrl })
	});
	return (await res
		.json()
		.catch(() => ({ success: false, message: 'Request failed' }))) as OidcTestResult;
}

export function saveOidc(body: Record<string, unknown>): Promise<void> {
	return mutate('/api/admin/settings/oidc', 'PUT', body);
}

// ── Storage settings + migration ───────────────────────────────────────────

/**
 * One `<cipher>:<key>` pair rendered for the admin storage panel.
 * Never carries key material — only cipher name + SSH-style
 * fingerprint safe to show operators.
 */
export interface StorageEncryptionPair {
	/** `"aes-256-gcm"` for a real-cipher pair, `"none"` for a `none:` sentinel. */
	cipher: string;
	/**
	 * SSH-style colon-hex 8-byte fingerprint of the key. Matches
	 * `backend_rotate`'s `head_key_fp` and the `oxicloud --fingerprint`
	 * CLI output — enables one-glance identification of which key is
	 * which. `undefined` for `none:` pairs (no key material).
	 */
	fingerprint?: string;
	/** True for the LAST pair in the list — the write pair (head). */
	is_head: boolean;
}

export interface StorageEntrySummary {
	name: string;
	backend: string;
	is_active: boolean;
	encryption_enabled: boolean;
	/** Human-readable physical hint (root_dir / bucket / container). */
	location_hint?: string | null;
	/**
	 * Ordered pair-list summary — oldest first, head last. Empty when
	 * the entry has no `_ENCRYPTION_KEY` declared at all. Used by the
	 * entry card to render the pair chain so admins can identify
	 * which key is the current head + which are safe to remove after
	 * a completed rotation.
	 */
	encryption_pairs: StorageEncryptionPair[];
}

export interface StorageSettings {
	// Live stats — what the running process reports.
	current_backend?: string;
	total_blobs?: number;
	total_bytes_stored?: number;
	dedup_ratio?: number;
	// Multi-entry view (slice 6 of docs/plan/storage-multi-entry.md).
	// `entries` is empty for the legacy zero-entries path.
	entries?: StorageEntrySummary[];
	active_entry_name?: string;
	migration_readonly?: boolean;
}

export function getStorageSettings(): Promise<StorageSettings> {
	return apiJson<StorageSettings>('/api/admin/settings/storage', { credentials: 'same-origin' });
}

export function saveStorage(body: Record<string, unknown>): Promise<void> {
	return mutate('/api/admin/settings/storage', 'PUT', body);
}

export async function testStorage(body: Record<string, unknown>): Promise<StorageTestResult> {
	const res = await apiFetch('/api/admin/settings/storage/test', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	return (await res.json().catch(() => ({ connected: false }))) as StorageTestResult;
}

export interface MigrationStatus {
	status: 'idle' | 'running' | 'paused' | 'completed' | 'failed';
	total_blobs: number;
	migrated_blobs: number;
	migrated_bytes: number;
	throughput_bytes_per_sec?: number;
	failed_blobs?: string[];
}

export function getMigration(): Promise<MigrationStatus> {
	return apiJson<MigrationStatus>('/api/admin/storage/migration', { credentials: 'same-origin' });
}

export function migrationAction(
	action: 'start' | 'pause' | 'resume',
	targetName?: string
): Promise<void> {
	// `complete` was retired when the migration became a recoverable
	// job — Completed is the terminal `RunSummary.status`; there's
	// nothing left to acknowledge. Post-migration cutover happens on
	// operator restart (server re-boots on active_backend_name = the
	// new entry; the boot-clear rule drops migration_readonly).
	//
	// `start` REQUIRES `targetName` in multi-entry mode — the backend
	// rejects an unnamed start with 400 (see StartMigrationDto).
	// `pause` and `resume` take no body (resume reads target_name
	// from the paused run's params).
	const body: Record<string, unknown> =
		action === 'start' ? { target_name: targetName ?? '', concurrency: 4 } : {};
	return mutate(`/api/admin/storage/migration/${action}`, 'POST', body);
}

/**
 * K4 (storage-key-rotation): trigger `backend_rotate` on a specific
 * storage entry. Normalises every blob on `<name>` to that entry's
 * head-pair format: legacy → v1, plaintext ↔ encrypted, old-key →
 * new-key. Fire-and-forget — poll `GET /api/admin/jobs/backend_rotate`
 * for status.
 *
 * Backend: `POST /api/admin/storage/entries/{name}/rotate`
 * (`admin_handler::trigger_backend_rotate`). Refuses (400) on unknown
 * entry name or when a `backend_rotate` / `backend_migration` run is
 * already in flight.
 */
export function rotateStorageEntry(name: string): Promise<void> {
	return mutate(`/api/admin/storage/entries/${encodeURIComponent(name)}/rotate`, 'POST', undefined);
}

// verifyMigration + MigrationVerifyResult retired in slice 7 of
// docs/plan/storage-multi-entry.md — the corresponding backend
// endpoint's sample-based check is superseded by
// `POST /api/admin/jobs/blobs_consistency/trigger?storage=<name>`,
// which does a full walk against any named entry and integrates
// with the standard runs / findings admin surface. Trigger from
// the Jobs tab; the Storage tab drops the "Verify integrity"
// button.

// ── Plugins ─────────────────────────────────────────────────────────────

export interface PluginInfo {
	id: string;
	name: string;
	version?: string;
	enabled: boolean;
	description?: string;
	abi?: string | number;
	subscriptions?: string[];
}

export interface PluginRetention {
	retention_days: number;
	max_bytes: number;
}

/**
 * Install a plugin from a .zip bundle. The browser sets the multipart
 * Content-Type (with boundary) — do not override it here.
 */
export async function installPlugin(bundle: File): Promise<PluginInfo> {
	const form = new FormData();
	form.append('bundle', bundle);
	const res = await apiFetch('/api/admin/plugins', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...getCsrfHeaders() },
		body: form
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { message?: string };
		throw new Error(e.message || `install failed: ${res.status}`);
	}
	return (await res.json()) as PluginInfo;
}

export async function getPluginRetention(id: string): Promise<PluginRetention | null> {
	const res = await apiFetch(`/api/admin/plugins/${encodeURIComponent(id)}/retention`, {
		credentials: 'same-origin'
	});
	if (!res.ok) return null;
	return (await res.json()) as PluginRetention;
}

export function savePluginRetention(id: string, r: PluginRetention): Promise<void> {
	return mutate(`/api/admin/plugins/${encodeURIComponent(id)}/retention`, 'PUT', r);
}

export function clearPluginLogs(id: string): Promise<void> {
	return mutate(`/api/admin/plugins/${encodeURIComponent(id)}/logs`, 'DELETE');
}

export interface PluginsResult {
	/** false when the plugin subsystem is disabled (server returns 503). */
	available: boolean;
	enabled?: boolean;
	plugins: PluginInfo[];
}

export async function listPlugins(): Promise<PluginsResult> {
	const res = await apiFetch('/api/admin/plugins', { credentials: 'same-origin' });
	if (res.status === 503) return { available: false, plugins: [] };
	if (!res.ok) throw new Error(`plugins failed: ${res.status}`);
	const data = (await res.json()) as { enabled?: boolean; plugins?: PluginInfo[] };
	return { available: true, enabled: data.enabled, plugins: data.plugins ?? [] };
}

export function setPluginEnabled(id: string, enabled: boolean): Promise<void> {
	return mutate(`/api/admin/plugins/${encodeURIComponent(id)}/enabled`, 'PUT', { enabled });
}

export function deletePlugin(id: string): Promise<void> {
	return mutate(`/api/admin/plugins/${encodeURIComponent(id)}`, 'DELETE');
}

export interface PluginLogEntry {
	timestamp?: string;
	ts?: string;
	level?: string;
	message?: string;
	/** Streamed-entry message field (SSE / persisted logs use `msg`). */
	msg?: string;
	/** "outcome" | "log" — outcome entries carry a `reason`. */
	kind?: string;
	reason?: string;
	invocation_id?: string;
	[k: string]: unknown;
}

export interface PluginLogPage {
	total: number;
	entries: PluginLogEntry[];
}

export function getPluginLogs(
	id: string,
	opts: { limit?: number; offset?: number; level?: string; search?: string } = {}
): Promise<PluginLogPage> {
	const params = new URLSearchParams();
	params.set('limit', String(opts.limit ?? 50));
	params.set('offset', String(opts.offset ?? 0));
	if (opts.level) params.set('level', opts.level);
	if (opts.search) params.set('search', opts.search);
	return apiJson<PluginLogPage>(`/api/admin/plugins/${encodeURIComponent(id)}/logs?${params}`, {
		credentials: 'same-origin'
	});
}

// ── External file mounts ────────────────────────────────────────────────────

/** A configured external mount as returned by the admin API. */
export interface ExternalMount {
	mount_folder_id: string;
	name: string;
	kind: string;
	owner_id: string;
	read_only: boolean;
	drive_id: string;
	mount_path: string;
	config: Record<string, unknown>;
}

/** Request body for creating an external mount. */
export interface CreateExternalMountInput {
	name: string;
	host_path: string;
	kind?: string;
	read_only?: boolean;
}

/** GET /api/admin/external-mounts — list all configured mounts. */
export function listExternalMounts(): Promise<ExternalMount[]> {
	return apiJson<ExternalMount[]>('/api/admin/external-mounts', {
		credentials: 'same-origin'
	});
}

/** POST /api/admin/external-mounts — create a mount in the admin's drive. */
export async function createExternalMount(input: CreateExternalMountInput): Promise<ExternalMount> {
	const res = await apiFetch('/api/admin/external-mounts', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(input)
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { message?: string };
		throw new Error(e.message || `Create mount failed: ${res.status}`);
	}
	return (await res.json()) as ExternalMount;
}

/** DELETE /api/admin/external-mounts/{id} — remove a mount (host content kept). */
export function deleteExternalMount(mountFolderId: string): Promise<void> {
	return mutate(`/api/admin/external-mounts/${mountFolderId}`, 'DELETE');
}
