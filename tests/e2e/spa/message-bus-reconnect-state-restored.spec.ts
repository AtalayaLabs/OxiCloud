// =============================================================
// SPA · state is RESTORED on reconnect across every tab
// =============================================================
// `message-bus-reconnect.spec.ts` pins the architectural
// property (one reconnect for the browser, not N). This spec
// pins the OTHER half of Gate 5: after the reconnect finishes,
// every open tab's SUBSCRIBE state is replayed and live-event
// fan-out works again. A broken `#subs`-replay path (e.g. a
// regression where the worker's `activeTopics` set never gets
// re-sent upstream post-reconnect) would leave the tabs
// `connected: true` but silent — no incoming events, folder
// view goes stale, user sees nothing updating until a manual
// refresh.
//
// Why not an actual server restart: Playwright's `webServer`
// config launches the backend once at suite start. Killing +
// restarting mid-session is possible via raw process APIs but
// adds a lot of brittle plumbing for a property we can prove
// deterministically through `oxi.reconnectBus()`, which drives
// the SAME `transport.reconnect()` code path (aggressive-reset
// + close + schedule-immediate-retry) a server restart would
// trigger via `#onClose`. The property under test — "subscribes
// are replayed, fan-out works post-reconnect" — is identical in
// both cases.
//
// Shape:
//
//   1. Two tabs of the same user open `/files`. Shared bus engages.
//      Baseline: `active_ws_sessions === 1`.
//   2. Pre-upload: upload file F1 to home. Both tabs see it.
//      (Confirms baseline fan-out — if THIS fails the test
//      isn't about reconnect at all.)
//   3. Trigger `oxi.reconnectBus()` on tab A. Both tabs' bridges
//      cycle `connected → disconnected → connecting → connected`.
//   4. One reconnect for the browser — reassert
//      `active_ws_sessions === 1`.
//   5. Post-reconnect: upload file F2 to home. BOTH tabs must
//      see F2 live, without navigating. This is the state-
//      restoration pin: subscribes were replayed, fan-out
//      works, both tabs are USEFUL — not just `connected`.
//
// Related:
//   * `tests/e2e/spa/message-bus-reconnect.spec.ts` — concurrent
//     reconnect, one-WS property.
//   * `tests/e2e/spa/message-bus-folder-fanout.spec.ts` — the
//     steady-state fan-out property this spec extends.
//   * `docs/plan/message-bus-via-shared-worker.md § Gate 5`.

import type { Page } from '@playwright/test';

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';
import { apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';

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

test.describe('SPA · reconnect restores state across every tab', () => {
	test('post-reconnect, both tabs still receive folder events (subs replayed)', async ({
		browser
	}) => {
		test.setTimeout(90_000);

		const context = await browser.newContext();
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);

		// Baseline: both tabs on shared bus, bus connected.
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
			await expect(tab.getByTestId('appshell-user-menu-btn')).toBeVisible({
				timeout: 15_000
			});
		}

		const baseline = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(baseline.active_ws_sessions).toBe(1);

		// Resolve home-folder id via the scratch-folder trick.
		const scratch = await apiCreateFolder(tabA, `rr-scratch-${Date.now()}`);
		const home = scratch.parent_id;
		if (!home) throw new Error('could not resolve home folder id');

		// ── Pre-upload: F1 — confirms steady-state fan-out ────────
		// If THIS step fails the test is already not about
		// reconnect; the folder-fanout spec would also be red.
		const stamp = Date.now().toString(36);
		const f1 = { ...SAMPLE_FILES.text(), name: `rr-pre-${stamp}.txt` };
		await apiUploadFile(tabA, f1, home);
		for (const tab of [tabA, tabB]) {
			await expect(tab.getByTestId(f1.name)).toBeVisible({ timeout: 10_000 });
		}

		// ── Fire reconnect on tab A ────────────────────────────────
		// Only tab A — tab B shares the SAME worker + SAME WS, so
		// tab A's `request-reconnect` tears the browser's one WS
		// down and both tabs see the state cycle. If a regression
		// made the reconnect per-port, tab B would stay
		// `connected` throughout and `active_ws_sessions` would
		// stay at some non-trivial-but-wrong number.
		await tabA.evaluate(async () => {
			const w = window as unknown as {
				oxi?: { reconnectBus?: () => Promise<unknown> };
			};
			await w.oxi?.reconnectBus?.();
		});

		// Wait for the state cycle on BOTH tabs. The aggressive
		// reset makes recovery near-instant (one ticket round
		// trip), but we poll explicitly so a slow CI doesn't make
		// the test flaky on the "did state change?" question.
		for (const tab of [tabA, tabB]) {
			await expect
				.poll(
					async () => {
						const snapshot = await probeBus(tab);
						return snapshot?.ok === true && snapshot.state === 'connected';
					},
					{ timeout: 20_000, message: 'bus did not re-settle to connected post-reconnect' }
				)
				.toBe(true);
		}

		// One reconnect for the browser (same architectural
		// property the sibling reconnect spec pins — reasserting
		// here so a regression that broke THIS test on fan-out
		// doesn't go unnoticed if it also broke the WS count).
		const after = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(after.active_ws_sessions).toBe(1);

		// ── Post-reconnect: F2 — the state-restoration pin ────────
		// If the worker's `activeTopics` replay path were broken,
		// the server would have NO upstream subscribe for
		// `folder:<home>` after the reconnect, and the F2 upload
		// would publish into a topic nobody is listening to.
		// `scheduleLiveReload` never fires, DOM stays stale,
		// `getByTestId(f2.name)` never resolves → test fails.
		//
		// A broken `#subs` replay in the bridge (worker fine, but
		// the bridge-side cache didn't resend subscribes) would
		// surface the same way.
		//
		// A broken fan-out through the port routing table
		// (binaryByFile / #topics map lost its ports) is already
		// pinned by the folder-fanout + collab-shared specs on
		// the pre-reconnect path, but reasserting on the POST path
		// catches a reconnect-specific regression that didn't
		// touch steady-state routing.
		const f2 = { ...SAMPLE_FILES.text(), name: `rr-post-${stamp}.txt` };
		await apiUploadFile(tabA, f2, home);
		for (const tab of [tabA, tabB]) {
			await expect(tab.getByTestId(f2.name)).toBeVisible({ timeout: 10_000 });
		}

		await context.close();
	});
});
