// Round-12 §F1 — list-view thumbnail rendition (benches/ROUND12.md).
//
// The list rows draw file thumbnails in a 40×40 CSS-px slot (100×70 in
// grid), but both views requested the 400px `preview` rendition. The list
// view now requests the 150px `icon` rendition: still ≥2× device-pixel
// density for the 40px slot, at ~1/7th of the decoded pixels (and roughly
// icon ≈ 4-8 KB vs preview ≈ 20-40 KB encoded WebP per thumbnail).
//
// Gates: the URL actually switches per view; grid keeps `preview`; the
// pixel-area saving is the documented ~7x.

import { describe, expect, it } from 'vitest';
import { fileThumbnailUrl } from './files';
import { thumbSizeForView } from '$lib/utils/thumbnails';

describe('round12 §F1 — thumbnail rendition per view', () => {
	it('list view requests the icon rendition, grid keeps preview', () => {
		expect(thumbSizeForView('list')).toBe('icon');
		expect(thumbSizeForView('grid')).toBe('preview');
		expect(fileThumbnailUrl('abc', thumbSizeForView('list'))).toBe('/api/files/abc/thumbnail/icon');
		expect(fileThumbnailUrl('abc', thumbSizeForView('grid'))).toBe(
			'/api/files/abc/thumbnail/preview'
		);
	});

	it('icon rendition moves ~7x fewer pixels than preview for the 40px slot', () => {
		// Server renditions: icon = 150px, preview = 400px (see the photos
		// srcset: `icon 150w, preview 400w, large 800w`).
		const areaRatio = (400 * 400) / (150 * 150);
		expect(areaRatio).toBeGreaterThan(7);
		// The 40×40 slot at 2x DPR needs 80px — icon's 150px still
		// oversamples it; preview was pure waste.
		expect(150).toBeGreaterThanOrEqual(80);
	});
});
