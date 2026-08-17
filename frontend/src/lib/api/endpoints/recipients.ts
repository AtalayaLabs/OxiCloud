/**
 * Recipient search for the share People tab — system users (via the system
 * address book) + groups (via /api/groups/search) + a synthesized "invite by
 * email" suggestion when the query parses as an email. Ported from the original
 * shareModal recipient autocomplete (addressBook.searchContacts + _searchGroups
 * + _looksLikeEmail).
 */
import { apiFetch } from '$lib/api/client';
import { session } from '$lib/stores/session.svelte';
import type { SubjectType } from './grants';

export interface Recipient {
	type: Extract<SubjectType, 'user' | 'group' | 'email' | 'opencloudmesh'>;
	/** For email recipients this is the normalised email; for users/groups, the UUID. */
	id: string;
	label: string;
	sublabel?: string;
}

interface Contact {
	id: string;
	first_name?: string;
	last_name?: string;
	full_name?: string;
	email?: Array<{ email: string; is_primary?: boolean }>;
}

interface GroupResult {
	id: string;
	name: string;
}

/**
 * Permissive client-side email check — matches a non-whitespace local part, an
 * `@`, and a domain with a dot. The server's `normalize_email` is authoritative;
 * this just decides whether to surface the synthetic invite-by-email row.
 */
function looksLikeEmail(q: string): boolean {
	return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(q);
}

// The system book lists all users; we filter client-side (matches the original).
// Two caches because the backend response differs (default excludes the caller,
// `?include_self=1` returns them). Keying by flag avoids one variant overwriting
// the other.
let contactCache: Contact[] | null = null;
let contactCacheWithSelf: Contact[] | null = null;
/** `false` once we confirm the system address book is unavailable. */
let directoryAvailable: boolean | null = null;

async function systemContacts(includeSelf = false): Promise<Contact[]> {
	const cached = includeSelf ? contactCacheWithSelf : contactCache;
	if (cached) return cached;
	try {
		// `?include_self=true` (not `=1`) — Axum's `Query` extractor uses
		// `serde_urlencoded`, which only deserialises `"true"`/`"false"`
		// for `bool`. Sending `=1` would 400 before the handler runs.
		const url = includeSelf
			? '/api/address-books/system/contacts?include_self=true'
			: '/api/address-books/system/contacts';
		const res = await apiFetch(url, { credentials: 'same-origin' });
		if (!res.ok) {
			directoryAvailable = false;
			if (includeSelf) {
				contactCacheWithSelf = [];
				return contactCacheWithSelf;
			}
			contactCache = [];
			return contactCache;
		}
		directoryAvailable = true;
		const list = (await res.json()) as Contact[];
		if (includeSelf) contactCacheWithSelf = list;
		else contactCache = list;
		return list;
	} catch {
		directoryAvailable = false;
		if (includeSelf) {
			contactCacheWithSelf = [];
			return contactCacheWithSelf;
		}
		contactCache = [];
		return contactCache;
	}
}

/**
 * Whether the system user directory is reachable. Returns `true` until proven
 * otherwise so callers degrade gracefully; call `ensureResolvers()` first to
 * get an accurate answer.
 */
export function isDirectoryAvailable(): boolean {
	return directoryAvailable !== false;
}

function contactLabel(c: Contact): { label: string; email: string } {
	const name = [c.first_name, c.last_name].filter(Boolean).join(' ') || c.full_name || '';
	const email = c.email?.find((e) => e.is_primary)?.email ?? c.email?.[0]?.email ?? '';
	return { label: name || email || c.id, email };
}

async function searchGroups(q: string): Promise<Recipient[]> {
	try {
		const res = await apiFetch(`/api/groups/search?q=${encodeURIComponent(q)}&limit=8`, {
			credentials: 'same-origin'
		});
		if (!res.ok) return [];
		const groups = (await res.json()) as GroupResult[];
		return groups.map((g) => ({ type: 'group' as const, id: g.id, label: g.name }));
	} catch {
		return [];
	}
}

