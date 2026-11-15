import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { ui } = vi.hoisted(() => ({ ui: { notify: vi.fn() } }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
// `errorMessage` as well as `errorToast`: `useAsyncAction` renders the inline
// error through it, so a mock without it throws "No export is defined" on the
// FAILURE path only — invisible until a test exercises a refused grant.
vi.mock('$lib/utils/errors', () => ({
	errorToast: vi.fn(),
	errorMessage: (e: unknown) => (e instanceof Error ? e.message : String(e))
}));
vi.mock('$lib/api/endpoints/shares', () => ({
	copyShareLink: vi.fn(),
	createShare: vi.fn(),
	deleteShare: vi.fn(),
	listSharesForItem: vi.fn(),
	updateShare: vi.fn()
}));
vi.mock('$lib/api/endpoints/grants', () => ({
	createGrant: vi.fn(),
	expiryToIso: (v: string | null) => v,
	displayRole: (r: string) => r,
	fetchGrantsForResource: vi.fn(),
	notifyGrantRecipient: vi.fn(),
	revokeGrant: vi.fn(),
	todayIso: () => '2026-07-22',
	updateGrantRole: vi.fn()
}));
vi.mock('$lib/api/endpoints/recipients', () => ({
	ensureResolvers: vi.fn(),
	isDirectoryAvailable: () => true,
	resolveRecipient: (_t: string, id: string) => ({ id, label: id }),
	searchRecipients: vi.fn(async () => [])
}));
// Inherited access — the ancestor walk and the file→parent lookup.
vi.mock('$lib/api/endpoints/folders', () => ({ getFolderAncestorsWithGrants: vi.fn() }));
vi.mock('$lib/api/endpoints/files', () => ({ getFile: vi.fn() }));
// The link form reads the owning drive's EFFECTIVE policies from here —
// `GET /api/drives` already resolves defaults, so no extra endpoint.
const { drivesStore } = vi.hoisted(() => ({
	drivesStore: { drives: [] as { id: string; policies: Record<string, unknown> }[], load: vi.fn() }
}));
vi.mock('$lib/stores/drives.svelte', () => ({ drives: drivesStore }));

import { createShare, listSharesForItem } from '$lib/api/endpoints/shares';
import { createGrant, fetchGrantsForResource } from '$lib/api/endpoints/grants';
import { getFolderAncestorsWithGrants } from '$lib/api/endpoints/folders';
import { searchRecipients } from '$lib/api/endpoints/recipients';
import ShareDialog from './ShareDialog.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;
const item = { id: 'f1', name: 'doc.txt', kind: 'file' as const };

/** A folder whose access all arrives from the ancestor above it. */
const folderItem = { id: 'fold1', name: 'Sub', kind: 'folder' as const };

/**
 * Ancestor walk for `fold1`: parent `p1` carries one group grant and one
 * public link, `fold1` itself carries nothing.
 *
 * The group subject is deliberate — a `user` subject renders
 * `UserVignette`, which resolves profiles of its own accord. Groups take
 * the plain icon+label branch, so these tests exercise the inheritance
 * logic without standing up an unrelated component.
 */
const chainWithInherited = {
	ancestors: [
		{ id: 'p1', name: 'Projects', parent_id: null, drive_id: 'd1' },
		{ id: 'fold1', name: 'Sub', parent_id: 'p1', drive_id: 'd1' }
	],
	access_source: { kind: 'drive', drive: { id: 'd1', name: 'Personal', kind: 'personal' } },
	effective_grants: [
		{
			id: 'g-inherited',
			subject: { type: 'group', id: 'team-a' },
			role: 'editor',
			resource: { type: 'folder', id: 'p1' }
		}
	],
	effective_links: [
		{
			grant_id: 'lg1',
			share_id: 's9',
			name: 'Team link',
			has_password: true,
			resource: { type: 'folder', id: 'p1' }
		}
	]
};

/**
 * Same walk, but the access sits on the DRIVE rather than a folder.
 *
 * Drive-anchored grants take a different branch: they cannot link into
 * `/files` (a drive has no browsable folder URL of its own), so the chip
 * points at the drive's config page, where drive membership is actually
 * managed.
 */
const chainWithDriveInherited = {
	ancestors: [{ id: 'fold1', name: 'Sub', parent_id: null, drive_id: 'd1' }],
	access_source: { kind: 'drive', drive: { id: 'd1', name: 'Marketing', kind: 'shared' } },
	effective_grants: [
		{
			id: 'g-drive',
			subject: { type: 'group', id: 'team-a' },
			role: 'editor',
			resource: { type: 'drive', id: 'd1' }
		}
	],
	effective_links: [
		{
			grant_id: 'lg-drive',
			share_id: 's10',
			name: 'Drive-wide link',
			has_password: false,
			resource: { type: 'drive', id: 'd1' }
		}
	]
};

beforeEach(() => {
	vi.clearAllMocks();
	m(fetchGrantsForResource).mockResolvedValue([]);
	m(listSharesForItem).mockResolvedValue([]);
	// Default: nothing inherited. Tests that care override it.
	m(getFolderAncestorsWithGrants).mockResolvedValue({
		ancestors: [],
		access_source: { kind: 'drive' },
		effective_grants: [],
		effective_links: []
	});
	drivesStore.drives = [];
});

// ── Drive policy applied to the link form ──────────────────────────────
//
// The server REFUSES an over-cap or password-less link rather than
// clamping it, on the grounds that the dialog shows the limit first.
// These pin that half of the bargain: without them the user meets the
// policy as a 405 after committing, which is the worst of both designs.

/** Ancestor chain that resolves the item to drive `d1`. */
const chainOnDrive = {
	ancestors: [{ id: 'fold1', name: 'Sub', parent_id: null, drive_id: 'd1' }],
	access_source: { kind: 'drive', drive: { id: 'd1', name: 'Team', kind: 'shared' } },
	effective_grants: [],
	effective_links: []
};

async function openLinkTabOnDrive(policies: Record<string, unknown>) {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainOnDrive);
	drivesStore.drives = [{ id: 'd1', policies }];
	render(ShareDialog, { props: { open: true, item: folderItem } });
	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));
}

