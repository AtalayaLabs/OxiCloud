/**
 * Client-side thumbnail generation + upload.
 *
 * Fallback path: when the server returns 404 for a file's thumbnail
 * (the mime type isn't supported server-side, e.g. PDF, or the async
 * server-side generator hasn't caught up yet), the client can generate
 * the three canonical sizes from the file itself and PUT them back so
 * subsequent viewers hit the server thumbnail.
 *
 * Ported from the legacy vanilla-JS `static/js/features/thumbnail.js`
 * (retired in commit 54639d46). Same shape, same behaviour:
 *
 *   * SUPPORTED_MIME_TYPE — image/*, application/pdf, video/*.
 *   * SIZES               — icon 150×150, preview 300×300, large 900×800.
 *   * FORMAT / QUALITY    — JPEG q=0.8 (matches the server's own encoder).
 *   * MAX_CONCURRENT      — 3 parallel generations, excess queued.
 *
 * pdf.js lives at `/vendors/pdf.min.mjs` (+ worker) — dynamically
 * imported on first PDF encounter so image/video-only sessions never
 * pay the ~1 MB pdf.js download.
 */
import { base } from '$app/paths';
import { withBase } from '$lib/api/client';
import type { FileItem } from '$lib/api/types';
import { getCsrfHeaders } from '$lib/api/csrf';

/**
 * The subset of `FileItem` this module actually reads. Keeping
 * `FileItem` as the canonical shape means the files browser passes
 * its DTO through verbatim; ResourceList (which only carries
 * `ResourceEntry`) builds an object with just these three fields and
 * satisfies the same structural type — no widening cast, no parallel
 * named type to maintain.
 */
type ThumbnailFile = Pick<FileItem, 'id' | 'name' | 'mime_type'>;

const PDFJS_LIB_URL = `${base}/vendors/pdf.min.mjs`;
const PDFJS_WORKER_URL = `${base}/vendors/pdf.worker.min.mjs`;

// Anything that ships a runtime API surface too broad to type here without
// vendoring `@types/pdfjs-dist`; the two methods we call (`getDocument`,
// worker options) are stable across pdf.js 4.x.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let pdfjsLibPromise: Promise<any> | null = null;
let pdfWorkerWarmed = false;

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function getPdfjsLib(): Promise<any> {
	if (!pdfjsLibPromise) {
		pdfjsLibPromise = import(/* @vite-ignore */ PDFJS_LIB_URL)
			.then((lib) => {
				lib.GlobalWorkerOptions.workerSrc = PDFJS_WORKER_URL;
				return lib;
			})
			.catch((err) => {
				pdfjsLibPromise = null; // allow retry on a later sighting
				throw err;
			});
	}
	return pdfjsLibPromise;
}

