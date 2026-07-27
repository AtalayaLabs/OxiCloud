// Round-18 frontend micro-pack (benches/ROUND18.md §F1).
//
// Each section is BEFORE (verbatim replica of the shipped-before shape) vs
// AFTER (the shipped incremental builder), with an equivalence gate, a
// reference-contract gate, and a wall-time perf gate — the same discipline as
// the Rust micro-packs: an AFTER that doesn't beat its BEFORE fails the gate.

import { describe, expect, it } from 'vitest';
import { buildItemIndex, ItemIndexBuilder } from '$lib/utils/itemIndex';

// ────────────────────────────────────────────────────────────────────────────
// [F1] ResourceList `itemIndexById` — rebuild-a-fresh-Map-per-page vs incremental
// ────────────────────────────────────────────────────────────────────────────
//
// Audit finding (ROUND17 deferred list): ResourceList derived
// `itemIndexById = new Map(items.map((i, idx) => [i.id, idx]))`. Every
// infinite-scroll page (`items = [...items, ...page]`) rebuilt a brand-new Map
// over the WHOLE accumulated list — O(N) per page, Σ O(N²) across a P-page
// drain — and, being a fresh instance each page, re-fired the reap-stale
// `$effect` that reference-diffs it (allocating another O(N) id Set for a reap
// an append can never trigger). `ItemIndexBuilder` extends the persistent Map
// with the fresh page only and returns the same reference on an append.

interface Item {
	id: string;
}

/** A page of `{ id }` items (50/page, the default page size). */
function pageOf(start: number, n: number): Item[] {
	return Array.from({ length: n }, (_, i) => ({ id: `it-${start + i}` }));
}

/** BEFORE: rebuild a fresh Map over the whole accumulated list each page. */
function rebuildPerPage(pages: Item[][]): Map<string, number> {
	let acc: Item[] = [];
	let index = new Map<string, number>();
	for (const page of pages) {
		acc = [...acc, ...page]; // the component's `items = [...items, ...page]`
		index = new Map(acc.map((i, idx) => [i.id, idx])); // new instance + O(N) rebuild
	}
	return index;
}

/** AFTER: one persistent builder, extend with only the fresh page's ids. */
function incrementalPerPage(pages: Item[][]): Map<string, number> {
	const builder = new ItemIndexBuilder<Item>();
	let acc: Item[] = [];
	let index = new Map<string, number>();
	for (const page of pages) {
		acc = [...acc, ...page];
		index = builder.sync(acc);
	}
	return index;
}

describe('round18 §F1 — ResourceList itemIndexById incremental Map', () => {
	it('final index is identical to the full rebuild (equivalence gate)', () => {
		const pages = Array.from({ length: 20 }, (_, p) => pageOf(p * 50, 50));
		const acc = pages.flat();
		const before = rebuildPerPage(pages);
		const after = incrementalPerPage(pages);
		const reference = buildItemIndex(acc);
		expect(after.size).toBe(before.size);
		for (const [id, idx] of reference) expect(after.get(id)).toBe(idx);
		for (const [id, idx] of after) expect(before.get(id)).toBe(idx);
	});

	it('the index stays deep-equal to the reference at EVERY page (equivalence gate)', () => {
		const builder = new ItemIndexBuilder<Item>();
		let acc: Item[] = [];
		for (let p = 0; p < 12; p++) {
			acc = [...acc, ...pageOf(p * 50, 50)];
			const got = builder.sync(acc);
			const want = buildItemIndex(acc);
			expect(got.size).toBe(want.size);
			for (const [id, idx] of want) expect(got.get(id)).toBe(idx);
		}
	});

	it('a later duplicate id resolves to its highest index, matching Map (equivalence gate)', () => {
		// The old `new Map(items.map(...))` keeps the last (highest-index)
		// occurrence of a duplicate id; the incremental extend must too.
		const builder = new ItemIndexBuilder<Item>();
		const dup: Item = { id: 'dup' };
		const p1 = [dup, { id: 'a' }];
		const p2 = [{ id: 'b' }, dup]; // 'dup' re-appears at index 3
		builder.sync(p1);
		const got = builder.sync([...p1, ...p2]);
		const want = buildItemIndex([...p1, ...p2]);
		expect(got.get('dup')).toBe(want.get('dup'));
		expect(got.get('dup')).toBe(3);
	});

	it('reuses the Map reference on append, mints a new one on rebuild (reference-contract gate)', () => {
		const builder = new ItemIndexBuilder<Item>();
		const p1 = pageOf(0, 50);
		const first = builder.sync(p1);
		// Append: same reference (so the reap-stale effect does NOT re-fire —
		// an append removes nothing).
		const appended = builder.sync([...p1, ...pageOf(50, 50)]);
		expect(appended).toBe(first);
		// Deletion (shorter, non-append prefix): fresh reference (so the
		// reap-stale effect DOES re-fire and drops the removed id).
		const afterDelete = builder.sync(p1.slice(0, 40));
		expect(afterDelete).not.toBe(first);
		expect(afterDelete.has('it-49')).toBe(false);
		// Reload with a different first element (non-append): fresh reference.
		const reloaded = builder.sync(pageOf(1000, 50));
		expect(reloaded).not.toBe(afterDelete);
	});

	it('a P-page drain builds the index ≥4x faster incrementally (perf gate)', () => {
		const PAGES = 40;
		const PER = 50; // 2 000 items total
		const pages = Array.from({ length: PAGES }, (_, p) => pageOf(p * PER, PER));

		const run = (f: (p: Item[][]) => Map<string, number>): number => {
			const t0 = performance.now();
			for (let r = 0; r < 20; r++) f(pages);
			return performance.now() - t0;
		};

		// Warm-up (JIT) then measure.
		run(rebuildPerPage);
		run(incrementalPerPage);
		const beforeMs = run(rebuildPerPage);
		const afterMs = run(incrementalPerPage);

		console.info(
			`§F1 ${PAGES} pages × ${PER}: rebuild-per-page ${beforeMs.toFixed(1)} ms vs incremental ${afterMs.toFixed(1)} ms (${(beforeMs / afterMs).toFixed(1)}x)`
		);
		// Ratio threshold: single-shot microbenches on shared CI runners
		// (throttled CPU, cold caches, sibling load) routinely wobble
		// ±20 %. 4× still catches any real regression — a correctness
		// break would collapse the ratio to <2× — while surviving
		// runner noise. Local dev machines see 6–10× consistently.
		expect(afterMs).toBeLessThan(beforeMs / 4);
	});
});
