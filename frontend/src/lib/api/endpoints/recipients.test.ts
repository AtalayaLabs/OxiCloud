import { describe, it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// The caller's own id, for the "can't share with yourself" filter. Mocked
// because the store reads a live session this module never establishes.
vi.mock('$lib/stores/session.svelte', () => ({ session: { user: { id: 'me' } } }));
import { apiFetch } from '$lib/api/client';
import {
	isDirectoryAvailable,
	resolveLabel,
	resolveRecipient,
	searchRecipients
} from './recipients';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
describe('recipients pure helpers', () => {
	it('isDirectoryAvailable defaults to true', () => {
		expect(isDirectoryAvailable()).toBe(true);
	});
	it('resolveLabel falls back to the id when uncached', () => {
		expect(resolveLabel('group', 'g1')).toBe('g1');
		expect(resolveLabel('user', 'u1')).toBe('u1');
	});
	it('resolveRecipient builds a recipient object', () => {
		expect(resolveRecipient('group', 'g1')).toMatchObject({ type: 'group', id: 'g1', label: 'g1' });
		expect(resolveRecipient('user', 'u1')).toMatchObject({ type: 'user', id: 'u1' });
	});
});
describe('searchRecipients', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		// system contacts + groups both return arrays from .json()
		f.mockResolvedValue({ ok: true, status: 200, json: async () => [] });
	});
	it('returns an array of recipients', async () => {
		const r = await searchRecipients('alice').catch(() => []);
		expect(Array.isArray(r)).toBe(true);
		const e = await searchRecipients('a@b.test').catch(() => []);
		expect(Array.isArray(e)).toBe(true);
	});
});

describe('resolving the caller', () => {
	// The directory omits the caller unless asked for them, and the id→label
	// index is built from it. A member list that includes you then rendered
	// your row as a raw UUID — your own name is the one it could never show.
	it('resolves the caller after ensureResolvers', async () => {
		vi.resetModules();
		const SELF = '11111111-1111-1111-1111-111111111111';
		const other = { id: '22222222-2222-2222-2222-222222222222', full_name: 'Alice' };
		const self = { id: SELF, full_name: 'Bob' };
		const { apiFetch: fetchMock } = await import('$lib/api/client');
		(fetchMock as unknown as ReturnType<typeof vi.fn>).mockImplementation(async (url: string) => ({
			ok: true,
			status: 200,
			json: async () => (url.includes('include_self') ? [other, self] : [other])
		}));
		const mod = await import('./recipients');
		await mod.ensureResolvers();
		expect(mod.resolveRecipient('user', SELF).label).toBe('Bob');
		expect(mod.resolveLabel('user', other.id)).toBe('Alice');
	});
});

