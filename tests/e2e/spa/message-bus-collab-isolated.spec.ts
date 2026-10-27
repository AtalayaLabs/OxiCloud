// =============================================================
// SPA · two tabs collab DIFFERENT files — no cross-talk
// =============================================================
// Companion to `message-bus-collab-shared.spec.ts` (same file,
// fan-out property). This one pins the INVERSE: when two tabs
// open DIFFERENT collab files, the worker's binary-dispatch
// routing by `fileId` must keep their frames isolated.
//
// Tab A edits file 1 → only file 1's binary handler fires (on
// tab A, and on any other tab subscribed to file 1 — zero in
// this test). Tab B's registered binary handler for file 2 must
// NOT fire. Likewise in reverse.
//
// If `WorkerCore.#binaryByFile` were keyed wrong (e.g. accidental
// cross-file union), or if the bridge's binary dispatch didn't
// gate on `fileId`, tab B would silently receive tab A's Yjs
// UPDATE for file 1 and feed it to file 2's Y.Doc — a Yjs
// client would reject the alien update ID but the test would
// show stale content ("No cross-talk" fails at the `.cm-content`
// assertion).
//
// Shape of the test:
//
//   1. Two tabs on shared bus (baseline check).
//   2. Upload TWO distinct `.md` files to the home folder.
//   3. Tab A opens file 1; tab B opens file 2.
//   4. Admin still reports `active_ws_sessions === 1` — the
//      shared bus accommodates both subscribes on one WS.
//   5. Tab A types a unique marker in file 1. Tab B's
//      `.cm-content` (file 2's view) MUST NOT contain that
//      marker. Tab A's view DOES contain it (positive control).
//   6. Tab B types a different marker in file 2. Tab A's
//      `.cm-content` MUST NOT contain it. Tab B does.
//
// Related:
//   * `tests/e2e/spa/message-bus-collab-shared.spec.ts` — the
//     fan-out property for the SAME file.
//   * `docs/plan/message-bus-via-shared-worker.md § Gate 4`.

import type { Page } from '@playwright/test';

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';
import { apiCreateFolder } from './helpers';

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

/** Upload a markdown file and return its server-assigned `id`. */
async function uploadMarkdownReturningId(
	page: Page,
	folderId: string,
	name: string,
	body: string
): Promise<string> {
	const b64 = Buffer.from(body).toString('base64');
	const res = await page.evaluate(
		async ({ b64, name, folderId }) => {
			const csrf = document.cookie.match(/(?:^|; )oxicloud_csrf=([^;]+)/)?.[1] ?? '';
			const binary = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
			const blob = new Blob([binary], { type: 'text/markdown' });
			const form = new FormData();
			form.set('folder_id', folderId);
			form.set('file', blob, name);
			const r = await fetch('/api/files/upload', {
				method: 'POST',
				credentials: 'same-origin',
				headers: csrf ? { 'x-csrf-token': csrf } : {},
				body: form
			});
			return { ok: r.ok, status: r.status, body: await r.text() };
		},
		{ b64, name, folderId }
	);
	if (!res.ok) throw new Error(`upload failed: ${res.status} ${res.body}`);
	const parsed = JSON.parse(res.body) as { id: string } | { file: { id: string } };
	if ('id' in parsed) return parsed.id;
	if ('file' in parsed && parsed.file?.id) return parsed.file.id;
	throw new Error(`unexpected upload response shape: ${res.body}`);
}

test.describe('SPA · two tabs collab DIFFERENT files — no cross-talk', () => {
	test('A types in file1 doesn\'t leak into file2 (fileId port-routing isolation)', async ({
		browser
	}) => {
		test.setTimeout(90_000);

		const context = await browser.newContext();
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);

		// Baseline shared-mode liveness.
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

		// Fixture: upload TWO distinct .md files to home. Reuse the
		// scratch-folder-for-home-id trick.
		const scratch = await apiCreateFolder(tabA, `isolated-scratch-${Date.now()}`);
		const home = scratch.parent_id;
		if (!home) throw new Error('could not resolve home folder id from scratch folder');
		const stamp = Date.now();
		const fileA = `isolated-A-${stamp}.md`;
		const fileB = `isolated-B-${stamp}.md`;
		await uploadMarkdownReturningId(tabA, home, fileA, '# file A\n\nBaseline A.\n');
		await uploadMarkdownReturningId(tabA, home, fileB, '# file B\n\nBaseline B.\n');

		// Each tab opens a DIFFERENT file.
		await Promise.all([tabA.goto('/files'), tabB.goto('/files')]);
		await expect(tabA.getByTestId(fileA)).toBeVisible({ timeout: 15_000 });
		await expect(tabB.getByTestId(fileB)).toBeVisible({ timeout: 15_000 });
		await tabA.getByTestId(fileA).click();
		await tabB.getByTestId(fileB).click();

		// Both tabs' CollabEditor DOM up.
		await expect(tabA.locator('.cm-content')).toBeVisible({ timeout: 15_000 });
		await expect(tabB.locator('.cm-content')).toBeVisible({ timeout: 15_000 });

		// Shared bus still single WS under two distinct collab subs.
		const dashboard = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(dashboard.active_ws_sessions).toBe(1);

		// ── Tab A types in file A ─────────────────────────────────
		// Positive control: tab A's view shows the marker.
		// Negative assertion: tab B's view (file B) must NOT.
		const markerA = `OnlyInFileA_${Date.now().toString(36)}`;
		await tabA.locator('.cm-content').click();
		await tabA.keyboard.type(markerA);
		await expect(tabA.locator('.cm-content')).toContainText(markerA, { timeout: 10_000 });

		// Give the worker + server a generous moment to misroute if
		// they're going to. If `#binaryByFile` were wrong, tab B's
		// handler for file B would receive file A's UPDATE now.
		// The negative assertion polls for ~3 s — if the marker was
		// going to leak, it would arrive well inside that window
		// (local-WS RTT < 10 ms).
		await tabB.waitForTimeout(3_000);
		await expect(tabB.locator('.cm-content')).not.toContainText(markerA);

		// ── Tab B types in file B — mirror image ──────────────────
		const markerB = `OnlyInFileB_${Date.now().toString(36)}`;
		await tabB.locator('.cm-content').click();
		await tabB.keyboard.type(markerB);
		await expect(tabB.locator('.cm-content')).toContainText(markerB, { timeout: 10_000 });

		await tabA.waitForTimeout(3_000);
		await expect(tabA.locator('.cm-content')).not.toContainText(markerB);

		await context.close();
	});
});
