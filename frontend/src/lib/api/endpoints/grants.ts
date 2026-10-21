/** Sharing (ReBAC grants) endpoints — ported from model/grants.js. */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { ItemType } from '$lib/api/types';
import type {
	IncomingGrantOrderBy,
	OutgoingGrantOrderBy
} from '$lib/api/generated/openapi/types.gen';
import type { ResourceBody, ResourcePage } from './resources';

/**
 * Resource kinds the `/api/grants` family addresses. File/folder grants flow
 * through the cascade engine; drive grants flow through
 * `DriveManagementService` server-side, which layers personal-drive guard +
 * last-owner protection on top of the same role-grant write. Either way the
 * wire shape is identical, so the FE helpers below accept all three.
 */
export type GrantResourceType = ItemType | 'drive';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

export type SubjectType = 'user' | 'group' | 'email' | 'token';
/** Roles the share UI exposes. The backend role enum also has `commenter` and
 * `contributor`, which {@link displayRole} collapses to the nearest of these. */
export type ShareRole = 'viewer' | 'editor' | 'owner';

export interface GrantSubject {
	type: SubjectType;
	id: string;
}

/**
 * Subject shape accepted by `POST /api/grants`. The `email` variant feeds the
 * invite-by-email flow — the server resolves it to (or provisions) an external
 * user. Mirrors the backend `SubjectInputDto`.
 */
export type GrantSubjectInput =
	| { type: 'user'; id: string }
	| { type: 'group'; id: string }
	| { type: 'token'; id: string }
	| { type: 'email'; email: string };

/**
 * One role grant for a (subject, resource). Role-keyed since the role-grants
 * migration: each row carries an explicit `role` (the backend enum, which may
 * be `owner`/`editor`/`viewer`/`commenter`/`contributor`).
 */
export interface Grant {
	id: string;
	granted_at?: string;
	granted_by?: string;
	subject: GrantSubject;
	role: string;
	resource: { type: GrantResourceType; id: string };
	expires_at?: string | null;
}

// ── Notification outcomes (PR N1/N2) ─────────────────────────────────────────

export interface NotifyOutcome {
	kind: 'sent' | 'coalesced' | 'rate_limited' | 'not_applicable';
	detail?: string;
	last_sent_at?: string;
	retry_after_secs?: number;
	reason?: string;
}

export interface NotifyOutcomeSet {
	total_recipients: number;
	outcomes: NotifyOutcome[];
}

export interface CreateGrantResponse {
	grants: Grant[];
	notification: NotifyOutcomeSet;
}

// `displayRole` moved to `$lib/utils/grantFormat.ts`.
// `expiryToIso` / `todayIso` moved to `$lib/utils/dateInput.ts` —
// they're generic date-picker helpers, not grant-specific.

export function fetchGrantsForResource(type: GrantResourceType, id: string): Promise<Grant[]> {
	const params = new URLSearchParams({ resource_type: type, resource_id: id });
	return apiJson<Grant[]>(`/api/grants?${params}`, { credentials: 'same-origin' });
}

export async function createGrant(
	subject: GrantSubjectInput,
	resource: { type: GrantResourceType; id: string },
	role: ShareRole,
	expiresAt?: string | null
): Promise<CreateGrantResponse> {
	const res = await apiFetch('/api/grants', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ subject, resource, role, expires_at: expiresAt ?? null })
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { error?: string };
		throw new Error(e.error || `create grant failed: ${res.status}`);
	}
	return (await res.json()) as CreateGrantResponse;
}

export async function updateGrantRole(
	subject: GrantSubject,
	resource: { type: GrantResourceType; id: string },
	role: ShareRole,
	expiresAt?: string | null
): Promise<void> {
	const res = await apiFetch('/api/grants/role', {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ subject, resource, role, expires_at: expiresAt ?? null })
	});
	if (!res.ok) throw new Error(`update role failed: ${res.status}`);
}