// ── Where the matching happens ────────────────────────────────────────
//
// THE bug (Ed, 2026-11-14): a lookup by username found nobody, and anyone
// outside the first hundred users was unfindable by ANY field. This module
// used to fetch the whole system address book — which the endpoint caps at
// `limit=100` by default — cache it, and filter locally on the display name
// and primary e-mail. So: username never matched, and the directory was
// effectively truncated at 100 at the 10k-user target.
//
// The fix hands the query to the server's `?search=`, which spans username,
// e-mail, both name parts and the two joined (`USER_SEARCH_PREDICATE`). These
// pins hold that line: a regression that reinstates local filtering shows up
// as a missing `search` parameter, not as a subtly wrong result set nobody
// notices until a directory is big. The server half is pinned in
// `tests/api/contacts.hurl` step 14b.
describe('server-side recipient matching', () => {
	/** A system-book contact, as the server projects a user into one. */
	function contact(over: Record<string, unknown> = {}) {
		return {
			id: 'u1',
			first_name: 'Ada',
			last_name: 'Lovelace',
			full_name: 'Ada Lovelace',
			nickname: 'ada',
			email: [{ email: 'ada@example.test', is_primary: true }],
			...over
		};
	}

	/** Answer the contacts URL with `users` and the groups URL with `groups`. */
	function serve(users: unknown[], groups: unknown[] = []) {
		f.mockImplementation((url: string) =>
			Promise.resolve({
				ok: true,
				status: 200,
				json: async () => (url.includes('/api/groups/search') ? groups : users)
			})
		);
	}

	/** The contacts request of the last call batch. */
	function contactsUrl(): string | undefined {
		return f.mock.calls.map((c) => String(c[0])).find((u) => u.includes('/contacts'));
	}

	beforeEach(() => vi.clearAllMocks());

	it('asks the server to do the matching, with the query and a bounded limit', async () => {
		serve([contact()]);

		await searchRecipients('ada');

		expect(contactsUrl()).toBeDefined();
		expect(contactsUrl()).toContain('search=ada');
		// Bounded, so a three-letter query cannot drag the directory down the
		// wire on every keystroke.
		expect(contactsUrl()).toContain('limit=8');
	});

	// `=true`, not `=1`: Axum's `Query` extractor deserialises bools from
	// "true"/"false" only, so `=1` would 400 before the handler runs.
	it('passes include_self as the literal true the server parses', async () => {
		serve([contact()]);

		await searchRecipients('ada', { includeSelf: true });

		expect(contactsUrl()).toContain('include_self=true');
	});

	// …and omits it by default, which is what keeps "you can't share with
	// yourself" out of the share picker.
	it('omits include_self by default and drops the caller from the results', async () => {
		serve([contact(), contact({ id: 'me', full_name: 'Me Myself' })]);

		const out = await searchRecipients('a');

		expect(contactsUrl()).not.toContain('include_self');
		expect(out.map((r) => r.id)).toEqual(['u1']);
	});

	// A username-only match now arrives from the server and must survive the
	// mapping with a usable label — the row still shows the person's NAME,
	// which is what a human recognises, with the e-mail as the disambiguator.
	it('maps a server match into a recipient with name and email', async () => {
		serve([contact()]);

		const out = await searchRecipients('ada');

		expect(out).toEqual([
			{ type: 'user', id: 'u1', label: 'Ada Lovelace', sublabel: 'ada@example.test' }
		]);
	});

	// A user with no name at all falls back to the e-mail rather than
	// rendering a bare UUID — an empty label is indistinguishable from a
	// broken row in the picker.
	it('falls back to the email when a contact carries no name', async () => {
		serve([contact({ first_name: undefined, last_name: undefined, full_name: undefined })]);

		const out = await searchRecipients('ada');

		expect(out[0].label).toBe('ada@example.test');
	});

	// The synthetic invite row, for an address that is not a user yet. It must
	// NOT appear when the address already belongs to someone the search found,
	// or the picker offers to invite an existing colleague.
	it('offers an email invite only when no user holds that address', async () => {
		serve([]);
		const invite = await searchRecipients('new@example.test');
		expect(invite).toEqual([{ type: 'email', id: 'new@example.test', label: 'new@example.test' }]);

		serve([contact({ email: [{ email: 'new@example.test', is_primary: true }] })]);
		const existing = await searchRecipients('new@example.test');
		expect(existing.some((r) => r.type === 'email')).toBe(false);
	});

	// An unreachable directory degrades the suggestions instead of emptying
	// the dialog: groups are fetched in parallel and still answer.
	it('still returns groups when the directory is unavailable', async () => {
		f.mockImplementation((url: string) =>
			url.includes('/api/groups/search')
				? Promise.resolve({ ok: true, status: 200, json: async () => [{ id: 'g1', name: 'Team' }] })
				: Promise.resolve({ ok: false, status: 503, json: async () => [] })
		);

		const out = await searchRecipients('te');

		expect(out).toEqual([{ type: 'group', id: 'g1', label: 'Team' }]);
	});

	// No query, no requests. The input fires on every keystroke including the
	// one that empties it, and an empty `search=` would ask the server for the
	// whole directory.
	it('makes no request for an empty query', async () => {
		serve([contact()]);

		expect(await searchRecipients('   ')).toEqual([]);
		expect(apiFetch).not.toHaveBeenCalled();
	});
});
