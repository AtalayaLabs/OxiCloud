import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Rides the generated SDK directly (Phase 4), so the mock surface is the
// generated module rather than a hand-written endpoint wrapper.
vi.mock('$lib/api/generated/openapi/sdk.gen', () => ({
	listRecentChanges: vi.fn()
}));
// `typeLabelOf` — the name the page actually imports. Mocking the module
// replaces it wholesale, so an export the page calls but the mock omits is
// `undefined` at runtime and throws the moment that axis is selected;
// `npm run check` cannot see it, because it type-checks against the real
// module. Identity, so bucket keys assert as themselves.
vi.mock('$lib/utils/resourceBuckets', () => ({
	dateBucket: () => 'Today',
	sizeBucket: () => 'Small',
	typeLabelOf: (k: string) => k,
	typeLabel: () => 'File'
}));
// The page and `<UserAvatar>` share this resolver, so one mock covers both.
// It returns a readable name AND an email: the lane header asserts on the
// name, so a regression that stopped resolving modifiers shows up as a
// changed label rather than passing silently, and the email is what the
// hover tooltip must carry.
vi.mock('$lib/api/endpoints/users', () => ({
	resolveUser: vi.fn(async (id: string) => ({
		id,
		name: id === 'bob' ? 'Bob' : 'Carol',
		email: `${id}@example.test`,
		image: null,
		isExternal: false,
		isOnline: false
	}))
}));

import { listRecentChanges } from '$lib/api/generated/openapi/sdk.gen';
import { resolveUser } from '$lib/api/endpoints/users';
import ChangesPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function file(id: string, name: string) {
	return {
		// A real `ResourceCategory` member — the fixture these were copied
		// from used "Document", which is not one, and the type axis buckets
		// on this value.
		category: 'Text',
		created_at: 0,
		id,
		mime_type: 'text/plain',
		modified_at: 0,
		name,
		created_by: 'bob',
		updated_by: 'bob',
		folder_id: 'root',
		path: `/${name}`,
		size: 4,
		sort_date: 0,
		etag: 'e',
		content_hash: 'h'
	};
}

function change(fileId: string, name: string, modifier: string, changedAt: string, laneTotal = 1) {
	return {
		changed_at: changedAt,
		lane_total: laneTotal,
		modifier_id: modifier,
		resource: file(fileId, name)
	};
}

beforeEach(() => vi.clearAllMocks());

// The regression the whole feature exists for, at the FE layer: ONE file,
// changed by two people, belongs to BOTH lanes — and Bob's lane shows the
// date Bob changed it, not the file's latest change.
//
// It is also the duplicate-key pin. `VirtualList` keys rows by `item.id`, so
// the two rows survive only because each lane is its own `#each`. A bucketing
// change that read the lane out of `ResourceList`'s id-keyed `contextMap`
// would drop both copies into one bucket and throw `each_key_duplicate` —
// the same crash the music queue hit when one track was enqueued twice.
it('shows one file in both lanes when two people changed it', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [
			change('f1', 'notes.txt', 'carol', '2024-01-02T00:00:00Z'),
			change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')
		],
		next_cursor: null
	});
	render(ChangesPage);

	await waitFor(() => expect(listRecentChanges).toHaveBeenCalled());
	await waitFor(() => expect(screen.getByTestId('changes-lane-carol')).toBeTruthy());
	expect(screen.getByTestId('changes-lane-bob')).toBeTruthy();
	// Two rows for the same file — one per lane.
	expect(screen.getAllByTestId('notes.txt')).toHaveLength(2);
});

// The lane totals come from the server, counted AFTER the access filter, and
// render in the header beside the person. A lane that lost its total would
// still show the name, so the count is pinned separately.
it('renders the server lane total in the lane header', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z', 14)],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getByTestId('changes-lane-bob')).toBeTruthy());
	await waitFor(() => expect(screen.getByText(/14 file/)).toBeTruthy());
});