it('blocks link creation when the drive forbids public links', async () => {
	await openLinkTabOnDrive({ forbid_public_links: true });

	const msg = await screen.findByTestId('share-dialog-policy-block');
	expect(msg.textContent).toContain('does not allow public links');
	expect((screen.getByTestId('share-dialog-create-btn') as HTMLButtonElement).disabled).toBe(true);
});

it('requires a password before allowing creation when the policy says so', async () => {
	await openLinkTabOnDrive({ require_public_link_password: true });

	const btn = () => screen.getByTestId('share-dialog-create-btn') as HTMLButtonElement;
	await waitFor(() => expect(btn().disabled).toBe(true));
	expect(screen.getByTestId('share-dialog-policy-block').textContent).toContain(
		'requires a password'
	);

	await fireEvent.input(screen.getByTestId('share-dialog-link-password-input'), {
		target: { value: 'hunter2' }
	});

	// Typing a password clears the block — the limit was stated up front
	// rather than surfacing as a refusal after submit.
	await waitFor(() => expect(btn().disabled).toBe(false));
	expect(screen.queryByTestId('share-dialog-policy-block')).toBeNull();
});

it('caps the expiry picker and refuses a date beyond it', async () => {
	await openLinkTabOnDrive({ max_public_link_days: 7 });

	const input = (await screen.findByTestId('share-dialog-link-expires-input')) as HTMLInputElement;
	// The cap is on the native picker, so it is visible in the calendar
	// rather than discovered by being rejected.
	expect(input.max).toMatch(/^\d{4}-\d{2}-\d{2}$/);

	const btn = () => screen.getByTestId('share-dialog-create-btn') as HTMLButtonElement;
	// No expiry at all is over the cap: a link that never expires is the
	// laxest value there is, not an exemption. Same rule as the server.
	await waitFor(() => expect(btn().disabled).toBe(true));

	await fireEvent.change(input, { target: { value: '2099-01-01' } });
	await waitFor(() =>
		expect(screen.getByTestId('share-dialog-policy-block').textContent).toContain(
			'caps public links'
		)
	);
	expect(btn().disabled).toBe(true);
});

