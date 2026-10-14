/**
 * Thumbnail-rendition policy. Pure one-liner today, but a dedicated
 * module keeps the view-mode → rendition mapping in one place so a
 * future policy change (e.g. a `large` grid view wanting the `original`
 * rendition) touches one definition instead of eight call sites.
 */

/**
 * Thumbnail size matched to the rendering slot. List rows draw thumbnails in
 * a 40×40 box, so the 150px `icon` rendition is already ≥2× retina density —
 * fetching the 400px `preview` there moved ~7× more pixels than the slot can
 * show (benches/ROUND12.md §F1). Grid cards (100×70 slot) keep `preview`.
 */
export function thumbSizeForView(view: 'grid' | 'list'): 'icon' | 'preview' {
	return view === 'list' ? 'icon' : 'preview';
}