// The lane header identifies a person three ways, and each one can regress on
// its own: the avatar (photo or coloured initials), the resolved NAME rather
// than the raw uuid the DTO carries, and the full email on hover — the
// disambiguator, since two colleagues can share a display name and "who
// exactly" is the question a provenance view answers.
//
// The email is pinned on the wrapper, not the avatar: `<UserAvatar>` defaults
// its own `title` to the display name, so leaving that in place would mean the
// inner tooltip wins and the email never appears.
it('shows avatar, resolved name and the email on hover in the lane header', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: null
	});
	render(ChangesPage);

	const lane = await screen.findByTestId('changes-lane-bob');
	await waitFor(() => expect(lane.textContent).toContain('Bob'));
	await waitFor(() => expect(lane.getAttribute('title')).toBe('bob@example.test'));
	expect(lane.querySelector('.ua')).toBeTruthy();
});

// A lane can straddle a page boundary: the cursor ends mid-lane and the next
// page continues it. Appending must MERGE into the open lane rather than draw
// a second header for the same person — which is what a per-page grouping
// pass would do, and it looks like two Bobs to the reader.
it('merges an appended page into the open lane', async () => {
	m(listRecentChanges)
		.mockResolvedValueOnce({
			items: [change('f1', 'one.txt', 'bob', '2024-01-02T00:00:00Z', 2)],
			next_cursor: 'cursor-1'
		})
		.mockResolvedValueOnce({
			items: [change('f2', 'two.txt', 'bob', '2024-01-01T00:00:00Z', 2)],
			next_cursor: null
		});
	render(ChangesPage);
	await waitFor(() => expect(screen.getByTestId('one.txt')).toBeTruthy());

	await fireEvent.click(await screen.findByTestId('resource-list-load-more-btn'));
	await waitFor(() => expect(screen.getByTestId('two.txt')).toBeTruthy());
	expect(screen.getAllByTestId('changes-lane-bob')).toHaveLength(1);
});

// The date axis has no lanes, so two people who changed one file produce two
// rows with the same id in ONE bucket — a duplicate `#each` key and a hard
// render error. That axis therefore shows each file once, at its most recent
// change (the server orders touches newest-first, so it is the first row).
it('shows a file once on the date axis', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [
			change('f1', 'notes.txt', 'carol', '2024-01-02T00:00:00Z'),
			change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')
		],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getAllByTestId('notes.txt')).toHaveLength(2));

	await fireEvent.click(screen.getByTestId('display-mode-groupby-btn'));
	await fireEvent.click(await screen.findByTestId('display-mode-groupby-changedAt-item'));

	await waitFor(() => expect(screen.getAllByTestId('notes.txt')).toHaveLength(1));
	// And the axis actually changed on the wire — the page must not re-ask
	// for `modifier` rows and merely re-bucket them client-side.
	expect(listRecentChanges).toHaveBeenLastCalledWith(
		expect.objectContaining({ query: expect.objectContaining({ order_by: 'changed_at' }) })
	);

	// The bucket key here is a DATE ("Today"), not a person. One snippet
	// serves both axes, so an ungated avatar would fetch `/api/users/Today`
	// and then draw a coloured "T" circle as though a person called Today
	// existed. Both halves are pinned: no avatar in the header, and no
	// resolve attempted for the bucket name.
	const bucket = screen.getByTestId('changes-lane-Today');
	expect(bucket.querySelector('.ua')).toBeNull();
	expect(bucket.getAttribute('title')).toBeNull();
	expect(resolveUser).not.toHaveBeenCalledWith('Today');
});

// The type axis buckets on the server's `ResourceCategory`, client-side, and
// must NOT change what it asks the server for: the wire enum is two values
// wide, so this axis rides `changed_at` order and groups what the page has.
// Asserting the request too is what keeps a future "just add type to the
// enum" from passing this test while silently needing a spec regen.
//
// Same dedupe as every non-people axis: one bucket, so a file two people
// changed would otherwise be the same `#each` key twice.
it('groups by type without changing the server order', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [
			change('f1', 'notes.txt', 'carol', '2024-01-02T00:00:00Z'),
			change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')
		],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getAllByTestId('notes.txt')).toHaveLength(2));

	await fireEvent.click(screen.getByTestId('display-mode-groupby-btn'));
	await fireEvent.click(await screen.findByTestId('display-mode-groupby-type-item'));

	await waitFor(() => expect(screen.getByTestId('changes-lane-Text')).toBeTruthy());
	expect(screen.getAllByTestId('notes.txt')).toHaveLength(1);
	expect(listRecentChanges).toHaveBeenLastCalledWith(
		expect.objectContaining({ query: expect.objectContaining({ order_by: 'changed_at' }) })
	);
	// No identity chrome on a category bucket, for the `/api/users/Text`
	// reason the date axis pins.
	expect(screen.getByTestId('changes-lane-Text').querySelector('.ua')).toBeNull();
});

