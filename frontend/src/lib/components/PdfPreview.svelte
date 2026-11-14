<script lang="ts">
	/**
	 * PDF preview for browsers with no embedded PDF viewer.
	 *
	 * `FileViewer` frames a `blob:` URL where the browser can render a PDF
	 * itself. Chrome on Android cannot — `navigator.pdfViewerEnabled` is
	 * `false` there — and the attempt fails as a broken-document icon plus a
	 * CSP refusal of the empty-origin document its fallback path creates
	 * (issue #780). Relaxing the CSP does not help: `data:` in `frame-src`
	 * only moves the failure to `script-src`, and then to an opaque origin
	 * that may not read our blob.
	 *
	 * So this renders the pages ourselves with the pdf.js the app already
	 * vendors for client-side thumbnails. Same library, same instance — see
	 * `getPdfjsLib`.
	 */
	import { getPdfjsLib } from '$lib/utils/thumbnail';
	import { t } from '$lib/i18n/index.svelte';

	interface Props {
		/** The PDF's bytes. Owned by the caller; this component only reads. */
		data: ArrayBuffer;
		/** File name, for the accessible label on the page list. */
		name: string;
	}
	let { data, name }: Props = $props();

	let host = $state<HTMLDivElement | null>(null);
	let loading = $state(true);
	let failed = $state(false);

	interface PdfRenderTask {
		promise: Promise<void>;
		cancel(): void;
	}
	interface PdfViewport {
		width: number;
		height: number;
	}
	interface PdfPage {
		getViewport(options: { scale: number }): PdfViewport;
		render(options: {
			canvasContext: CanvasRenderingContext2D;
			viewport: PdfViewport;
			transform: number[];
		}): PdfRenderTask;
		cleanup(): void;
	}
	interface PdfLoadingTask {
		promise: Promise<{ numPages: number; getPage(n: number): Promise<PdfPage> }>;
		destroy(): Promise<void>;
	}

	/**
	 * Cap the device-pixel ratio at 2. A phone can report 3+, and at that
	 * ratio a full-width A4 page is ~25 MP of canvas — several of those is
	 * enough to have the tab killed on the devices this code path exists
	 * for. 2 is already past the point of visible difference for text.
	 */
	const MAX_DPR = 2;

	function startRender(bytes: ArrayBuffer, el: HTMLDivElement): () => void {
		let cancelled = false;
		let task: PdfLoadingTask | undefined;
		let rendering: PdfRenderTask | undefined;

		// A loading task owns its document and worker, including while its
		// promise is pending. Effect cleanup runs on both replacement and unmount.
		function dispose(): void {
			if (cancelled) return;
			cancelled = true;
			rendering?.cancel();
			void task?.destroy().catch(() => {});
		}

		async function render(): Promise<void> {
			loading = true;
			failed = false;
			el.replaceChildren();

			try {
				const pdfjs = await getPdfjsLib();
				if (cancelled) return;

				// `isEvalSupported: false` keeps pdf.js off `eval()` for its font
				// fast paths, so the app's CSP needs no `unsafe-eval`. A copy of
				// the buffer because pdf.js transfers ownership of what it is
				// given, and the caller keeps using `data` (a re-render after a
				// resize, say).
				task = pdfjs.getDocument({
					data: bytes.slice(0),
					isEvalSupported: false
				}) as PdfLoadingTask;
				const doc = await task.promise;
				if (cancelled) return;

				const dpr = Math.min(window.devicePixelRatio || 1, MAX_DPR);

				for (let n = 1; n <= doc.numPages; n++) {
					const page = await doc.getPage(n);
					try {
						if (cancelled) return;

						// Fit the page to the host's width; the viewer pane scrolls
						// vertically. `clientWidth` is read per page so a rotation
						// mid-render still lays the rest out correctly.
						const unscaled = page.getViewport({ scale: 1 });
						const width = el.clientWidth || unscaled.width;
						const viewport = page.getViewport({ scale: width / unscaled.width });

						const canvas = document.createElement('canvas');
						canvas.className = 'pdfp__page';
						canvas.width = Math.floor(viewport.width * dpr);
						canvas.height = Math.floor(viewport.height * dpr);
						// CSS size in layout pixels; the backing store is DPR-scaled.
						canvas.style.width = '100%';
						canvas.style.height = 'auto';

						const ctx = canvas.getContext('2d');
						if (!ctx) throw new Error('canvas 2d context unavailable');

						// Append before rendering so pages appear one by one on a long
						// document rather than all at the end.
						el.appendChild(canvas);
						rendering = page.render({
							canvasContext: ctx,
							viewport,
							transform: [dpr, 0, 0, dpr, 0, 0]
						});
						await rendering.promise;
						if (cancelled) return;

						// pdf.js keeps per-page structures alive until released; on a
						// 200-page document that is the difference between a preview
						// and an out-of-memory tab.
						loading = false;
					} finally {
						rendering = undefined;
						page.cleanup();
					}
				}
				loading = false;
			} catch {
				// Covers a corrupt file, a pdf.js that failed to load (offline,
				// blocked, a vendor file missing from the build) and a canvas we
				// could not get a context for. The caller shows nothing else, so
				// this component owns the message.
				if (!cancelled) {
					failed = true;
					loading = false;
					dispose();
				}
			}
		}

		void render();
		return dispose;
	}

	$effect(() => {
		// Depend on `data` and `host`: a new document, or the host element
		// arriving on mount, both start a render and invalidate the old one.
		const bytes = data;
		const el = host;
		if (!el) return;
		return startRender(bytes, el);
	});
</script>

<div class="pdfp">
	{#if failed}
		<p class="pdfp__status">{t('files.preview_failed', 'Could not load preview.')}</p>
	{:else if loading}
		<p class="pdfp__status">{t('common.loading', 'Loading…')}</p>
	{/if}
	<!--
		Not `aria-hidden`: the canvases carry no text, so the label is all a
		screen reader gets. The download action in the viewer's toolbar is the
		accessible path to the actual content, as it was with the iframe.
	-->
	<div
		class="pdfp__pages"
		bind:this={host}
		role="document"
		aria-label={name}
		data-testid="pdf-preview-pages"
	></div>
</div>

<style>
	.pdfp {
		width: 100%;
		height: 100%;
		overflow: hidden auto;
		background: var(--color-bg-muted);
	}

	.pdfp__status {
		padding: var(--space-6);
		color: var(--color-text-secondary);
		text-align: center;
	}

	.pdfp__pages {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-3);
	}

	/* `:global` because the canvases are created in script, so the scoped
	   class on them is not in this component's markup for the compiler to
	   see. */
	.pdfp__pages :global(.pdfp__page) {
		display: block;
		max-width: 100%;
		box-shadow: 0 1px 4px var(--color-shadow);
		background: var(--color-bg-surface);
	}
</style>
