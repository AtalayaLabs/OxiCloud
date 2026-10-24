import { describe, expect, it } from 'vitest';
import type { ResourceCategory } from '$lib/api/types';
import type { PhotoItem } from '$lib/api/endpoints/photos';
import { photoTimestamp } from './media';
import {
	PhotoTimeline,
	buildPhotoRows,
	type GroupMode,
	type LayoutMode,
	type TimelineConfig
} from './photoTimeline';

/**
 * Benchmark gate for the incremental photo timeline (PhotoTimeline) that
 * replaced the photos view's `groups`→`photoRows` derive chain.
 *
 * Audit finding: `loadMore` does `items = [...items, ...page]` (60/page), and
 * both `groups` (O(N), a `new Date()` per photo) and `photoRows` (O(N) row
 * layout) are `$derived` over the whole accumulated list — so paging to photo
 * N re-groups + re-lays-out everything loaded so far, Σ ≈ O(N²/60) main-thread
 * work during the scroll (the same class ROUND6 fixed for the files listing).
 * Since pages arrive newest-first, grouping is append-only; PhotoTimeline
 * re-buckets only the fresh page and re-lays-out only the groups that changed.
 *
 * Gates:
 *  1. Equivalence — at EVERY page of the drain, the incremental output is
 *     deep-equal to the verbatim full-rebuild reference (buildPhotoRows), for
 *     both layouts; plus config-change, deletion and width=0 fall back to a
 *     correct full rebuild.
 *  2. Perf — grouping work (timestamp reads) collapses from Σ O(N²/60) to O(N)
 *     across the drain (deterministic count), and wall drops ≥3x.
 */

const DAY = 86_400; // seconds

/** A photo with a descending sort_date and a deterministic aspect ratio. */
function photo(i: number): PhotoItem {
	// Newest-first: photo 0 is most recent; ~half a day apart spans ~4 years
	// over 3k photos, so month/day buckets are bounded (realistic library).
	const sortDate = 1_700_000_000 - i * (DAY / 2);
	// Every date on the wire is an RFC 3339 UTC string, `sort_date` included
	// since the integer form was retired. The numeric `sortDate` above is
	// just the stride generator.
	const sortDateIso = new Date(sortDate * 1000).toISOString();
	const w = 200 + ((i * 37) % 400);
	const h = 200 + ((i * 53) % 300);
	return {
		category: 'Image',
		created_at: sortDateIso,
		id: `p-${i.toString().padStart(6, '0')}`,
		mime_type: 'image/jpeg',
		modified_at: sortDateIso,
		name: `photo ${i}.jpg`,
		created_by: null,
		updated_by: null,
		folder_id: 'f',
		path: `/photo ${i}.jpg`,
		size: 1000,
		sort_date: sortDateIso,
		etag: `e${i}`,
		content_hash: `h${i}`,
		width: w,
		height: h
	} as PhotoItem;
}

/** Instrumented config: counts every timestamp read (the grouping hot op). */
function makeConfig(
	groupMode: GroupMode,
	layoutMode: LayoutMode,
	width: number,
	counter?: { n: number }
): TimelineConfig {
	// Delegates to the production helper rather than re-implementing the
	// field precedence: a local copy drifted from it once already, and the
	// bench is measuring the real grouping hot path, so it should be reading
	// the date the real one reads.
	const timestampOf = (p: PhotoItem): number => {
		if (counter) counter.n++;
		return photoTimestamp(p);
	};
	// Stable label fn (reference identity matters for the config-unchanged path).
	const labelOf = (d: Date, mode: GroupMode) =>
		mode === 'year'
			? `${d.getFullYear()}`
			: mode === 'month'
				? `${d.getFullYear()}-${d.getMonth() + 1}`
				: `${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()}`;
	return { groupMode, layoutMode, width, mobile: false, timestampOf, labelOf };
}

const PAGE = 60;
const PAGES = 50; // 3 000-photo drain
const WIDTH = 1200;