// The row menu is deliberately TWO entries. This is a provenance view, not a
// file manager: rename / move / share / delete act on the file and belong
// where it lives — and on the people axis the same file is listed in two
// lanes, so a destructive action on a row the user sees twice has no good
// answer. Pinning the absences, because "we trimmed the menu" is exactly the
// kind of intent a later copy-paste from `/recent` undoes.
it('offers only open-parent and download in the row menu', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getByTestId('notes.txt')).toBeTruthy());

	await fireEvent.click(screen.getByTestId('resource-list-more-f1-btn'));

	expect(await screen.findByText('Open parent folder')).toBeTruthy();
	expect(screen.getByText('Download')).toBeTruthy();
	for (const absent of ['Rename', 'Move', 'Delete', 'Share']) {
		expect(screen.queryByText(absent)).toBeNull();
	}
});

// The favourite star is `ResourceList`'s, enabled purely by passing
// `onfavorite` — which is also what makes it clickable. Pinned because the
// handler now comes from the shared `listActions` module rather than a local
// copy, so a bad import would silently drop the affordance.
it('renders a clickable favourite badge on each row', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getByTestId('notes.txt')).toBeTruthy());

	expect(screen.getByTestId('resource-list-favorite-f1-btn')).toBeTruthy();
});

// "Modified by" is `ResourceList`'s own owner column — the same
// `<UserVignette>` in the same `.owner-cell` that `/shared-with-me` gets from
// `showOwner`, not a badge beside the filename. It shows on EVERY axis,
// people included.
//
// And the strong part: the cell is rendered from the ROW. One file in two
// lanes must name Bob in his row and Carol in hers. The component's default
// owner cell reads `ctx.ownerId` from a map keyed by `item.id`, which can
// hold only one of the two — so this is the assertion the `ownerCell` snippet
// exists for, and the reason the column no longer has to hide itself on the
// page's default axis to stay truthful.
it('shows Modified by per row, so one file in two lanes names both people', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [
			change('f1', 'notes.txt', 'carol', '2024-01-02T00:00:00Z'),
			change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')
		],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getAllByTestId('notes.txt')).toHaveLength(2));

	// Asserting on the CELLS, not on the text: the group-by button displays
	// the active axis label, so "Modified by" is on the page either way —
	// which is what made an earlier version of this test fail.
	await waitFor(() => {
		const cells = [...document.querySelectorAll('.owner-cell')].map((e) => e.textContent ?? '');
		expect(cells.some((c) => c.includes('Bob'))).toBe(true);
		expect(cells.some((c) => c.includes('Carol'))).toBe(true);
	});
});

// `per_lane` caps PEOPLE lanes, which are the only lanes the server knows
// about — the date and type axes both ride `order_by=changed_at`, where the
// server answers 400 rather than ignoring it. So the toggle renders only on
// the people axis, and the parameter is sent only from there. Both halves
// pinned: sending it on the wrong axis is a broken page, not a cosmetic slip.
it('caps people lanes, and sends no cap on the other axes', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(listRecentChanges).toHaveBeenCalled());

	expect(listRecentChanges).toHaveBeenLastCalledWith(
		expect.objectContaining({ query: expect.objectContaining({ per_lane: 10 }) })
	);
	// Placement, not just presence — the same check `/recent` makes of its
	// Clear button, for the mirror-image reason. This belongs in
	// `.view-toggle` with the group-by and view controls, because it changes
	// what you SEE; `.action-buttons` is for things that act on the page.
	const filterBtn = screen.getByTestId('changes-filter-btn');
	expect(filterBtn.closest('.view-toggle')).toBeTruthy();
	expect(filterBtn.closest('.action-buttons')).toBeNull();
	// LAST in the cluster, after the grid/list toggle — the trailing slot,
	// so the page's own control adds to the row rather than splitting the
	// built-in controls apart.
	const cluster = filterBtn.closest('.view-toggle') as HTMLElement;
	const controls = [...cluster.querySelectorAll('[data-testid]')].map((e) =>
		e.getAttribute('data-testid')
	);
	expect(controls.at(-1)).toBe('changes-filter-btn');
	expect(controls).toContain('display-mode-view-toggle-btn');

	await fireEvent.click(filterBtn);
	expect(await screen.findByTestId('changes-cap-toggle')).toBeTruthy();

	await fireEvent.click(screen.getByTestId('display-mode-groupby-btn'));
	await fireEvent.click(await screen.findByTestId('display-mode-groupby-type-item'));

	await waitFor(() =>
		expect(listRecentChanges).toHaveBeenLastCalledWith(
			expect.objectContaining({
				query: expect.objectContaining({ order_by: 'changed_at', per_lane: null })
			})
		)
	);
	// The panel is still open — jsdom's `fireEvent.click` dispatches no
	// `pointerdown`, so the outside-click dismissal never fired. Clicking the
	// trigger again would CLOSE it and make the next assertion pass because
	// the whole panel vanished, which is what an earlier version of this test
	// did. Assert the panel is open (the window radios are there) and the cap
	// row specifically is not.
	expect(screen.getByTestId('changes-days-30')).toBeTruthy();
	expect(screen.queryByTestId('changes-cap-toggle')).toBeNull();
});

