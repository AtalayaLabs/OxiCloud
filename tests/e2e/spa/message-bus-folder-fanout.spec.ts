// =============================================================
// SPA · folder-topic file_created event fans out to both tabs
// =============================================================
// Companion to `message-bus-collab-shared.spec.ts` — that one
// pins Yjs binary fan-out (CRDT frames) via the per-fileId port
// routing table. THIS one pins the OTHER fan-out path: regular
// `event` envelopes on a folder topic. Different code path in
// the worker (`#fanoutEvent` vs `#fanoutBinary`), different
// subscribe entry (`#topics` map vs `#binaryByFile` map), so
// a routing regression could break one without touching the
// other.
//
// Shape:
//
//   1. Two tabs of the same user open the files listing. Shared
//      bus engages for both.
//   2. Baseline: `active_ws_sessions === 1` — proves shared mode
//      is actually live, not a per-tab test pretending to be one.
//   3. A THIRD "paired" caller (same session, API-side upload
//      via `apiUploadFile`) adds a file to the home folder.
//      The server publishes `file_created` on
//      `folder:<home>` → broadcast WS → worker fan-out →
//      BOTH tabs' bridges → BOTH tabs' `useFolderTopic` → both
//      tabs' `scheduleLiveReload` → both tabs' DOM shows the
//      row.
//   4. Assert both tabs' DOMs see the file. The upload DOES NOT
//      navigate either tab — the row MUST appear via live
//      subscribe fan-out, not a tab-side refetch triggered by
//      navigation.
//
// Why "paired user" here is the same user via API: Playwright
// shares no cross-user session fixture in this suite, and the
// routing property under test is "fan-out from a folder event
// to every subscribed port". The identity of the upload-side
// caller doesn't change which path the server traverses — the
// event fires the same way whether tab A uploaded, user B
// uploaded via WebDAV, or a sync-client pushed. The fan-out
// side is what we're pinning.
//
// Related:
//   * `tests/e2e/spa/message-bus-collab-shared.spec.ts` — the
//     Yjs binary fan-out property; different map in the worker.
//   * `docs/plan/message-bus-via-shared-worker.md § Gate 4`
//     final line item.

import type { Page } from '@playwright/test';

import {
	test,
	expect,
	uiLogin,
	TEST_ADMIN
} from './coverage-helpers';
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

test.describe('SPA · folder-topic fan-out across both tabs under shared mode', () => {
	test('file_created event reaches BOTH tabs via the worker fan-out', async ({ browser }) => {
		test.setTimeout(90_000);

		const context = await browser.newContext();
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);

		// Both tabs reach the shared bus. If either lands on per-
		// tab (fallback) the "one WS per browser" baseline below
		// would see 2, so this test would already fail — but
		// pinning the impl up front gives a clearer error line.
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

		// Baseline: one WS for the browser, same subscribe shared
		// across both tabs. If the worker double-subscribed, server
		// side `active_ws_sessions` would still be 1 (one WS), but
		// each tab would own a different sub id — the fan-out path
		// stays correct either way, so this baseline is about the
		// transport layer only.
		const baseline = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(baseline.active_ws_sessions).toBe(1);

		// Both tabs need to be on `/files` AND subscribed to the
		// home-folder topic. The `useFolderTopic` composable mounts
		// on `+page.svelte`, so landing on `/files` is sufficient.
		// Wait for the files surface to be ready in both tabs.
		for (const tab of [tabA, tabB]) {
			await expect(tab.getByTestId('appshell-user-menu-btn')).toBeVisible({
				timeout: 15_000
			});
		}

		// Upload a new file to home. `apiUploadFile` needs the
		// home-folder id — we discover it via the
		// `apiCreateFolder(name).parent_id` trick (the created
		// folder's parent IS the home folder).
		const scratch = await apiCreateFolder(tabA, `fanout-scratch-${Date.now()}`);
		const home = scratch.parent_id;
		if (!home) throw new Error('could not resolve home folder id');

		const stamp = Date.now().toString(36);
		const dropped = {
			...SAMPLE_FILES.text(),
			name: `fanout-${stamp}.txt`
		};
		await apiUploadFile(tabA, dropped, home);

		// The upload fires `file_created` on `folder:<home>`. Both
		// tabs' bridges should see the event, both tabs'
		// `scheduleLiveReload` should fire (100 ms debounce), both
		// tabs' DOM should show the row.
		//
		// Timeout generous enough for: event propagation (one WS
		// RTT) + 100 ms coalescer + the refetch RTT + Svelte paint.
		// On a loaded CI runner this can push past 2 s; 10 s is
		// comfortable without masking a real regression.
		for (const tab of [tabA, tabB]) {
			await expect(tab.getByTestId(dropped.name)).toBeVisible({ timeout: 10_000 });
		}

		// Reconfirm the one-WS property after the fan-out cycle.
		// A regression that silently opened a second WS per tab
		// in response to an event would surface here.
		const after = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(after.active_ws_sessions).toBe(1);

		await context.close();
	});
});