// ── Label resolution for existing grants (subject id → display name) ────────
let groupCache: Map<string, string> | null = null;

async function loadGroups(): Promise<Map<string, string>> {
	if (groupCache) return groupCache;
	groupCache = new Map();
	try {
		const res = await apiFetch('/api/groups/search?q=&limit=200', { credentials: 'same-origin' });
		if (res.ok) {
			for (const g of (await res.json()) as GroupResult[]) groupCache.set(g.id, g.name);
		}
	} catch {
		/* leave empty */
	}
	return groupCache;
}

/** Preload the user + group caches so grant rows can show names. */
export async function ensureResolvers(): Promise<void> {
	await Promise.all([systemContacts(), loadGroups()]);
}

// O(1) id→contact index over `contactCache`, built once per cache identity.
// `resolveLabel`/`resolveRecipient` run per rendered grant row on /shared —
// the previous `contactCache.find(...)` linear scan made each render frame
// O(rows × directory size).
let contactById: Map<string, Contact> | null = null;
let contactByIdSource: Contact[] | null = null;

function contactIndex(): Map<string, Contact> | null {
	if (!contactCache) return null;
	if (!contactById || contactByIdSource !== contactCache) {
		contactById = new Map(contactCache.map((c) => [c.id, c]));
		contactByIdSource = contactCache;
	}
	return contactById;
}

/** Resolve a subject id to a display label using the preloaded caches. */
export function resolveLabel(type: 'user' | 'group', id: string): string {
	if (type === 'group') return groupCache?.get(id) ?? id;
	const c = contactIndex()?.get(id);
	return c ? contactLabel(c).label : id;
}

/** Resolve a subject id to a label + sublabel (email) for member vignettes. */
export function resolveRecipient(type: 'user' | 'group', id: string): Recipient {
	if (type === 'group') {
		return { type: 'group', id, label: groupCache?.get(id) ?? id };
	}
	const c = contactIndex()?.get(id);
	if (!c) return { type: 'user', id, label: id };
	const { label, email } = contactLabel(c);
	return { type: 'user', id, label, sublabel: email };
}

/**
 * Combined user + group results matching the query (case-insensitive), plus a
 * synthetic invite-by-email suggestion when the query is an email that no
 * contact already owns. Capped at 8 combined (groups, then users, then email).
 *
 * `includeSelf` defaults to `false` — the share modal excludes the current
 * caller from the picker because "you can't share with yourself". The admin
 * drive-owners surface flips it on: an admin legitimately needs to add
 * themselves (or anyone) as Owner without that personal-share restriction.
 */
export async function searchRecipients(
	query: string,
	{ includeSelf = false }: { includeSelf?: boolean } = {}
): Promise<Recipient[]> {
	const q = query.toLowerCase().trim();
	if (!q) return [];
	const currentUserId = session.user?.id ?? null;
	const [contacts, groups] = await Promise.all([systemContacts(includeSelf), searchGroups(q)]);
	const matched = contacts
		.filter((c) => includeSelf || c.id !== currentUserId)
		.map((c) => ({ c, ...contactLabel(c) }))
		.filter(
			({ label, email }) => label.toLowerCase().includes(q) || email.toLowerCase().includes(q)
		);
	const users: Recipient[] = matched.map(({ c, label, email }) => ({
		type: 'user' as const,
		id: c.id,
		label,
		sublabel: email
	}));

	const syntheticItems: Recipient[] = [];
	if (looksLikeEmail(q)) {
		const exists = matched.some(({ email }) => email.toLowerCase() === q);
		if (!exists) {
      syntheticItems.push({ type: 'email', id: q, label: q });
      syntheticItems.push({ type: 'opencloudmesh', id: q, label: q });
    }
	}

	return [...groups, ...users, ...syntheticItems].slice(0, 8);
}
