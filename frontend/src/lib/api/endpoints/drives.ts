/**
 * Drives endpoints. D0 ships read-only listing; D2 adds the membership API;
 * D3a adds the create-shared-drive flow.
 *
 * Consumers usually go through the `drives` store (`$lib/stores/drives.svelte`)
 * which dedupes the request and caches the list — touch this module directly
 * only when bypassing the cache is intentional (e.g. an explicit refresh).
 */
import { apiErrorFromResponse, apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type {
	CreateDriveBody,
	Drive,
	DriveMember,
	DriveMemberSubject,
	DrivePolicies,
	DrivePoliciesPartial,
	DriveRole
} from '$lib/api/types';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/** `GET /api/drives` — every drive the caller can read, default first by convention. */
export function listDrives(): Promise<Drive[]> {
	return apiJson<Drive[]>('/api/drives', { credentials: 'same-origin' });
}

/**
 * `POST /api/drives` — create a drive (D3a). Today only `kind: 'shared'` is
 * implemented; `kind: 'personal'` is accepted on the wire but returns 501.
 * Admin-only at the server; callers should already have gated the UI on
 * `session.user?.role === 'admin'`. Throws on non-2xx with the server's
 * error body parsed where possible.
 */
export async function createDrive(body: CreateDriveBody): Promise<Drive> {
	const res = await apiFetch('/api/drives', {
		method: 'POST',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		credentials: 'same-origin',
		body: JSON.stringify(body)
	});
	if (!res.ok) throw await apiErrorFromResponse(res);
	return (await res.json()) as Drive;
}

/** `GET /api/drives/{id}/members` — every role grant on the drive. */
export function listDriveMembers(driveId: string): Promise<DriveMember[]> {
	return apiJson<DriveMember[]>(`/api/drives/${encodeURIComponent(driveId)}/members`, {
		credentials: 'same-origin'
	});
}

/**
 * `POST /api/drives/{id}/members` — add a member (or refresh an existing
 * subject's role; the underlying `set_role` is idempotent via UNIQUE
 * `(subject, resource)`).
 *
 * Refused with 405 on personal drives (immutable membership) and 400 if a
 * last-owner demotion would orphan a shared drive.
 */
export async function addDriveMember(
	driveId: string,
	subject: DriveMemberSubject,
	role: DriveRole,
	expiresAt?: string | null
): Promise<DriveMember> {
	const res = await apiFetch(`/api/drives/${encodeURIComponent(driveId)}/members`, {
		method: 'POST',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		credentials: 'same-origin',
		body: JSON.stringify({ subject, role, expires_at: expiresAt ?? null })
	});
	if (!res.ok) throw await apiErrorFromResponse(res);
	return (await res.json()) as DriveMember;
}

/**
 * `PATCH /api/drives/{id}/members/{kind}/{sid}` — change a member's role.
 * Same guards as `addDriveMember` apply.
 */
export async function updateDriveMember(
	driveId: string,
	subject: DriveMemberSubject,
	role: DriveRole,
	expiresAt?: string | null
): Promise<DriveMember> {
	const url =
		`/api/drives/${encodeURIComponent(driveId)}/members/` +
		`${encodeURIComponent(subject.type)}/${encodeURIComponent(subject.id)}`;
	const res = await apiFetch(url, {
		method: 'PATCH',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		credentials: 'same-origin',
		body: JSON.stringify({ role, expires_at: expiresAt ?? null })
	});
	if (!res.ok) throw await apiErrorFromResponse(res);
	return (await res.json()) as DriveMember;
}

/**
 * `DELETE /api/drives/{id}` — Owner-only drive delete (D3b).
 *
 * Refused with `405` for the default Personal drive and `409` for a
 * non-empty drive (caller must move/trash content first). Throws on
 * non-2xx with the server's detail message when present so the caller
 * can decide whether to surface a confirmation prompt vs an error.
 */
export async function deleteDrive(driveId: string): Promise<void> {
	const res = await apiFetch(`/api/drives/${encodeURIComponent(driveId)}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw await apiErrorFromResponse(res);
}

/**
 * `PATCH /api/drives/{id}/policies` — update drive policies (D5).
 *
 * **OxiCloud-admin only.** Owners cannot mutate policies — the carve-out
 * exists because policies are a compliance surface (an owner who could
 * flip them would defeat the gates by disabling, sharing, re-enabling).
 * Non-admin callers receive 404 (anti-enum). The frontend only surfaces
 * this from the admin panel.
 *
 * Body is a partial — keys not present are left untouched at the JSONB
 * merge layer. Returns the post-merge typed view.
 */
export async function updateDrivePolicies(
	driveId: string,
	partial: DrivePoliciesPartial
): Promise<DrivePolicies> {
	const res = await apiFetch(`/api/drives/${encodeURIComponent(driveId)}/policies`, {
		method: 'PATCH',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		credentials: 'same-origin',
		body: JSON.stringify(partial)
	});
	if (!res.ok) throw await apiErrorFromResponse(res);
	return (await res.json()) as DrivePolicies;
}

/**
 * `DELETE /api/drives/{id}/members/{kind}/{sid}` — remove a member.
 * Idempotent (removing a non-member returns 204). Refused with 400 if it
 * would leave a shared drive without an owner.
 */
export async function removeDriveMember(
	driveId: string,
	subject: DriveMemberSubject
): Promise<void> {
	const url =
		`/api/drives/${encodeURIComponent(driveId)}/members/` +
		`${encodeURIComponent(subject.type)}/${encodeURIComponent(subject.id)}`;
	const res = await apiFetch(url, {
		method: 'DELETE',
		headers: getCsrfHeaders(),
		credentials: 'same-origin'
	});
	if (!res.ok) throw await apiErrorFromResponse(res);
}
