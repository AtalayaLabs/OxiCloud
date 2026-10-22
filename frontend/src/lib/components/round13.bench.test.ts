// Round-13 §V1 — grouped views are windowed (benches/ROUND13.md).
//
// Before this round, the grouped GRID path mounted EVERY card:
// `{#each sections}{#each section.rows}{@render row}` with no windowing
// (the grouped-by-default trash grid, and the files route's grouped grid,
// were the last unwindowed paths). Now each swimlane feeds its own windowed
// <VirtualList> — a flex stack of (header + windowed card grid) per section
// — so only a viewport-bounded slice of `.file-item` cards is realized,
// regardless of group size.
//
// Gate: render the real ResourceList in grouped GRID mode with N=800 items
// in one bucket and assert the mounted card count is viewport-bounded, not
// N. jsdom does no layout, so VirtualList's visible band is a small constant
// — the same lever the round-12 files page test documents.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/files', () => ({
	fileThumbnailUrl: () => '/thumb',
	thumbSizeForView: () => 'preview' as const
}));

import ResourceList from './ResourceList.svelte';
import type { GroupByDef } from './ResourceList.svelte';
import { files as filesStore } from '$lib/stores/files.svelte';

interface TestFile {
	category: string;
	/** RFC 3339 UTC — matches the FileItem wire shape post-pivot. */
	created_at: string;
	icon_class: string;
	id: string;
	mime_type: string;
	/** RFC 3339 UTC — matches the FileItem wire shape post-pivot. */
	modified_at: string;
	name: string;
	created_by: string;
	updated_by: string;
	folder_id: string;
	path: string;
	size: number;
	size_formatted: string;
	sort_date: number;
	etag: string;
	content_hash: string;
	is_favorite: boolean;
	is_shared: boolean;
}

function fileItem(i: number): TestFile {
	return {
		category: 'Document',
		created_at: '1970-01-01T00:00:00Z',
		icon_class: 'fa-file',
		id: `f${i}`,
		mime_type: 'text/plain',
		modified_at: '1970-01-01T00:00:00Z',
		name: `file-${i}.txt`,
		created_by: 'me',
		updated_by: 'me',
		folder_id: 'home',
		path: `/file-${i}.txt`,
		size: 4,
		size_formatted: '4 B',
		sort_date: 0,
		etag: 'e',
		content_hash: 'h',
		is_favorite: false,
		is_shared: false
	};
}

// Single bucket → one big swimlane (the worst case the old grid mounted whole).
const groupBys: GroupByDef[] = [
	{
		key: 'type',
		label: 'Type',
		orderBy: 'name',
		bucketOf: (item) => (item as TestFile).category ?? 'other',
		labelOf: (k) => k
	}
];

describe('round13 §V1 — grouped grid is windowed', () => {
	beforeEach(() => {
		filesStore.viewMode = 'grid';
	});

	it('mounts a viewport-bounded slice of cards, not all N, in grouped grid', () => {
		const N = 800;
		const items = Array.from({ length: N }, (_, i) => fileItem(i));
		const { container } = render(ResourceList, {
			props: {
				title: 'Round13',
				items,
				groupBys,
				groupBy: 'type',
				selectable: true,
				actions: undefined
			}
		});

		const mounted = container.querySelectorAll('.file-item').length;
		// A swimlane header confirms we are on the grouped path.
		expect(container.querySelectorAll('.rl-swimlane-header').length).toBeGreaterThan(0);
		// Windowed: the visible band is viewport+overscan bounded, far below N.
		// (The pre-fix grid-grouped path mounted all 800.)
		expect(mounted).toBeGreaterThan(0);
		expect(mounted).toBeLessThan(120);
		expect(mounted).toBeLessThan(N / 4);
	});

	it('full scroll height is still reserved (windowing spacer, not truncation)', () => {
		const N = 800;
		const items = Array.from({ length: N }, (_, i) => fileItem(i));
		const { container } = render(ResourceList, {
			props: { title: 'Round13', items, groupBys, groupBy: 'type', selectable: true }
		});
		// The VirtualList reserves total height via its `.vlist` spacer so the
		// scrollbar / end-of-list sentinel keep working — height must scale with
		// N, proving cards weren't simply dropped.
		const vlist = container.querySelector('.vlist') as HTMLElement | null;
		expect(vlist).not.toBeNull();
		const reserved = parseFloat(vlist!.style.height || '0');
		expect(reserved).toBeGreaterThan(1000);
	});
});
