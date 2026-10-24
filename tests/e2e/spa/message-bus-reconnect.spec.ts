// =============================================================
// SPA · one reconnect for the browser — the architectural proof
// =============================================================
// The whole SharedWorker migration exists to deliver ONE
// WebSocket per browser instead of N per tab. Shared mode
// trivially satisfies this at the construction site (only the
// worker holds a WS), but the harder guarantee is that it keeps
// holding it through reconnect cycles: when both tabs trigger a
// reconnect concurrently (equivalent to a brief network blip),
// ONE reconnect fires for the whole browser, not N per-tab
// reconnects.
//
// This spec proves that end-to-end:
//
//   1. Two tabs on shared bus. Baseline: `active_ws_sessions === 1`.
//   2. Fire `oxi.reconnectBus()` on BOTH tabs concurrently —
//      exercises the `request-reconnect` port envelope + the
//      worker's `transport.reconnect()` aggressive-reset path.
//   3. Wait for both bridges to recover to `state === 'connected'`.
//   4. Assert `active_ws_sessions === 1` AGAIN. Not 2, not N.
//      ONE reconnect for the browser.
//
// Why reconnect-call instead of `context.setOffline`:
// Playwright's offline flag doesn't close existing WS connections
// in Chromium — it only blocks NEW ones (observed 2026-10-24,
// bridges stayed `connected` throughout the offline window).
// `reconnectBus()` triggers the SAME `transport.reconnect()`
// code path a network drop would eventually trigger via
// `#onClose`, but deterministically and quickly. The
// architectural property (one WS for N `request-reconnect`
// signals) is the same.
//
// If per-tab mode were secretly in play, step 4 would see 2.
// If the worker spawned a reconnect per attached port, step 4
// would see N (one per tab). Either regression collapses the
// whole motivation — this test is the regression-killer.
//
// Related:
//   * `tests/e2e/spa/message-bus-shared-worker.spec.ts` — the
//     static property (two tabs → one WS at steady state).
//   * `docs/plan/message-bus-via-shared-worker.md § Gate 4`.

import type { Page } from '@playwright/test';

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

type BusChainSnapshot = {
	ok: boolean;
	impl: string;
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

test.describe('SPA · message bus — one reconnect for the browser', () => {
	test('concurrent reconnect from both tabs leaves ONE WebSocket, not N', async ({
		browser
	}) => {
		test.setTimeout(60_000);

		const context = await browser.newContext();
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		// Log in via tab A — tab B inherits the session cookie.
		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);
		await expect(tabB.getByTestId('appshell-user-menu-btn')).toBeVisible();

		// Baseline: both tabs on shared bus, bus is connected.
		for (const tab of [tabA, tabB]) {
			await expect
				.poll(
					async () => {
						const snapshot = await probeBus(tab);
						return snapshot?.ok === true && snapshot.state === 'connected'
							? snapshot.impl
							: null;
					},
					{ timeout: 15_000, message: 'bus did not reach connected (baseline)' }
				)
				.toBe('SharedWorkerBridge');
		}

		// Baseline: ONE WS for this browser.
		const baseline = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(baseline.active_ws_sessions).toBe(1);

		// ── Fire reconnect from BOTH tabs concurrently ────────────
		// Each tab's `reconnectBus()` posts a `{kind:
		// 'request-reconnect'}` port envelope. Both envelopes route
		// to the SAME worker (one worker per browser). The worker's
		// `transport.reconnect()` is called twice; the aggressive
		// reset path handles double-invocation by closing the WS
		// idempotently + scheduling an immediate retry.
		//
		// If the worker spawned a NEW WS per request-reconnect
		// envelope, we'd end up with N open WS afterwards. The
		// post-cycle assertion catches that regression.
		await Promise.all([
			tabA.evaluate(async () => {
				const w = window as unknown as {
					oxi?: { reconnectBus?: () => Promise<unknown> };
				};
				await w.oxi?.reconnectBus?.();
			}),
			tabB.evaluate(async () => {
				const w = window as unknown as {
					oxi?: { reconnectBus?: () => Promise<unknown> };
				};
				await w.oxi?.reconnectBus?.();
			})
		]);

		// Wait for both bridges to re-settle to 'connected'. The
		// aggressive reset clears timers + zeros counters, so
		// recovery is near-instant (one ticket round-trip).
		for (const tab of [tabA, tabB]) {
			await expect
				.poll(
					async () => {
						const snapshot = await probeBus(tab);
						return snapshot?.ok === true && snapshot.state === 'connected';
					},
					{ timeout: 20_000, message: 'bus did not re-settle to connected' }
				)
				.toBe(true);
		}

		// ── The architectural assertion ───────────────────────────
		// After both tabs' concurrent reconnect, STILL ONE WS. If
		// the shared worker spawned a reconnect per attached port,
		// we'd see 2. If a per-tab fallback secretly engaged
		// somewhere, we'd see 2. Either is a regression of the
		// whole motivation.
		const recovered = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(recovered.active_ws_sessions).toBe(1);

		await context.close();
	});
});
