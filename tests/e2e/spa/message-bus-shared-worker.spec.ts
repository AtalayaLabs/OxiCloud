// =============================================================
// SPA · SharedWorker-backed message bus — the central property
// =============================================================
// The whole SharedWorker migration exists to deliver ONE
// `/api/rt/ws` per BROWSER instead of N per tab. This spec pins
// that property end-to-end against a real server:
//
//   * Open two Pages in the SAME BrowserContext (shared cookies +
//     storage → same SharedWorker instance).
//   * Both load `/files` → both subscribe their folder topic via
//     `useFolderTopic` and trigger the bridge's connect flow.
//   * Both bridges talk to ONE worker which holds ONE WS.
//   * Admin dashboard reports `active_ws_sessions === 1`, not 2.
//
// Why Playwright: Vitest can't exercise SharedWorker (jsdom has
// no SharedWorker implementation), Hurl can't open browsers, and
// the property only exists cross-tab. Only a real browser with
// two Pages in one Context proves the design works.
//
// Lives in its own file so a per-tab regression in the broader
// suites can't poison this one.

import type { Page } from '@playwright/test';

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

/** Shape of `window.oxi.checkBusChain()`'s resolved snapshot — kept
 *  inline here so the spec doesn't depend on the SPA's own type
 *  declaration. Mirrors `docs/architecture/ui-diagnostics.md § Message
 *  bus — probe + state`. */
type BusChainSnapshot = {
	ok: boolean;
	impl: 'SharedWorkerBridge' | 'PerTabMessageBusClient' | string;
	state: 'idle' | 'connecting' | 'connected' | 'disconnected' | 'unavailable';
	workerVersion: string | null;
	latencyMs: number | null;
};

/** Call `window.oxi.checkBusChain()` in the page context. Returns
 *  `null` if the helper isn't on the window yet (hooks.client.ts
 *  hasn't finished boot). */
async function probeBus(tab: Page): Promise<BusChainSnapshot | null> {
	return tab.evaluate(async () => {
		const w = window as unknown as {
			oxi?: { checkBusChain?: () => Promise<unknown> };
		};
		if (!w.oxi?.checkBusChain) return null;
		return (await w.oxi.checkBusChain()) as BusChainSnapshot;
	});
}

/** Hit an admin GET endpoint through the browser's own fetch so the
 *  DPoP service worker signs the request. Returns the parsed JSON
 *  body or throws if the response is non-2xx. Playwright's
 *  `page.request.get` wouldn't work here — it runs in Node outside
 *  the SW's scope, so the request lacks DPoP under
 *  `OXICLOUD_DPOP_MODE=required`. */
async function adminFetch<T>(tab: Page, url: string): Promise<T> {
	const result = await tab.evaluate<{ ok: boolean; status: number; body: string }, string>(
		async (requestUrl) => {
			if ('serviceWorker' in navigator && !navigator.serviceWorker.controller) {
				// Wait for the SW to claim this page so DPoP signing is live.
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

test.describe('SPA · SharedWorker-backed message bus', () => {
	test('two tabs share ONE WebSocket (admin reports active_ws_sessions === 1)', async ({
		browser
	}) => {
		// Same context → shared cookies, IndexedDB (DPoP keypair),
		// SharedWorker, BroadcastChannel. `browser.newContext()` for
		// each tab would isolate them and defeat the test premise.
		const context = await browser.newContext();
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		// Log in via tab A — tab B inherits the session cookie.
		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);
		await expect(tabB.getByTestId('appshell-user-menu-btn')).toBeVisible();

		// Both tabs' bridges must reach `state === 'connected'` before
		// we probe the admin gauge — otherwise `active_ws_sessions`
		// might read as 0 (bus still connecting) and fail misleadingly.
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
				.toBe('SharedWorkerBridge');
		}

		// Both tabs are on the shared bus. The admin dashboard counts
		// server-side WS sessions. Expect ONE for this browser's user,
		// not two. Reading from tab A — same request-signing path as
		// any other admin UI probe (DPoP via SW).
		const dashboard = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(dashboard.active_ws_sessions).toBe(1);

		await context.close();
	});
});
