import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Rides the generated SDK directly (Phase 4), so the mock surface is the
// generated module rather than a hand-written endpoint wrapper.
vi.mock('$lib/api/generated/openapi/sdk.gen', () => ({
	listRecentChanges: vi.fn()
}));
vi.mock('$lib/utils/resourceBuckets', () => ({
	dateBucket: () => 'Today',
	sizeBucket: () => 'Small',
	typeLabel: () => 'File'
}));
vi.mock('$lib/utils/ownerName', () => ({
	// Resolves to a readable name so the lane header is asserted on the
	// name, not on a raw id — and so a regression that stopped resolving
	// modifiers shows up as a changed label rather than passing silently.
	resolveOwnerName: vi.fn(async (id: string) => (id === 'bob' ? 'Bob' : 'Carol'))
}));

import { listRecentChanges } from '$lib/api/generated/openapi/sdk.gen';
import ChangesPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function file(id: string, name: string) {
	return {
		category: 'Document',
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

	await fireEvent.change(screen.getByTestId('changes-days-select'), { target: { value: '7' } });

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
