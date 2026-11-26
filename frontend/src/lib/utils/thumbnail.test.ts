import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';

/**
 * Regression pin: client-side PDF thumbnailing must fetch through
 * `apiFetch`, not hand pdf.js a URL.
 *
 * `sourceToBitmap`'s PDF branch used to call
 * `pdfjsLib.getDocument(source)` with a URL string, which makes pdf.js
 * fetch the document itself from its own worker. Two consequences, and
 * both are what this pins:
 *
 *  - the request escaped `apiFetch`, so it carried no DPoP header and a
 *    401 got no refresh-and-retry — a PDF thumbnail just failed for a
 *    user whose token had expired;
 *  - a non-2xx rejected from inside pdf.js's worker with its own
 *    `UnexpectedResponseException`, which `queueGenerate`'s try/catch
 *    cannot reach. That surfaced as an unhandled page error, and it made
 *    `tests/e2e/spa/themes.spec.ts` fail intermittently — the spec fails
 *    on ANY unhandled error, and whether the rejection landed before
 *    teardown was a race. It cost a bisect across six commits before the
 *    non-determinism showed it was never commit-attributable.
 *
 * The image branch in the same function always fetched via `apiFetch` and
 * checked `response.ok`; this is that asymmetry being pinned shut. A
 * revert to `getDocument(url)` fails the first test here, because
 * `apiFetch` would not be called for a PDF at all.
 */

const apiFetch = vi.fn();
const getDocument = vi.fn();

vi.mock('$lib/api/client', () => ({
	apiFetch: (...args: unknown[]) => apiFetch(...args),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
vi.mock('$lib/utils/assets', () => ({ fetchAsset: vi.fn() }));
vi.mock('$app/paths', () => ({ base: '' }));

describe('PDF thumbnail fetching', () => {
	beforeEach(() => {
		apiFetch.mockReset();
		getDocument.mockReset();
		vi.stubGlobal('window', { location: { origin: 'https://oxi.test' } });
	});

	afterEach(() => {
		vi.unstubAllGlobals();
	});

	it('fetches the PDF through apiFetch rather than letting pdf.js fetch it', async () => {
		// 404 on purpose: the generation must fail HANDLED, and failing
		// before pdf.js is reached is what proves the fetch is ours. If the
		// URL were handed to `getDocument`, `apiFetch` would never be called
		// for a PDF and this expectation would see zero calls.
		apiFetch.mockResolvedValue({ ok: false, status: 404 });

		const { queueGenerate } = await import('./thumbnail');
		// `vitest-setup.ts` fails a test on unexpected console output, and
		// the handled failure warns by design — so the warning is expected
		// here, not suppressed noise.
		const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});

		await expect(
			queueGenerate({ id: 'f1', name: 'Review 001.pdf', mime_type: 'application/pdf' })
		).resolves.toBeUndefined();

		expect(apiFetch).toHaveBeenCalledTimes(1);
		expect(apiFetch).toHaveBeenCalledWith('https://oxi.test/api/files/f1');
		warn.mockRestore();
	});

	it('swallows the failure instead of rejecting, so no unhandled error escapes', async () => {
		// `queueGenerate` is fire-and-forget at every call site, so a
		// rejection here would become an unhandled rejection on the page —
		// exactly the e2e failure mode. It must resolve.
		apiFetch.mockResolvedValue({ ok: false, status: 401 });

		const { queueGenerate } = await import('./thumbnail');
		const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});

		await expect(
			queueGenerate({ id: 'f2', name: 'Expired.pdf', mime_type: 'application/pdf' })
		).resolves.toBeUndefined();

		expect(warn).toHaveBeenCalled();
		warn.mockRestore();
	});
});
