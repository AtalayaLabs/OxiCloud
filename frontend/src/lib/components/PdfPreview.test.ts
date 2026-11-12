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

const cancelMock = vi.fn();
const renderMock = vi.fn(() => ({ promise: Promise.resolve(), cancel: cancelMock }));
const cleanupMock = vi.fn();
const destroyMock = vi.fn(() => Promise.resolve());
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
		getDocumentMock.mockReturnValue({
			promise: Promise.resolve(pdfWithPages(3)),
			destroy: destroyMock
		});
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
		getDocumentMock.mockReturnValue({
			promise: Promise.reject(new Error('corrupt')),
			destroy: destroyMock
		});
		render(PdfPreview, { data: new ArrayBuffer(8), name: 'broken.pdf' });

		// Visible failure, not an empty pane. #780 was reported precisely
		// because a broken preview looked like a blank one.
		await waitFor(() => expect(screen.getByText('Could not load preview.')).toBeTruthy());
	});
	it('keeps the new document when an older load resolves during its render', async () => {
		const oldLoad = Promise.withResolvers<ReturnType<typeof pdfWithPages>>();
		const firstPage = Promise.withResolvers<void>();
		const oldDoc = pdfWithPages(3);
		const newDoc = pdfWithPages(3);
		const destroyOld = vi.fn(() => Promise.resolve());
		const destroyNew = vi.fn(() => Promise.resolve());
		getDocumentMock.mockReturnValueOnce({ promise: oldLoad.promise, destroy: destroyOld });
		getDocumentMock.mockReturnValueOnce({ promise: Promise.resolve(newDoc), destroy: destroyNew });
		renderMock.mockReturnValueOnce({ promise: firstPage.promise, cancel: cancelMock });
		const view = render(PdfPreview, { data: new ArrayBuffer(8), name: 'old.pdf' });
		await waitFor(() => expect(getDocumentMock).toHaveBeenCalledTimes(1));
		await view.rerender({ data: new ArrayBuffer(16), name: 'new.pdf' });
		await waitFor(() => expect(renderMock).toHaveBeenCalledTimes(1));
		oldLoad.resolve(oldDoc);
		await oldLoad.promise;
		firstPage.resolve();
		await waitFor(() => expect(newDoc.getPage).toHaveBeenCalledTimes(3));
		expect(oldDoc.getPage).not.toHaveBeenCalled();
		expect(destroyOld).toHaveBeenCalledTimes(1);
		expect(destroyNew).not.toHaveBeenCalled();
		expect(screen.queryByText('Could not load preview.')).toBeNull();
		view.unmount();
		expect(destroyNew).toHaveBeenCalledTimes(1);
	});

	it('destroys a completed document when another file replaces it', async () => {
		const view = render(PdfPreview, { data: new ArrayBuffer(8), name: 'old.pdf' });
		await waitFor(() => expect(cleanupMock).toHaveBeenCalledTimes(3));
		const destroyNew = vi.fn(() => Promise.resolve());
		getDocumentMock.mockReturnValueOnce({
			promise: Promise.resolve(pdfWithPages(1)),
			destroy: destroyNew
		});
		await view.rerender({ data: new ArrayBuffer(16), name: 'new.pdf' });
		await waitFor(() => expect(cleanupMock).toHaveBeenCalledTimes(4));
		expect(destroyMock).toHaveBeenCalledTimes(1);
		expect(destroyNew).not.toHaveBeenCalled();
		view.unmount();
		expect(destroyMock).toHaveBeenCalledTimes(1);
		expect(destroyNew).toHaveBeenCalledTimes(1);
	});

	it('destroys a pending load on unmount and handles its rejection', async () => {
		const load = Promise.withResolvers<ReturnType<typeof pdfWithPages>>();
		getDocumentMock.mockReturnValueOnce({ promise: load.promise, destroy: destroyMock });
		const view = render(PdfPreview, { data: new ArrayBuffer(8), name: 'pending.pdf' });
		await waitFor(() => expect(getDocumentMock).toHaveBeenCalledTimes(1));
		view.unmount();
		expect(destroyMock).toHaveBeenCalledTimes(1);
		load.reject(new Error('Loading aborted'));
		await load.promise.catch(() => {});
		expect(renderMock).not.toHaveBeenCalled();
	});

	it('cancels an active render and cleans up its page on unmount', async () => {
		const pending = Promise.withResolvers<void>();
		const cancel = vi.fn(() => pending.reject(new Error('Rendering cancelled')));
		renderMock.mockReturnValueOnce({ promise: pending.promise, cancel });
		const view = render(PdfPreview, { data: new ArrayBuffer(8), name: 'rendering.pdf' });
		await waitFor(() => expect(renderMock).toHaveBeenCalledTimes(1));
		view.unmount();
		expect(cancel).toHaveBeenCalledTimes(1);
		expect(destroyMock).toHaveBeenCalledTimes(1);
		await waitFor(() => expect(cleanupMock).toHaveBeenCalledTimes(1));
	});
});
