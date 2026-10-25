// =============================================================
// SPA · SharedWorker fallback path — forced per-tab mode
// =============================================================
// Pins the mandatory fallback: when a browser can't run shared
// mode (Safari < 16.4, enterprise CSP SharedWorkerAllowed=false,
// iOS PWA quirks, handshake timeout), the SPA silently falls
// back to the per-tab client. Zero events lost, zero
// user-visible breakage.
//
// We simulate "SharedWorker API absent" via `addInitScript` that
// deletes `window.SharedWorker` BEFORE any page script runs. The
// launch flag `--disable-features=SharedWorkerAPI` works on
// stable Google Chrome (verified in-the-wild 2026-10-24) but NOT
// reliably on Playwright's bundled Chromium — observed on
// 2026-10-24: three tests failed because `typeof SharedWorker ===
// 'function'` under the launch flag. `addInitScript` is the
// reliable per-test lever recommended in the plan doc.
//
// Properties this file pins:
//   1. `typeof SharedWorker === 'undefined'` under the launch flag.
//   2. `window.messageBus` is a `PerTabMessageBusClient`
//      (via `oxi.checkBusChain()`.impl).
//   3. Two tabs → TWO active WS sessions server-side (per-tab
//      mode is one WS per tab, which is what we're falling back
//      from the shared bus to).
//
// Related:
//   * `docs/plan/message-bus-via-shared-worker.md § Gate 4`.
//   * `tests/e2e/spa/message-bus-shared-worker.spec.ts` — the
//     inverse property (shared mode: two tabs → one WS).

import type { BrowserContext, Page } from '@playwright/test';

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

type BusChainSnapshot = {
	ok: boolean;
	impl: 'SharedWorkerBridge' | 'PerTabMessageBusClient' | string;
	state: 'idle' | 'connecting' | 'connected' | 'disconnected' | 'unavailable';
	workerVersion: string | null;
	latencyMs: number | null;
};

async function probeBus(tab: Page): Promise<BusChainSnapshot | null> {
	return tab.evaluate(async () => {
		const w = window as unknown as {
			oxi?: { checkBusChain?: () => Promise<unknown> };
		};
		if (!w.oxi?.checkBusChain) return null;
		return (await w.oxi.checkBusChain()) as BusChainSnapshot;
	});
}

async function adminFetch<T>(tab: Page, url: string): Promise<T> {
	const result = await tab.evaluate<{ ok: boolean; status: number; body: string }, string>(
		async (requestUrl) => {
			if ('serviceWorker' in navigator && !navigator.serviceWorker.controller) {
				await Promise.race([
					(async () => {
						await navigator.serviceWorker.ready;
						if (!navigator.serviceWorker.controller) {
							await new Promise<void>((resolve) =>
								navigator.serviceWorker.addEventListener('controllerchange', () => resolve(), {
									once: true
								})
							);
						}
					})(),
					new Promise<void>((resolve) => setTimeout(resolve, 10_000))
				]);
			}
			const csrf = document.cookie.match(/(?:^|; )oxicloud_csrf=([^;]+)/)?.[1] ?? '';
			const headers: Record<string, string> = csrf ? { 'x-csrf-token': csrf } : {};
			const res = await fetch(requestUrl, {
				method: 'GET',
				credentials: 'same-origin',
				headers
			});
			return { ok: res.ok, status: res.status, body: await res.text() };
		},
		url
	);
	if (!result.ok) {
		throw new Error(`admin fetch ${url} failed: HTTP ${result.status} — ${result.body}`);
	}
	return JSON.parse(result.body) as T;
}

/** Install the SharedWorker-absent shim for every page in `context`.
 *  Runs BEFORE any page script via `addInitScript`, so the SPA's
 *  factory sees `typeof SharedWorker === 'undefined'` and takes the
 *  fallback branch. Must be called BEFORE `newPage()` for each tab. */
async function stripSharedWorker(context: BrowserContext): Promise<void> {
	await context.addInitScript(() => {
		// `delete` on the readonly global `SharedWorker` works in
		// Chromium because it's a configurable property; the result
		// is `typeof SharedWorker === 'undefined'`, which the SPA's
		// factory treats as "fallback" (identical to Safari < 16.4).
		try {
			delete (window as unknown as { SharedWorker?: unknown }).SharedWorker;
		} catch {
			// Belt + braces: if the delete fails (unlikely), stub it
			// to throw at construction. The factory catches via its
			// try/catch and lands on per-tab anyway.
			(window as unknown as { SharedWorker?: unknown }).SharedWorker = undefined;
		}
	});
}

test.describe('SPA · SharedWorker fallback → per-tab mode', () => {
	test('typeof SharedWorker is undefined under the init-script', async ({ browser }) => {
		// Sanity: proves the init-script actually hid the API.
		// Without this, a passing test for the fallback behaviour might
		// just mean something else disabled shared mode (CSP, server
		// flag, URL override) and we'd lose the specific signal.
		const context = await browser.newContext();
		await stripSharedWorker(context);
		const tab = await context.newPage();
		await tab.goto('/login');
		await tab.waitForLoadState('domcontentloaded');
		const sharedWorkerPresent = await tab.evaluate(() => typeof SharedWorker !== 'undefined');
		expect(sharedWorkerPresent).toBe(false);
		await context.close();
	});

	test('two tabs land on per-tab mode (admin reports active_ws_sessions >= 2)', async ({
		browser
	}) => {
		const context = await browser.newContext();
		await stripSharedWorker(context);
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);
		await expect(tabB.getByTestId('appshell-user-menu-btn')).toBeVisible();

		// Both tabs' per-tab clients must reach connected before probing.
		for (const tab of [tabA, tabB]) {
			await expect
				.poll(
					async () => {
						const snapshot = await probeBus(tab);
						return snapshot?.ok === true && snapshot.state === 'connected'
							? snapshot.impl
							: null;
					},
					{ timeout: 15_000, message: 'bus did not reach connected' }
				)
				.toBe('PerTabMessageBusClient');
		}

		// Per-tab mode = one WS per tab. Two tabs → two active WS.
		// This is the LOWER bound; admin + login + any racing user
		// sessions bump the gauge further. We assert >= 2 which is
		// the shape the fallback contract guarantees without needing
		// a quiescent-fleet setup.
		const dashboard = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(dashboard.active_ws_sessions).toBeGreaterThanOrEqual(2);

		await context.close();
	});
});
