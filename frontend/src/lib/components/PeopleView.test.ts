import { it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, screen, waitFor } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/people', () => ({
	fetchPeople: vi.fn(),
	fetchPersonPhotos: vi.fn(),
	renamePerson: vi.fn()
}));
vi.mock('$lib/api/endpoints/files', () => ({
	fileThumbnailUrl: () => '/thumb.png',
	thumbSizeForView: () => 'preview' as const
}));
vi.mock('$lib/stores/dialogs.svelte', () => ({ promptDialog: vi.fn() }));

import { fetchPeople, fetchPersonPhotos, renamePerson } from '$lib/api/endpoints/people';
import { promptDialog } from '$lib/stores/dialogs.svelte';
import PeopleView from './PeopleView.svelte';

const fp = fetchPeople as unknown as ReturnType<typeof vi.fn>;
const fpp = fetchPersonPhotos as unknown as ReturnType<typeof vi.fn>;
const rn = renamePerson as unknown as ReturnType<typeof vi.fn>;
const pd = promptDialog as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => vi.clearAllMocks());

it('renders the people grid (named + unnamed)', async () => {
	fp.mockResolvedValue([
		{ id: 'p1', name: 'Alice', face_count: 3, cover_file_id: 'c1' },
		{ id: 'p2', name: '', face_count: 1, cover_file_id: null }
	]);
	render(PeopleView);
	expect(await screen.findByText('Alice')).toBeTruthy();
	expect(screen.getByText('Unnamed')).toBeTruthy();
});

it('shows an empty state when there are no people', async () => {
	fp.mockResolvedValue([]);
	render(PeopleView);
	expect(await screen.findByText('No people yet')).toBeTruthy();
});

it('shows the disabled state when the list errors', async () => {
	fp.mockRejectedValue(new Error('off'));
	render(PeopleView);
	expect(await screen.findByText('Face recognition is disabled')).toBeTruthy();
});

it('drills into a person and back to the list', async () => {
	fp.mockResolvedValue([{ id: 'p1', name: 'Alice', face_count: 2, cover_file_id: null }]);
	fpp.mockResolvedValue(['ph1', 'ph2']);
	render(PeopleView);
	const btn = (await screen.findByText('Alice')).closest('button')!;
	await fireEvent.click(btn);
	await waitFor(() => expect(fpp).toHaveBeenCalledWith('p1'));
	await fireEvent.click(screen.getByLabelText('Back'));
	expect(await screen.findByText('Alice')).toBeTruthy();
});

it('renames the current person', async () => {
	fp.mockResolvedValue([{ id: 'p1', name: 'Alice', face_count: 1, cover_file_id: null }]);
	fpp.mockResolvedValue([]);
	// `promptDialog` callers migrated to the inline-error UX pattern pass
	// `action: async (next) => renamePerson(…)` instead of running the
	// mutation outside the await. Simulate the real `dialogs.resolve`
	// flow by invoking `opts.action` with the submitted value. See
	// `frontend/AGENTS.md § Dialog error UX`.
	pd.mockImplementation(async (opts) => {
		if (opts.action) await opts.action('Bob');
		return 'Bob';
	});
	rn.mockResolvedValue(undefined);
	render(PeopleView);
	await fireEvent.click((await screen.findByText('Alice')).closest('button')!);
	await waitFor(() => screen.getByLabelText('Name this person'));
	await fireEvent.click(screen.getByLabelText('Name this person'));
	await waitFor(() => expect(rn).toHaveBeenCalledWith('p1', 'Bob'));
});

it("opens a person's photo in the lightbox", async () => {
	fp.mockResolvedValue([{ id: 'p1', name: 'Alice', face_count: 2, cover_file_id: null }]);
	fpp.mockResolvedValue(['ph1', 'ph2']);
	const { container } = render(PeopleView);
	await fireEvent.click((await screen.findByText('Alice')).closest('button')!);
	await waitFor(() => expect(fpp).toHaveBeenCalledWith('p1'));
	const tiles = await waitFor(() => {
		const found = container.querySelectorAll('.photos__open');
		if (found.length === 0) throw new Error('no tiles yet');
		return found;
	});
	expect(tiles.length).toBe(2);
	await fireEvent.click(tiles[0]);
	expect(await screen.findByTestId('photo-lightbox')).toBeTruthy();
});
