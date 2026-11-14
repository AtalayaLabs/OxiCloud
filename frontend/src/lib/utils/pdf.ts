/**
 * Does this browser have an embedded PDF viewer?
 *
 * `navigator.pdfViewerEnabled` is the spec'd signal for "a PDF in an
 * `<iframe>`/`<embed>`/`<object>` will render here". It is NOT a guess about
 * the platform: measured on the same OxiCloud build, it reports `true` in
 * desktop Chrome and `false` in Chrome on Android, which is exactly the split
 * behind issue #780 — the PDF preview worked on the desktop and showed a
 * broken-document icon on the phone.
 *
 * Why the flag and not a user-agent test: the capability is a property of the
 * browser build, not of the OS. Chrome on Android ships no embedded viewer;
 * Firefox renders PDFs with pdf.js and reports `true` everywhere. A UA string
 * would have to be re-guessed every time either changes.
 *
 * Returns `false` when the property is missing, which is the safe direction:
 * an unknown browser gets our own renderer, which works regardless, instead
 * of an iframe that may silently show nothing.
 */
export function hasNativePdfViewer(): boolean {
	if (typeof navigator === 'undefined') return false;
	// Older TS DOM libs don't declare `pdfViewerEnabled`. Narrow the one
	// property rather than casting the whole `Navigator` to `any`.
	const nav = navigator as Navigator & { pdfViewerEnabled?: boolean };
	return nav.pdfViewerEnabled === true;
}