const SUPPORTED_MIME_TYPE = [/^image\//, /^application\/pdf$/, /^video\//];

const SIZES = {
	icon: { width: 150, height: 150 },
	preview: { width: 300, height: 300 },
	large: { width: 900, height: 800 }
} as const;

const FORMAT = 'image/jpeg';
const QUALITY = 0.8;

const MAX_CONCURRENT = 3;
let activeGenerates = 0;
const generateQueue: Array<(release: void) => void> = [];

interface Size {
	width: number;
	height: number;
}

function computeSize(
	srcWidth: number,
	srcHeight: number,
	targetWidth: number,
	targetHeight: number
): Size {
	const srcRatio = srcWidth / srcHeight;
	const targetRatio = targetWidth / targetHeight;
	if (srcRatio > targetRatio) {
		return { width: targetWidth, height: Math.round(targetWidth / srcRatio) };
	}
	return { width: Math.round(targetHeight * srcRatio), height: targetHeight };
}

async function bitmapToBlob(
	bitmap: ImageBitmap,
	targetWidth: number,
	targetHeight: number,
	options: ImageEncodeOptions
): Promise<Blob> {
	const { width, height } = computeSize(bitmap.width, bitmap.height, targetWidth, targetHeight);
	const canvas = new OffscreenCanvas(width, height);
	canvas.getContext('2d')?.drawImage(bitmap, 0, 0, width, height);
	return canvas.convertToBlob(options);
}

function blobToDataUrl(blob: Blob): Promise<string> {
	return new Promise((resolve, reject) => {
		const reader = new FileReader();
		reader.onload = () => resolve(reader.result as string);
		reader.onerror = reject;
		reader.readAsDataURL(blob);
	});
}

async function sourceToBitmap(file: ThumbnailFile, source: string): Promise<ImageBitmap> {
	const mime = file.mime_type ?? '';
	if (mime.startsWith('image/')) {
		const response = await fetch(source);
		if (!response.ok) throw new Error(`failed to fetch: ${response.status}`);
		const blob = await response.blob();
		return createImageBitmap(blob);
	}
	if (mime === 'application/pdf') {
		const pdfjsLib = await getPdfjsLib();
		const pdf = await pdfjsLib.getDocument(source).promise;
		const page = await pdf.getPage(1);
		const viewport = page.getViewport({ scale: 1 });
		const canvas = document.createElement('canvas');
		canvas.width = viewport.width;
		canvas.height = viewport.height;
		await page.render({ canvasContext: canvas.getContext('2d'), viewport }).promise;
		return createImageBitmap(canvas);
	}
	if (mime.startsWith('video/')) {
		return new Promise<ImageBitmap>((resolve, reject) => {
			const video = document.createElement('video');
			video.src = source;
			video.muted = true;
			video.preload = 'metadata';
			video.onloadedmetadata = () => {
				// Snapshot at 1/3 duration — skips titles/logos, still in the
				// meat of the content for most videos.
				video.currentTime = video.duration / 3;
			};
			video.onseeked = async () => {
				const bitmap = await createImageBitmap(video);
				video.pause();
				video.removeAttribute('src'); // close the pending HTTP body
				video.load();
				resolve(bitmap);
			};
			video.onerror = reject;
		});
	}
	throw new Error(`unsupported mime type: ${mime} for file ${file.name}`);
}

async function generate(
	file: ThumbnailFile,
	onIconGenerated?: (dataUrl: string) => void,
	onPreviewGenerated?: (dataUrl: string) => void
): Promise<void> {
	const source = `${window.location.origin}${withBase(`/api/files/${file.id}`)}`;
	const bitmap = await sourceToBitmap(file, source);

	const [iconBlob, previewBlob, largeBlob] = await Promise.all(
		Object.values(SIZES).map(({ width, height }) =>
			bitmapToBlob(bitmap, width, height, { type: FORMAT, quality: QUALITY })
		)
	);

	if (onIconGenerated) onIconGenerated(await blobToDataUrl(iconBlob));
	if (onPreviewGenerated) onPreviewGenerated(await blobToDataUrl(previewBlob));

	await Promise.all(
		(
			[
				['icon', iconBlob],
				['preview', previewBlob],
				['large', largeBlob]
			] as const
		).map(([size, blob]) =>
			fetch(`${window.location.origin}${withBase(`/api/files/${file.id}/thumbnail/${size}`)}`, {
				method: 'PUT',
				headers: { ...getCsrfHeaders(), 'Content-Type': FORMAT },
				body: blob,
				credentials: 'same-origin'
			})
		)
	);
}

/**
 * True when this file's mime type is one the client-side generator can
 * handle. Callers use this to decide whether to install the fallback
 * `onerror` handler on the `<img>` in the first place.
 */
export function canThumbnailClientSide(file: ThumbnailFile): boolean {
	const mime = file.mime_type ?? '';
	return SUPPORTED_MIME_TYPE.some((re) => re.test(mime));
}

/**
 * Fire-and-forget warm-up of the pdf.js stack (module + worker script).
 *
 * Call the moment a PDF file appears in a listing so the ~1.3 MB library
 * downloads in the background while the user is still looking at the
 * list — instead of stalling the first thumbnail render on it.
 * Idempotent; only folders that actually contain PDFs pay the download.
 */
export function preloadPdf(): void {
	getPdfjsLib().catch(() => {
		/* transient failure — first real use retries */
	});
	if (pdfWorkerWarmed) return;
	pdfWorkerWarmed = true;
	fetch(PDFJS_WORKER_URL)
		.then((r) => (r.ok ? r.blob() : Promise.reject(new Error(`HTTP ${r.status}`))))
		.catch(() => {
			pdfWorkerWarmed = false; // allow retry on a later sighting
		});
}

/**
 * Concurrency-limited wrapper around `generate`. At most `MAX_CONCURRENT`
 * generations run simultaneously; excess calls await a released slot.
 *
 * The optional callbacks receive the icon / preview data URLs the moment
 * they encode locally — callers use them to paint the fallback
 * immediately, before the server round-trip completes.
 */
export async function queueGenerate(
	file: ThumbnailFile,
	onIconGenerated?: (dataUrl: string) => void,
	onPreviewGenerated?: (dataUrl: string) => void
): Promise<void> {
	if (activeGenerates >= MAX_CONCURRENT) {
		await new Promise<void>((resolve) => generateQueue.push(resolve));
	}
	activeGenerates++;
	try {
		await generate(file, onIconGenerated, onPreviewGenerated);
	} catch (err) {
		if (err instanceof Error) {
			console.warn(`thumbnail generation failed for ${file.name}:`, err.message);
		} else {
			console.warn(`thumbnail generation failed for ${file.name}:`, err);
		}
	} finally {
		activeGenerates--;
		const next = generateQueue.shift();
		if (next) next();
	}
}