it('leaves the form alone when the drive has no link policy', async () => {
	await openLinkTabOnDrive({});

	// The negative control: without it, every assertion above would pass
	// on a dialog that simply always blocks.
	await waitFor(() =>
		expect((screen.getByTestId('share-dialog-create-btn') as HTMLButtonElement).disabled).toBe(
			false
		)
	);
	expect(screen.queryByTestId('share-dialog-policy-block')).toBeNull();
});

it('loads grants and shares when opened', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog');
	await waitFor(() => expect(fetchGrantsForResource).toHaveBeenCalledWith('file', 'f1'));
	await waitFor(() => expect(listSharesForItem).toHaveBeenCalledWith('f1', 'file'));
});

it('switches to the link tab and creates a public link', async () => {
	m(createShare).mockResolvedValue({ id: 's1', token: 'abc', has_password: false });
	render(ShareDialog, { props: { open: true, item } });
	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));
	await fireEvent.input(screen.getByTestId('share-dialog-link-name-input'), {
		target: { value: 'My link' }
	});
	await fireEvent.click(screen.getByTestId('share-dialog-create-btn'));
	await waitFor(() =>
		expect(createShare).toHaveBeenCalledWith(
			expect.objectContaining({ itemId: 'f1', itemName: 'My link', itemType: 'file' })
		)
	);
});

it('does not load when closed', () => {
	render(ShareDialog, { props: { open: false, item } });
	expect(fetchGrantsForResource).not.toHaveBeenCalled();
});

// ── Reveal-on-pick add form ───────────────────────────────────────────
//
// The add form used to show four controls at all times, three of which did
// nothing until a recipient was chosen: the role was a choice about nobody
// and [Add] was greyed. Worse, they crowded the ONE field that holds free
// text down to a few characters at dialog width. Role, expiry and [Add] now
// appear on selection.
//
// Pinned because this is behaviour, not styling: the controls' presence is
// conditional, and a refactor that reinstates them unconditionally brings
// the cramped field back with it.

/**
 * Search, then queue one result — the commit controls appear with the first
 * pick. Takes the subject so a test can queue several.
 */
async function pickRecipient(id = 'u1', label = 'Alice', type = 'user') {
	m(searchRecipients).mockResolvedValue([{ type, id, label }]);
	await fireEvent.input(screen.getByTestId('share-dialog-search-input'), {
		target: { value: label.slice(0, 3) }
	});
	await fireEvent.click(
		await screen.findByTestId(`share-dialog-result-${type}-${id}`, undefined, { timeout: 2_000 })
	);
}

it('shows only the name field until a recipient is picked', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');

	// Nothing to commit, so nothing to commit WITH.
	expect(screen.queryByTestId('share-dialog-add-btn')).toBeNull();
	expect(screen.queryByTestId('share-dialog-role-viewer-btn')).toBeNull();

	await pickRecipient();

	expect(await screen.findByTestId('share-dialog-add-btn')).toBeTruthy();
	expect(screen.getByTestId('share-dialog-role-viewer-btn')).toBeTruthy();
});

// Emptying the queue must put the form back, or the dialog keeps a commit
// bar for recipients that are no longer there.
it('folds the commit controls away again when the queue is emptied', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');
	await pickRecipient();
	await screen.findByTestId('share-dialog-add-btn');

	await fireEvent.click(screen.getByTestId('share-dialog-unpick-user-u1'));

	await waitFor(() => expect(screen.queryByTestId('share-dialog-add-btn')).toBeNull());
	expect(screen.queryByTestId('share-dialog-role-viewer-btn')).toBeNull();
});

