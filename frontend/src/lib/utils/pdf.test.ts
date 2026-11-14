import { describe, it, expect, afterEach } from 'vitest';
import { hasNativePdfViewer } from './pdf';

/**
 * Regression pins for issue #780: the PDF preview rendered on desktop Chrome
 * and showed a broken-document icon on Chrome for Android. The split is
 * `navigator.pdfViewerEnabled` — `true` on the desktop, `false` on the phone,
 * measured on the same build — and `FileViewer` branches on this helper to
 * decide between framing a blob and rendering with pdf.js.
 *
 * What a regression here looks like: someone "simplifies" the check to a
 * truthiness test, or inverts the missing-property default. Either silently
 * sends Android back to the iframe, which fails as a broken icon rather than
 * as an error — the quietest possible way to reintroduce the bug.
 */
describe('hasNativePdfViewer', () => {
	function setFlag(value: unknown): void {
		Object.defineProperty(navigator, 'pdfViewerEnabled', {
			value,
			configurable: true
		});
	}

	afterEach(() => {
		// jsdom's Navigator has no such property; remove ours so each case
		// starts from the real absent state rather than the previous value.
		Reflect.deleteProperty(navigator, 'pdfViewerEnabled');
	});

	it('is true only when the browser reports an embedded viewer', () => {
		setFlag(true);
		expect(hasNativePdfViewer()).toBe(true);
	});

	it('is false when the browser reports no embedded viewer (Chrome on Android)', () => {
		setFlag(false);
		expect(hasNativePdfViewer()).toBe(false);
	});

	it('is false when the property is missing', () => {
		// The safe direction: an unknown browser gets our own renderer, which
		// works everywhere, rather than an iframe that may show nothing.
		expect('pdfViewerEnabled' in navigator).toBe(false);
		expect(hasNativePdfViewer()).toBe(false);
	});

	it('does not treat a truthy non-boolean as support', () => {
		// Strict `=== true`, so a polyfill or extension setting a string
		// cannot route a viewer-less browser back to the broken iframe path.
		setFlag('yes');
		expect(hasNativePdfViewer()).toBe(false);
	});
});
