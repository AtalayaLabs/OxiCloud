import { it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';

/**
 * Regression pin — the "Type" column must render `category` through
 * `typeLabel`, never raw.
 *
 * `category` is the server's ENGLISH machine key ("Image", "Spreadsheet"),
 * the input to `typeLabel` and to the group-by buckets — it is not display
 * text. The column rendered `{item.category ?? ''}` directly, so every
 * listing that sets `showType` showed untranslated English for non-English
 * locales. `/files` was unaffected because it already routed its group-by
 * through `typeLabel`, which is exactly why the gap survived: the one route
 * anyone checked looked right.
 *
 * `typeLabel` is mocked to a sentinel rather than driving a real locale.
 * That makes the pin deterministic and aims it at the regression itself —
 * "did the value go through the mapper" — instead of depending on which
 * locale files the test environment happens to have loaded. `typeLabel`'s
 * own behaviour (including its pass-through for unknown keys) is covered in
 * `utils/resourceBuckets.test.ts`.
 */

const { typeLabel } = vi.hoisted(() => ({
	typeLabel: vi.fn((c: string) => `LOCALISED(${c})`)
}));
vi.mock('$lib/utils/resourceBuckets', () => ({
	typeLabel,
	// `ResourceList` imports only `typeLabel`, but the module also exports
	// these two and other importers share the mock registry within a file.
	dateBucket: vi.fn(() => ''),
	sizeBucket: vi.fn(() => '')
}));

import ResourceList from './ResourceList.svelte';

function fileItem(category: string) {
	return {
		category,
		created_at: '1970-01-01T00:00:00Z',
		id: 'f1',
		mime_type: 'application/vnd.ms-excel',
		modified_at: '1970-01-01T00:00:00Z',
		name: 'budget.xlsx',
		created_by: null,
		updated_by: null,
		folder_id: 'home',
		path: '/budget.xlsx',
		size: 4,
		etag: 'e',
		content_hash: 'h',
		is_favorite: false,
		is_shared: false
	};
}

it('renders the Type column through typeLabel, not the raw category', async () => {
	render(ResourceList, {
		props: {
			items: [fileItem('Spreadsheet')],
			showType: true
		}
	});

	const cell = await screen.findByTestId('type-cell');

	// The mapped value is what reaches the DOM…
	expect(cell.textContent?.trim()).toBe('LOCALISED(Spreadsheet)');
	// …and the raw English key is NOT. This is the assertion that fails if
	// anyone reverts to `{item.category}`.
	expect(cell.textContent?.trim()).not.toBe('Spreadsheet');
	expect(typeLabel).toHaveBeenCalledWith('Spreadsheet');
});

it('renders an empty Type cell rather than calling typeLabel on nothing', async () => {
	typeLabel.mockClear();
	render(ResourceList, {
		props: {
			// A folder row synthesised without a category, and the DriveDto arm
			// of `ResourceContentDto`, both reach the cell with `category`
			// absent. `typeLabel('')` would return `''` anyway, but the guard
			// keeps the call out of the hot render path for every such row.
			items: [{ ...fileItem(''), category: undefined as unknown as string }],
			showType: true
		}
	});

	const cell = await screen.findByTestId('type-cell');
	expect(cell.textContent?.trim()).toBe('');
	expect(typeLabel).not.toHaveBeenCalled();
});