// ── Queueing several recipients ───────────────────────────────────────
//
// Picking a name used to REPLACE the search box with the chosen chip, so the
// second name could not be typed until the first had been granted: a round
// trip, a role press and an [Add] per person. The field now stays and picks
// accumulate, which is the whole point of the change — pinned at the
// behaviour, not the layout.
it('queues several recipients and keeps the field for the next one', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');

	await pickRecipient('u1', 'Alice');
	await pickRecipient('g1', 'Team', 'group');

	// Both still queued, and the field survived both picks.
	expect(screen.getByTestId('share-dialog-pick-user-u1')).toBeTruthy();
	expect(screen.getByTestId('share-dialog-pick-group-g1')).toBeTruthy();
	expect(screen.getByTestId('share-dialog-search-input')).toBeTruthy();
	// The count is in the label, or [Add] over five chips reads as "add the
	// one I just typed".
	expect(screen.getByTestId('share-dialog-add-btn').textContent).toContain('2');
});

// Focus, because adding people is a typing job: the click that queues a name
// lands on a result row that is then removed, so without this the caret is
// nowhere and the next name needs a mouse trip back to the field.
it('hands focus back to the field after a pick', async () => {
	render(ShareDialog, { props: { open: true, item } });
	const input = await screen.findByTestId('share-dialog-search-input');

	await pickRecipient();

	expect(document.activeElement).toBe(input);
});

// One [Add] grants the whole queue, at the one role chosen above it.
it('grants every queued recipient in one press', async () => {
	m(createGrant).mockResolvedValue({ notification: { outcomes: [] } });
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');
	await pickRecipient('u1', 'Alice');
	await pickRecipient('g1', 'Team', 'group');

	await fireEvent.click(screen.getByTestId('share-dialog-add-btn'));

	await waitFor(() => expect(createGrant).toHaveBeenCalledTimes(2));
	expect(m(createGrant).mock.calls[0][0]).toEqual({ type: 'user', id: 'u1' });
	expect(m(createGrant).mock.calls[1][0]).toEqual({ type: 'group', id: 'g1' });
	// Granted ones leave the queue, so a second press cannot re-grant them.
	await waitFor(() => expect(screen.queryByTestId('share-dialog-pick-user-u1')).toBeNull());
});

// A partial success must not read as a success. The refused recipient STAYS
// queued (one press retries, and nobody who succeeded is granted twice) and
// the server's own reason renders inline — this is the case a `Promise.all`
// plus a single catch would turn into "something went wrong, state unknown".
it('keeps only the refused recipient queued when one grant fails', async () => {
	m(createGrant)
		.mockRejectedValueOnce(new Error('policy forbids this'))
		.mockResolvedValueOnce({ notification: { outcomes: [] } });
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');
	await pickRecipient('u1', 'Alice');
	await pickRecipient('g1', 'Team', 'group');

	await fireEvent.click(screen.getByTestId('share-dialog-add-btn'));

	// The inline error needs its own wait: the queue is trimmed BEFORE the
	// refusal is rethrown (the grants reload sits between them), so a bare
	// read here passes the chip assertions and races the alert.
	await waitFor(() =>
		expect(screen.getByRole('alert').textContent).toContain('policy forbids this')
	);
	// The one that failed is still there; the one that worked is gone.
	expect(screen.getByTestId('share-dialog-pick-user-u1')).toBeTruthy();
	expect(screen.queryByTestId('share-dialog-pick-group-g1')).toBeNull();
});

// Already queued means already offered: the search must not list a name that
// is sitting in a chip right above the field, or a second press looks like it
// did nothing. Same filter as the one that hides direct members.
it('stops offering a recipient that is already queued', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');
	await pickRecipient();

	m(searchRecipients).mockResolvedValue([{ type: 'user', id: 'u1', label: 'Alice' }]);
	await fireEvent.input(screen.getByTestId('share-dialog-search-input'), {
		target: { value: 'ali' }
	});

	await waitFor(() => expect(searchRecipients).toHaveBeenCalledTimes(2));
	expect(screen.queryByTestId('share-dialog-result-user-u1')).toBeNull();
});

