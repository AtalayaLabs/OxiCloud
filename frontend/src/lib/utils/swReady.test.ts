import { describe, expect, it, vi, afterEach } from 'vitest';
import { awaitServiceWorkerReady } from './swReady';

/**
 * Regression pin: the Service Worker wait must TIME OUT.
 *
 * `routes/+layout.svelte` held the splash screen on a bare
 * `await navigator.serviceWorker.ready`. That promise resolves when a
 * worker becomes active and, where registration is REFUSED rather than
 * slow, never settles at all — Firefox private windows, enterprise
 * policy, a CSP block on the script. It does not reject, so the
 * `try/catch` written around it was unreachable: the app's own fail-open
 * comment described a path that could not run, and those users saw a
 * permanently blank splash.
 *
 * The first test is the one that would have caught that: a `.ready` that
 * never settles must still let the caller proceed.
 */
describe('awaitServiceWorkerReady', () => {
	afterEach(() => {
		vi.unstubAllGlobals();
		vi.useRealTimers();
	});

	it('resolves false when .ready never settles, instead of hanging', async () => {
		vi.useFakeTimers();
		// A promise that never settles — not one that rejects. A rejecting
		// stub would pass even against the old bare `await`, so it would
		// not pin the bug.
		vi.stubGlobal('navigator', { serviceWorker: { ready: new Promise<void>(() => {}) } });

		const pending = awaitServiceWorkerReady(5_000);
		await vi.advanceTimersByTimeAsync(5_000);

		await expect(pending).resolves.toBe(false);
	});

	it('resolves true once a worker becomes active', async () => {
		vi.stubGlobal('navigator', { serviceWorker: { ready: Promise.resolve({}) } });

		await expect(awaitServiceWorkerReady(5_000)).resolves.toBe(true);
	});

	it('resolves false where the platform has no serviceWorker at all', async () => {
		vi.stubGlobal('navigator', {});

		await expect(awaitServiceWorkerReady(5_000)).resolves.toBe(false);
	});

	it('treats a rejecting .ready as a failure rather than letting it escape', async () => {
		// Some platforms reject instead of hanging. Either way the caller's
		// boot must not see an exception.
		vi.stubGlobal('navigator', {
			serviceWorker: { ready: Promise.reject(new Error('refused')) }
		});

		await expect(awaitServiceWorkerReady(5_000)).resolves.toBe(false);
	});
});