describe('incremental photo timeline (benchmark gate)', () => {
	for (const layout of ['square', 'justified'] as LayoutMode[]) {
		it(`stays deep-equal to the full rebuild at every page — ${layout}`, () => {
			const all = Array.from({ length: PAGE * PAGES }, (_, i) => photo(i));
			const cfg = makeConfig('month', layout, WIDTH);
			const timeline = new PhotoTimeline();
			for (let p = 1; p <= PAGES; p++) {
				const cumulative = all.slice(0, p * PAGE);
				const incremental = timeline.sync(cumulative, cfg);
				const reference = buildPhotoRows(cumulative, cfg);
				expect(incremental, `page ${p}`).toEqual(reference);
			}
		});
	}

	it('falls back to a correct full rebuild on config change, deletion and width=0', () => {
		const all = Array.from({ length: 600 }, (_, i) => photo(i));
		const timeline = new PhotoTimeline();
		const monthSquare = makeConfig('month', 'square', WIDTH);

		// Drain a few pages, then flip layout — must equal a fresh full rebuild.
		timeline.sync(all.slice(0, 300), monthSquare);
		const justified = makeConfig('month', 'justified', WIDTH);
		expect(timeline.sync(all.slice(0, 300), justified)).toEqual(
			buildPhotoRows(all.slice(0, 300), justified)
		);

		// Change group mode.
		const yearJust = makeConfig('year', 'justified', WIDTH);
		expect(timeline.sync(all.slice(0, 300), yearJust)).toEqual(
			buildPhotoRows(all.slice(0, 300), yearJust)
		);

		// Deletion (list shrinks / prefix changes) → rebuild.
		const shrunk = all.slice(0, 300).filter((_, i) => i % 7 !== 0);
		expect(timeline.sync(shrunk, yearJust)).toEqual(buildPhotoRows(shrunk, yearJust));

		// width=0 yields [] and doesn't wedge the next positive-width sync.
		const zero = makeConfig('year', 'justified', 0);
		expect(timeline.sync(shrunk, zero)).toEqual([]);
		expect(timeline.sync(shrunk, yearJust)).toEqual(buildPhotoRows(shrunk, yearJust));
	});

	it('collapses grouping work from Σ O(N²/page) to O(N) and runs ≥3x faster', () => {
		const N = PAGE * PAGES;
		const all = Array.from({ length: N }, (_, i) => photo(i));

		// AFTER: incremental — each photo is bucketed exactly once across the drain.
		const afterCounter = { n: 0 };
		const afterCfg = makeConfig('month', 'square', WIDTH, afterCounter);
		const timeline = new PhotoTimeline();
		const t1 = performance.now();
		for (let p = 1; p <= PAGES; p++) timeline.sync(all.slice(0, p * PAGE), afterCfg);
		const afterMs = performance.now() - t1;

		// BEFORE: full rebuild per page — re-buckets the whole cumulative list.
		const beforeCounter = { n: 0 };
		const beforeCfg = makeConfig('month', 'square', WIDTH, beforeCounter);
		const t0 = performance.now();
		for (let p = 1; p <= PAGES; p++) buildPhotoRows(all.slice(0, p * PAGE), beforeCfg);
		const beforeMs = performance.now() - t0;

		console.info(
			`photo timeline ${PAGES}×${PAGE}: before ${beforeCounter.n} timestamp reads / ${beforeMs.toFixed(1)} ms — after ${afterCounter.n} reads / ${afterMs.toFixed(1)} ms (${(beforeCounter.n / afterCounter.n).toFixed(1)}x fewer reads, ${(beforeMs / afterMs).toFixed(1)}x wall)`
		);

		// Incremental buckets each photo once: exactly N reads.
		expect(afterCounter.n).toBe(N);
		// Full rebuild is quadratic: Σ_{p=1..P} p·PAGE.
		expect(beforeCounter.n).toBe((PAGES * (PAGES + 1) * PAGE) / 2);
		expect(afterCounter.n).toBeLessThan(beforeCounter.n / 5);
		expect(afterMs).toBeLessThan(beforeMs / 3);
	});
});