// The role picker replaced a `<select>` that no test drove. This is the pin
// that says the three segments are wired to the grant, and that the one you
// pressed is the role that gets written — not the default the form opened on.
it('grants the role chosen in the segmented picker', async () => {
	m(createGrant).mockResolvedValue({ notification: { outcomes: [] } });
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');
	await pickRecipient();

	await fireEvent.click(await screen.findByTestId('share-dialog-role-editor-btn'));
	await fireEvent.click(screen.getByTestId('share-dialog-add-btn'));

	await waitFor(() =>
		expect(createGrant).toHaveBeenCalledWith(
			{ type: 'user', id: 'u1' },
			{ type: 'file', id: 'f1' },
			'editor',
			null
		)
	);
});

// The picker is a radiogroup, so exactly one segment reports itself checked —
// the state a screen reader announces and the only thing distinguishing the
// three buttons to a non-visual caller.
it('marks one role segment as checked at a time', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-search-input');
	await pickRecipient();

	const viewer = await screen.findByTestId('share-dialog-role-viewer-btn');
	const editor = screen.getByTestId('share-dialog-role-editor-btn');
	expect(viewer.getAttribute('aria-checked')).toBe('true');

	await fireEvent.click(editor);

	await waitFor(() => expect(editor.getAttribute('aria-checked')).toBe('true'));
	expect(viewer.getAttribute('aria-checked')).toBe('false');
});

// ── Searching for someone who already has inherited access ────────────
//
// THE bug (Ed, 2026-11-14): a user who already appears in the dialog could
// not be found by the search — and the common reason they appear is
// INHERITED access from a parent folder. So there was no way to give them a
// grant on THIS resource, which is exactly what you want before the file
// moves out from under that parent, or when they need a different role here.
//
// The filter now drops only DIRECT members. An inherited row is a statement
// about an ancestor; it says nothing about this resource's own grants, which
// is also how the server reads it (`is_shared` counts `role_grants` on this
// row alone).
it('offers a recipient whose only access is inherited, and says where from', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	// `team-a` is the group that holds the INHERITED grant on `p1`.
	m(searchRecipients).mockResolvedValue([{ type: 'group', id: 'team-a', label: 'Team A' }]);
	render(ShareDialog, { props: { open: true, item: folderItem } });
	await screen.findByTestId('share-dialog-member-source-group-team-a');

	await fireEvent.input(screen.getByTestId('share-dialog-search-input'), {
		target: { value: 'team' }
	});

	// Offered at all — this is the assertion that was false before.
	const hit = await screen.findByTestId('share-dialog-result-group-team-a', undefined, {
		timeout: 2_000
	});
	// …and labelled, so it does not read as a duplicate of the row above.
	// `Projects` is the ancestor the grant sits on in `chainWithInherited`.
	expect(hit.textContent).toContain('Projects');
});

// The other half: a DIRECT member must still be filtered out. Offering them
// again would invite a duplicate grant the server refuses, and the row to
// change their role is already on screen.
it('still hides a recipient who holds a direct grant', async () => {
	m(fetchGrantsForResource).mockResolvedValue([
		{ id: 'g1', subject: { type: 'group', id: 'team-a' }, role: 'editor' }
	]);
	m(searchRecipients).mockResolvedValue([{ type: 'group', id: 'team-a', label: 'Team A' }]);
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog-member-role-group-team-a');

	await fireEvent.input(screen.getByTestId('share-dialog-search-input'), {
		target: { value: 'team' }
	});

	await waitFor(() => expect(searchRecipients).toHaveBeenCalled());
	expect(screen.queryByTestId('share-dialog-result-group-team-a')).toBeNull();
});

// ── The shared chip on every list ──────────────────────────────────────
//
// THE bug: sharing a resource and unsharing it both left the row's chip
// unchanged until the page was refreshed. The only signal was an
// `onshared` prop that fired on CREATION and was wired by `/files` alone,
// so `/favorites` (where Ed hit it), `/recent`, `/search`,
// `/shared-with-me` and `/changes` painted a stale chip in both
// directions. Every mutation now broadcasts the resource's new shared
// state and each listing patches its own row.
//
// These pin the EMITTER, which is where the bug was. The listener half is
// pinned in `listActions.test.ts` (`applyRowFlags`) and on the pages.
//
// The asserted value is the server's own predicate — `is_shared` is
// `EXISTS (SELECT 1 FROM storage.role_grants WHERE resource_id = <this>)` —
// so these also pin the two readings that would contradict the next
// reload: an inherited grant must NOT count, and a public link MUST.

