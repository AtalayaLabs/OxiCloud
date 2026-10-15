/** Group (ReBAC) endpoints — ported from model/groups.js. */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';

const JSON_HEADERS = { 'Content-Type': 'application/json' };
const enc = encodeURIComponent;

/**
 * Well-known UUID of the predefined "Internal" virtual group (matches the
 * Rust constant `INTERNAL_GROUP_ID` in `src/domain/entities/subject_group.rs`
 * and the legacy `model/groups.js`).
 */
export const INTERNAL_GROUP_ID = '00000000-0000-0000-0000-000000000001';

// `VIRTUAL_NAME_KEYS` + `VIRTUAL_DESC_KEYS` + the three
// `group{DisplayName,Description,IconName}` helpers moved to
// `$lib/utils/groupFormat.ts`. The `INTERNAL_GROUP_ID` constant
// stays here — it's a shared UUID the API-call helpers also use.

export interface GroupItem {
	id: string;
	name: string;
	description?: string | null;
	member_count?: number;
	is_virtual?: boolean;
	can_manage?: boolean;
}

/** The members endpoint returns a tagged union: `{ kind: 'user' | 'group', id }`. */
export interface GroupMember {
	kind: 'user' | 'group';
	id: string;
}

async function mutate(url: string, method: string, body?: unknown): Promise<void> {
	const res = await apiFetch(url, {
		method,
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: body === undefined ? undefined : JSON.stringify(body)
	});
	if (!res.ok) throw new Error(`${method} ${url} failed: ${res.status}`);
}

/** A single page of groups plus the server-reported total (for "Load more"). */
export interface GroupPage {
	items: GroupItem[];
	total: number;
}

/**
 * Fetch one page of groups. The list endpoint may return an array or
 * `{ groups | items, total }`. When no total is provided we fall back to the
 * page length so pagination collapses gracefully to a single page.
 */
export async function listGroupsPage(limit = 50, offset = 0, q?: string): Promise<GroupPage> {
	const params = new URLSearchParams({ limit: String(limit), offset: String(offset) });
	if (q) params.set('q', q);
	const data = await apiJson<
		GroupItem[] | { groups?: GroupItem[]; items?: GroupItem[]; total?: number }
	>(`/api/groups?${params}`, { credentials: 'same-origin' });
	if (Array.isArray(data)) return { items: data, total: offset + data.length };
	const items = data.groups ?? data.items ?? [];
	return { items, total: data.total ?? offset + items.length };
}

/** Convenience wrapper returning just the items of the first page. */
export async function listGroups(limit = 50, offset = 0, q?: string): Promise<GroupItem[]> {
	return (await listGroupsPage(limit, offset, q)).items;
}

export function createGroup(name: string, description?: string | null): Promise<void> {
	return mutate('/api/groups', 'POST', { name, description: description ?? null });
}

export function renameGroup(id: string, name: string): Promise<void> {
	return mutate(`/api/groups/${enc(id)}`, 'PATCH', { name });
}

export function deleteGroup(id: string): Promise<void> {
	return mutate(`/api/groups/${enc(id)}`, 'DELETE');
}

export function listMembers(id: string): Promise<GroupMember[]> {
	return apiJson<GroupMember[]>(`/api/groups/${enc(id)}/members`, { credentials: 'same-origin' });
}

export function addUserMember(groupId: string, userId: string): Promise<void> {
	return mutate(`/api/groups/${enc(groupId)}/members`, 'POST', { user_id: userId });
}

/** Add another group as a nested member. Backend enforces cycle + depth limits. */
export function addGroupMember(groupId: string, memberGroupId: string): Promise<void> {
	return mutate(`/api/groups/${enc(groupId)}/members`, 'POST', { group_id: memberGroupId });
}

export function removeUserMember(groupId: string, userId: string): Promise<void> {
	return mutate(`/api/groups/${enc(groupId)}/members/user/${enc(userId)}`, 'DELETE');
}

export function removeGroupMember(groupId: string, memberGroupId: string): Promise<void> {
	return mutate(`/api/groups/${enc(groupId)}/members/group/${enc(memberGroupId)}`, 'DELETE');
}