// Turning the cap off refetches from the first page: the two modes cut pages
// differently — capped breaks on lane boundaries, uncapped anywhere — so the
// server refuses a cursor minted under the other one.
it('refetches from the first page when the cap is toggled', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: 'cursor-1'
	});
	render(ChangesPage);
	await waitFor(() => expect(listRecentChanges).toHaveBeenCalled());

	await fireEvent.click(screen.getByTestId('changes-filter-btn'));
	await fireEvent.click(await screen.findByTestId('changes-cap-toggle'));

	await waitFor(() =>
		expect(listRecentChanges).toHaveBeenLastCalledWith(
			expect.objectContaining({ query: expect.objectContaining({ per_lane: null, cursor: null }) })
		)
	);
});

// No sort-direction toggle, because the endpoint has no `reverse` parameter.
// `DisplayModeControls` renders that button whenever `groups` is non-empty, so
// passing `groupBys` was enough to make it appear — bound to a variable
// `load()` could not send, re-fetching identical rows on every click. The
// group-by menu must still be there; it is the control that works.
it('offers no sort direction, since the feed has none', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: null
	});
	render(ChangesPage);
	await waitFor(() => expect(screen.getByTestId('notes.txt')).toBeTruthy());

	expect(screen.queryByTestId('display-mode-sort-direction-btn')).toBeNull();
	expect(screen.queryByTestId('display-mode-groupby-btn')).toBeTruthy();
});

// Changing the window must start a fresh page. A cursor minted under the old
// window is refused by the server (400) because the window decides which
// rows exist — so carrying one across would surface as an error toast rather
// than a shorter list.
it('refetches from the first page when the window changes', async () => {
	m(listRecentChanges).mockResolvedValue({
		items: [change('f1', 'notes.txt', 'bob', '2024-01-01T00:00:00Z')],
		next_cursor: 'cursor-1'
	});
	render(ChangesPage);
	await waitFor(() => expect(listRecentChanges).toHaveBeenCalled());
	expect(listRecentChanges).toHaveBeenLastCalledWith(
		expect.objectContaining({ query: expect.objectContaining({ days: 30 }) })
	);

	await fireEvent.click(screen.getByTestId('changes-filter-btn'));
	await fireEvent.click(await screen.findByTestId('changes-days-7'));

	await waitFor(() =>
		expect(listRecentChanges).toHaveBeenLastCalledWith(
			expect.objectContaining({ query: expect.objectContaining({ days: 7, cursor: null }) })
		)
	);
});

// Default axis is PEOPLE, per the plan: the question is "what did my
// colleagues touch", and the default view has to answer it without the user
// first changing a control.
it('defaults to the people axis', async () => {
	m(listRecentChanges).mockResolvedValue({ items: [], next_cursor: null });
	render(ChangesPage);
	await waitFor(() =>
		expect(listRecentChanges).toHaveBeenCalledWith(
			expect.objectContaining({ query: expect.objectContaining({ order_by: 'modifier' }) })
		)
	);
});