/** Collect every `is_shared` announcement made during one act. */
function watchSharedFlags(): { seen: (boolean | undefined)[]; stop: () => void } {
	const seen: (boolean | undefined)[] = [];
	const handler = (e: Event) => {
		const d = (e as CustomEvent<{ id: string; is_shared?: boolean }>).detail;
		if (d?.id === 'f1' || d?.id === 'fold1') seen.push(d.is_shared);
	};
	window.addEventListener('oxicloud:row-flags-changed', handler);
	return { seen, stop: () => window.removeEventListener('oxicloud:row-flags-changed', handler) };
}

it('announces the resource as shared once a public link exists', async () => {
	m(createShare).mockResolvedValue({ id: 's1', token: 'abc', has_password: false });
	// The reload after the write is what the announcement reads, so the
	// second answer is the one that matters.
	m(listSharesForItem)
		.mockResolvedValueOnce([])
		.mockResolvedValueOnce([{ id: 's1' }]);
	const w = watchSharedFlags();
	render(ShareDialog, { props: { open: true, item } });
	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));

	await fireEvent.click(screen.getByTestId('share-dialog-create-btn'));

	await waitFor(() => expect(w.seen).toContain(true));
	w.stop();
});

it('announces the resource as unshared when its last link goes', async () => {
	m(listSharesForItem).mockResolvedValue([{ id: 's1', token: 'abc', has_password: false }]);
	const w = watchSharedFlags();
	render(ShareDialog, { props: { open: true, item } });
	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));

	await fireEvent.click(await screen.findByTestId('share-dialog-link-delete-btn-s1'));

	await waitFor(() => expect(w.seen).toContain(false));
	w.stop();
});

// Removing a DIRECT member while a link survives must keep the chip lit —
// the flag is "is anything granted on this row", not "did I just revoke
// something". Counting the revocation instead of re-reading the lists
// would clear a chip the server still paints.
it('keeps the resource shared when a member goes but a link remains', async () => {
	m(fetchGrantsForResource)
		.mockResolvedValueOnce([{ id: 'g1', subject: { type: 'group', id: 'team-a' }, role: 'editor' }])
		.mockResolvedValueOnce([]);
	m(listSharesForItem).mockResolvedValue([{ id: 's1', token: 'abc', has_password: false }]);
	const w = watchSharedFlags();
	render(ShareDialog, { props: { open: true, item } });

	await fireEvent.click(await screen.findByTestId('share-dialog-member-remove-group-team-a'));

	await waitFor(() => expect(w.seen.length).toBeGreaterThan(0));
	expect(w.seen).not.toContain(false);
	w.stop();
});

// An INHERITED grant is on an ancestor, so the server computes `is_shared`
// as false for this row. Announcing true here would light a chip that the
// next listing fetch puts out — a flicker that reads as a lost share.
it('calls a resource unshared once only inherited access is left', async () => {
	// `fold1` has one DIRECT member plus the inherited group from `p1`.
	// Revoking the direct one leaves an inherited row still on screen —
	// and the chip must go out anyway, because the surviving grant sits on
	// the ancestor and the server's `is_shared` is per row.
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	m(fetchGrantsForResource)
		.mockResolvedValueOnce([
			{ id: 'g-direct', subject: { type: 'group', id: 'direct-team' }, role: 'editor' }
		])
		.mockResolvedValueOnce([]);
	m(listSharesForItem).mockResolvedValue([]);
	const w = watchSharedFlags();
	render(ShareDialog, { props: { open: true, item: folderItem } });

	await fireEvent.click(await screen.findByTestId('share-dialog-member-remove-group-direct-team'));

	await waitFor(() => expect(w.seen).toContain(false));
	// The inherited row is still listed — this is the state that must not
	// be read as "shared".
	expect(screen.getByTestId('share-dialog-member-source-group-team-a')).toBeTruthy();
	w.stop();
});

