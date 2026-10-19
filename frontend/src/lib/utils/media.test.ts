import { describe, it, expect } from 'vitest';
import { isVideo, photoTimestamp, minimalPhotoItem } from './media';
describe('media helpers', () => {
	it('isVideo detects video mime types', () => {
		expect(isVideo({ mime_type: 'video/mp4' } as never)).toBe(true);
		expect(isVideo({ mime_type: 'image/png' } as never)).toBe(false);
		expect(isVideo({} as never)).toBe(false);
	});
	it('photoTimestamp scales seconds to milliseconds', () => {
		// Legacy numeric `sort_date` seed (seconds) is scaled × 1000.
		expect(photoTimestamp({ sort_date: 1000 } as never)).toBe(1_000_000);
		// Values ≥ 1e12 are already ms and pass through.
		expect(photoTimestamp({ sort_date: 2_000_000_000_000 } as never)).toBe(2_000_000_000_000);
		// `created_at` is used only when there is no `sort_date` — synthesised
		// items (minimalPhotoItem) are the real case.
		expect(photoTimestamp({ created_at: '1970-01-01T00:00:05Z' } as never)).toBe(5000);
		expect(photoTimestamp({} as never)).toBe(0);
	});
	it('photoTimestamp prefers sort_date over created_at', () => {
		// Regression guard for the /photos `each_key_duplicate` crash.
		//
		// The timeline is ORDERED BY media_sort_date and the virtualiser only
		// ever extends the last group, so bucketing must use the same axis.
		// Preferring `created_at` let a closed month reopen.
		//
		// Real row from the broken instance: uploaded in October, content
		// dated July. `sort_date` is the July instant the server sorted on;
		// `created_at` is the October upload.
		//
		// Wire shape is RFC 3339 — the integer form was retired alongside
		// this fix so the two fields can no longer be told apart by type,
		// which is what made the wrong one look authoritative.
		expect(
			photoTimestamp({
				created_at: '2026-10-04T17:34:32Z',
				sort_date: '2026-09-14T22:19:35Z'
			} as never)
		).toBe(Date.parse('2026-09-14T22:19:35Z'));
		// Numeric seed still honoured, for fixtures and benches.
		expect(
			photoTimestamp({
				created_at: '2026-10-04T17:34:32Z',
				sort_date: 1789424375
			} as never)
		).toBe(1789424375 * 1000);
	});
	it('minimalPhotoItem stubs a FileItem from an id', () => {
		const p = minimalPhotoItem('abc');
		expect(p.id).toBe('abc');
		expect(p.category).toBe('image');
	});
});