export async function revokeGrant(grantId: string): Promise<void> {
	const res = await apiFetch(`/api/grants/${encodeURIComponent(grantId)}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`revoke grant failed: ${res.status}`);
}

/**
 * Resend / send a share notification for a single grant.
 * `POST /api/grants/{id}/notify`. Returns the aggregated outcome set, or a
 * `rate_limited` summary when the whole call was rate-limited (HTTP 429).
 */
export async function notifyGrantRecipient(grantId: string): Promise<NotifyOutcomeSet> {
	const res = await apiFetch(`/api/grants/${encodeURIComponent(grantId)}/notify`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (res.status === 204) return { total_recipients: 0, outcomes: [] };
	if (res.status === 429) {
		return { total_recipients: 1, outcomes: [{ kind: 'rate_limited' }] };
	}
	if (res.ok) return (await res.json()) as NotifyOutcomeSet;
	throw new Error(`notify failed: ${res.status}`);
}

export interface IncomingGrantItem {
	resource_type: GrantResourceType;
	resource: ResourceBody;
	granted_by?: string;
	granted_at?: string;
	role?: string;
}

/** One (subject, permissions) entry within an outgoing resource item. */
export interface OutgoingResourceGrant {
	grant_id: string;
	subject_type: 'user' | 'group' | 'token';
	subject_id: string;
	subject_display: string;
	role: ShareRole;
	granted_at: string;
	expires_at?: string | null;
	has_password: boolean;
	is_external: boolean;
}

export interface OutgoingGrantItem {
	resource_type: GrantResourceType;
	resource: ResourceBody;
	first_shared_at?: string;
	/** One entry per (subject, permissions) pair. */
	grants: OutgoingResourceGrant[];
}

// §7 part 3g: generic over the axis, because the two grant listings accept
// DISJOINT sets — part 3f split the backend query struct for the same
// reason. `IncomingGrantOrderBy` is granted_at|granted_by|name|type;
// `OutgoingGrantOrderBy` is first_shared_at|name|type|subject|role. Only
// `name` and `type` overlap, so one shared `string` could express neither,
// and sending an axis to the wrong endpoint is now a build error here
// instead of a 400 at runtime.
interface GrantsPageOpts<Axis extends string = string> {
	cursor?: string;
	orderBy?: Axis;
	limit?: number;
	reverse?: boolean;
	resourceTypes?: ItemType[];
}

export type SharedWithMePageOpts = GrantsPageOpts<IncomingGrantOrderBy>;
export type MySharesPageOpts = GrantsPageOpts<OutgoingGrantOrderBy>;

function params(opts: GrantsPageOpts<string>): string {
	const { cursor, orderBy, limit = 50, reverse = false, resourceTypes } = opts;
	const p = new URLSearchParams({ limit: String(limit) });
	if (resourceTypes?.length) p.set('resource_types', resourceTypes.join(','));
	if (cursor) p.set('cursor', cursor);
	// §7: the wire name is `order_by`, matching every other paged endpoint.
	// These two grant listings were the only ones that had shipped as
	// `sort_by`; renamed with no deprecation window, same call as §6 — the
	// API is recent and this SPA is its only consumer.
	if (orderBy) p.set('order_by', orderBy);
	if (reverse) p.set('reverse', 'true');
	return p.toString();
}

export async function fetchSharedWithMe(
	opts: SharedWithMePageOpts = {}
): Promise<ResourcePage<IncomingGrantItem>> {
	const res = await apiFetch(
		`/api/grants/incoming/resources?${params({ resourceTypes: ['file', 'folder'], ...opts })}`,
		{ credentials: 'same-origin' }
	);
	if (!res.ok) throw new Error(`shared-with-me failed: ${res.status}`);
	return (await res.json()) as ResourcePage<IncomingGrantItem>;
}

export async function fetchMyShares(
	opts: MySharesPageOpts = {}
): Promise<ResourcePage<OutgoingGrantItem>> {
	const res = await apiFetch(`/api/grants/outgoing/resources?${params(opts)}`, {
		credentials: 'same-origin'
	});
	if (!res.ok) throw new Error(`my-shares failed: ${res.status}`);
	return (await res.json()) as ResourcePage<OutgoingGrantItem>;
}