// ── Inherited access ───────────────────────────────────────────────────
//
// Access mostly arrives by cascade, so a dialog that lists only direct
// grants answers "who can reach this?" wrongly — it can show an empty
// list for a folder half the company can open.

it('lists a grant inherited from an ancestor, read-only, with its source', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	// Present, and labelled with the folder the grant actually sits on.
	// Plain `textContent` rather than jest-dom's `toHaveTextContent`: the
	// matchers load at runtime via vitest-setup, but no other test in this
	// codebase uses them and their types are not wired into svelte-check,
	// so asserting with them fails `npm run check` while passing vitest.
	const source = await screen.findByTestId('share-dialog-member-source-group-team-a');
	expect(source.textContent).toContain('Projects');

	// …and NOT editable here. The grant belongs to another folder, so a
	// role change or revoke from this dialog would silently mutate a
	// different folder's sharing. This assertion is the guard against
	// that: it fails the moment the controls leak into an inherited row.
	expect(screen.queryByTestId('share-dialog-member-role-group-team-a')).toBeNull();
	expect(screen.queryByTestId('share-dialog-member-remove-group-team-a')).toBeNull();
});

it('re-points the dialog when an inherited source is clicked', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	const onretarget = vi.fn();
	render(ShareDialog, { props: { open: true, item: folderItem, onretarget } });

	await fireEvent.click(await screen.findByTestId('share-dialog-member-source-group-team-a'));

	// Without this the chip would navigate /files → /files, which is the
	// same route: nothing remounts, and the dialog sits there still
	// showing the folder the user just left — grant still greyed.
	expect(onretarget).toHaveBeenCalledWith({ id: 'p1', name: 'Projects', kind: 'folder' });
});

it('counts inherited access in the tab badges', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	// Both are inherited and neither is direct, so a badge counting only
	// direct grants would read (0) beside a folder two parties can reach.
	await waitFor(() =>
		expect(screen.getByTestId('share-dialog-people-tab').textContent).toContain('1')
	);
	expect(screen.getByTestId('share-dialog-link-tab').textContent).toContain('1');
});

it('lists a public link inherited from an ancestor', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));

	// The link's own name, not just "a parent is shared" — that name is
	// why the backend joins `storage.shares`; the token grant alone
	// carries nothing displayable.
	expect(await screen.findByText('Team link')).not.toBeNull();
	expect(screen.getByTestId('share-dialog-inherited-link-source-lg1').textContent).toContain(
		'Projects'
	);
});

it('points a drive-inherited grant at the drive config page', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithDriveInherited);
	const onretarget = vi.fn();
	render(ShareDialog, { props: { open: true, item: folderItem, onretarget } });

	const source = await screen.findByTestId('share-dialog-member-source-group-team-a');
	expect(source.getAttribute('href')).toContain('/config/drive/d1');
	expect(source.textContent).toContain('Marketing');

	// Navigation only. Re-pointing THIS dialog at a drive would offer
	// folder controls for a resource that has none — drive grants use a
	// narrower role ladder and support no public links, which is why the
	// config page runs the dialog with allowLinks=false.
	await fireEvent.click(source);
	expect(onretarget).not.toHaveBeenCalled();
});

it('points a drive-inherited public link at the drive config page', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithDriveInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));

	const source = await screen.findByTestId('share-dialog-inherited-link-source-lg-drive');
	expect(source.getAttribute('href')).toContain('/config/drive/d1');
	// Before this the row was flat text — visible, but a dead end.
	expect(source.tagName).toBe('A');
});

it('walks from the parent folder when sharing a file', async () => {
	const { getFile } = await import('$lib/api/endpoints/files');
	m(getFile).mockResolvedValue({ id: 'f1', folder_id: 'parent-of-f1' });
	render(ShareDialog, { props: { open: true, item } });

	// A file has no ancestors of its own — the climb starts one level up.
	// Its own grants come from `fetchGrantsForResource`, so everything the
	// walk returns is inherited and none of it is filtered out.
	await waitFor(() => expect(getFolderAncestorsWithGrants).toHaveBeenCalledWith('parent-of-f1'));
});
