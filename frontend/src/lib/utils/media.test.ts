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
		// Post-ISO-8601 wire shape — `created_at` is an RFC 3339 UTC string.
		// Date.parse returns ms since epoch.
		expect(photoTimestamp({ created_at: '1970-01-01T00:00:05Z' } as never)).toBe(5000);
		expect(photoTimestamp({} as never)).toBe(0);
	});
	it('minimalPhotoItem stubs a FileItem from an id', () => {
		const p = minimalPhotoItem('abc');
		expect(p.id).toBe('abc');
		expect(p.category).toBe('image');
	});
});
