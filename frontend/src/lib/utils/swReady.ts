/** How long to wait for the Service Worker to become active before giving
 * up and rendering anyway.
 *
 * Generous, because the wait only ever costs anything on a first-ever
 * visit — afterwards the worker is already installed and `.ready`
 * resolves in about a millisecond. And timing out early is cheap: the
 * caller reloads once when it finds the page uncontrolled, which brings a
 * slow-but-working registration under control. Timing out LATE is the
 * expensive direction, because the splash is still up. */
const SW_READY_TIMEOUT_MS = 5_000;

/**
 * Await Service Worker control, with a timeout, never rejecting.
 *
 * Returns `true` if a worker became active, `false` if it did not in time
 * or the platform refused.
 *
 * **The timeout is the whole point.** `navigator.serviceWorker.ready` is a
 * promise that resolves when a worker becomes active — and where
 * registration is REFUSED rather than merely slow, it never settles at
 * all. It does not reject, so a `try/catch` around it is unreachable.
 * Awaiting it bare therefore hangs forever in:
 *
 *  - Firefox private windows, which expose `navigator.serviceWorker` but
 *    refuse registration;
 *  - browsers under an enterprise policy that disables workers;
 *  - any context where the script response fails CSP.
 *
 * The caller holds the splash screen until this resolves, so "never
 * settles" means a permanently blank app for those users — with a
 * fail-open path written right beside it that could never run. This makes
 * that intent true: on timeout the app renders, and the worst case is the
 * one the DPoP design already tolerates, unsigned browser-direct requests
 * on a bound session.
 */
export async function awaitServiceWorkerReady(
	timeoutMs: number = SW_READY_TIMEOUT_MS
): Promise<boolean> {
	if (typeof navigator === 'undefined' || !('serviceWorker' in navigator)) return false;

	let timer: ReturnType<typeof setTimeout> | undefined;
	try {
		return await Promise.race([
			navigator.serviceWorker.ready.then(() => true),
			new Promise<boolean>((resolve) => {
				timer = setTimeout(() => resolve(false), timeoutMs);
			})
		]);
	} catch {
		// `.ready` can still reject on some platforms; treat it the same as
		// a timeout rather than letting it escape into the caller's boot.
		return false;
	} finally {
		// Cleared on the winning path too: an uncleared timer keeps the
		// event loop busy, which matters in tests with fake timers and on
		// pages that never unload.
		if (timer !== undefined) clearTimeout(timer);
	}
}
