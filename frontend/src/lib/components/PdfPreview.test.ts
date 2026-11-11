import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';

/**
 * Regression pins for issue #780 — the PDF preview that worked on desktop
 * Chrome and showed a broken-document icon on Chrome for Android, where
 * `navigator.pdfViewerEnabled` is `false` and no `<iframe>` can render a PDF.
 * This component is the replacement path, so these pin the behaviours that
 * make it safe to run on the device class that forced it to exist:
 *
 *   * one canvas per page, so a multi-page document is not silently truncated;
 *   * the DPR cap, so a phone reporting 3 does not allocate ~2.25× the canvas
 *     pixels and get the tab killed;
 *   * `isEvalSupported: false`, so the app's CSP never needs `unsafe-eval`;
 *   * `page.cleanup()` per page, the difference between a long preview and an
 *     out-of-memory tab;
 *   * the failure path showing a message rather than an empty pane — the
 *     original bug's whole problem was failing invisibly.
 *
 * pdf.js itself is mocked: the real module is a 300 KB vendored ESM file
 * loaded from a runtime URL, which a unit test should neither fetch nor
 * execute. The mock asserts the call shape instead.
 */

const renderMock = vi.fn(() => ({ promise: Promise.resolve() }));
const cleanupMock = vi.fn();
const destroyMock = vi.fn();
const getDocumentMock = vi.fn();

vi.mock('$lib/utils/thumbnail', () => ({
	getPdfjsLib: () => Promise.resolve({ getDocument: getDocumentMock })
}));

function pdfWithPages(numPages: number) {
	return {
		numPages,
		destroy: destroyMock,
		getPage: vi.fn(() =>
			Promise.resolve({
				getViewport: ({ scale }: { scale: number }) => ({
					width: 600 * scale,
					height: 800 * scale
				}),
				render: renderMock,
				cleanup: cleanupMock
			})
		)
	};
}

import PdfPreview from './PdfPreview.svelte';

describe('PdfPreview', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		getDocumentMock.mockReturnValue({ promise: Promise.resolve(pdfWithPages(3)) });
		// jsdom canvases have no 2d context by default; the component treats a
		// null context as a failure, which would mask the real assertions.
		HTMLCanvasElement.prototype.getContext = vi.fn(
			() => ({}) as unknown as CanvasRenderingContext2D
		) as unknown as typeof HTMLCanvasElement.prototype.getContext;
	});

	it('draws one canvas per page and releases each page after rendering', async () => {
		render(PdfPreview, { data: new ArrayBuffer(8), name: 'report.pdf' });

		const pages = screen.getByTestId('pdf-preview-pages');
		await waitFor(() => expect(pages.querySelectorAll('canvas').length).toBe(3));
		// Released as we go, not at the end: a 200-page document must not hold
		// 200 pages' worth of pdf.js structures to show page 1.
		expect(cleanupMock).toHaveBeenCalledTimes(3);
	});

	it('asks pdf.js not to use eval, so the CSP needs no unsafe-eval', async () => {
		render(PdfPreview, { data: new ArrayBuffer(8), name: 'report.pdf' });

		await waitFor(() => expect(getDocumentMock).toHaveBeenCalled());
		expect(getDocumentMock.mock.calls[0][0]).toMatchObject({ isEvalSupported: false });
	});

	it('caps the backing store at 2× device pixels', async () => {
		// A phone reporting 3 is common. Uncapped, a full-width page here
		// would be 1800×2400 = 4.3 MP per page instead of 1.9 MP.
		Object.defineProperty(window, 'devicePixelRatio', { value: 3, configurable: true });
		render(PdfPreview, { data: new ArrayBuffer(8), name: 'report.pdf' });

		const pages = screen.getByTestId('pdf-preview-pages');
		await waitFor(() => expect(pages.querySelector('canvas')).not.toBeNull());
		const canvas = pages.querySelector('canvas') as HTMLCanvasElement;
		// jsdom reports clientWidth 0, so the component falls back to the
		// page's own width (600) — the assertion is about the DPR factor.
		expect(canvas.width).toBe(1200);
		expect(canvas.height).toBe(1600);
	});

	it('shows a message when the document cannot be read', async () => {
		getDocumentMock.mockReturnValue({ promise: Promise.reject(new Error('corrupt')) });
		render(PdfPreview, { data: new ArrayBuffer(8), name: 'broken.pdf' });

		// Visible failure, not an empty pane. #780 was reported precisely
		// because a broken preview looked like a blank one.
		await waitFor(() => expect(screen.getByText('Could not load preview.')).toBeTruthy());
	});
});
