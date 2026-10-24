// =============================================================
// SPA · two tabs collab the same file via the shared bus
// =============================================================
// The SharedWorker routing's binary-frame fan-out IS the collab
// data plane. When two tabs open the same `.md` file in collab
// mode:
//
//   * Both tabs' bridges fire `subscribe('collab:<fileId>')` →
//     the worker de-dupes (second ref doesn't send a new upstream
//     subscribe) and both tabs receive the ack.
//   * Both tabs' bridges fire `registerBinaryHandler(<fileId>)` →
//     the worker fans every inbound Yjs UPDATE to BOTH ports.
//   * Both tabs can `sendBinary(frame)` through the one WS; the
//     server rebroadcasts; the OTHER tab receives via the fan-out.
//
// If either leg breaks — multi-port subscribe refcount wrong,
// binary dispatch not keyed on fileId, port routing drops frames
// — tab B stops seeing tab A's edits. This spec is the end-to-end
// pin for that whole data path.
//
// Also doubles as the "shared bus stays shared under collab load"
// property: `active_ws_sessions === 1` even with both tabs holding
// a `collab:<fileId>` subscribe. Catches a regression where
// collab somehow spawns per-tab WS.
//
// Related:
//   * `tests/e2e/spa/message-bus-shared-worker.spec.ts` — the
//     static shape property (two tabs on folder listing → one WS).
//   * `tests/e2e/spa/message-bus-reconnect.spec.ts` — reconnect
//     shape property.
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

/** Upload a markdown file and return its server-assigned `id` so we
 *  can navigate directly to `/files/<folder>?file=<id>`. The repo's
 *  `apiUploadFile` helper returns void; local-only extension that
 *  parses the response body for the id. */
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
	// The upload endpoint has returned both shapes across refactors;
	// accept either so a minor DTO adjustment doesn't poison the test.
	if ('id' in parsed) return parsed.id;
	if ('file' in parsed && parsed.file?.id) return parsed.file.id;
	throw new Error(`unexpected upload response shape: ${res.body}`);
}

test.describe('SPA · two tabs collab same file (shared bus data plane)', () => {
	test('tab A types, tab B sees it; active_ws_sessions stays at 1', async ({ browser }) => {
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

		// Fixture: upload a tiny .md file to the HOME folder (resolved
		// via a scratch folder's parent_id — same trick
		// `seedFilesAndFolders` uses in helpers.ts). Keeps the test
		// scoped to the `/files` root rather than navigating into a
		// subfolder (which exercises more infra than this spec is
		// pinning).
		const scratch = await apiCreateFolder(tabA, `collab-scratch-${Date.now()}`);
		const home = scratch.parent_id;
		if (!home) throw new Error('could not resolve home folder id from scratch folder');
		const fileName = `collab-two-tab-${Date.now()}.md`;
		await uploadMarkdownReturningId(
			tabA,
			home,
			fileName,
			'# collab-two-tab-test\n\nBaseline content.\n'
		);

		// Both tabs land on `/files` (home listing) and click the file
		// row. The row has `data-testid={item.name}` (ResourceList).
		// Clicking triggers `openFile`, which pushes `?file=<id>` into
		// the URL — the deep-link effect sees the listing is already
		// loaded, finds the FileItem, and opens the viewer. For an
		// .md, FileViewer mounts CollabEditor.
		await Promise.all([tabA.goto('/files'), tabB.goto('/files')]);
		for (const tab of [tabA, tabB]) {
			await expect(tab.getByTestId(fileName)).toBeVisible({ timeout: 15_000 });
			await tab.getByTestId(fileName).click();
		}

		// Both tabs' CodeMirror root mounts. `.cm-content` is the
		// contenteditable div CodeMirror injects — the most
		// foundational "editor is up" signal; it renders before
		// any collab-* decoration land, so a slower-than-expected
		// subscribe-ack doesn't fail the test here.
		await expect(tabA.locator('.cm-content')).toBeVisible({ timeout: 15_000 });
		await expect(tabB.locator('.cm-content')).toBeVisible({ timeout: 15_000 });

		// ── Shared-mode property under collab load ────────────────
		// Both tabs hold a `collab:<fileId>` subscribe + a binary
		// handler. Admin should still report ONE WS — if collab
		// somehow spawned per-tab WS, we'd see 2.
		const dashboard = await adminFetch<{ active_ws_sessions: number }>(
			tabA,
			'/api/admin/dashboard'
		);
		expect(dashboard.active_ws_sessions).toBe(1);

		// ── Fan-out property: tab A types, tab B sees it ──────────
		// This exercises the full binary-dispatch path:
		//
		//   tab A editor →
		//   `sendBinary(KIND_UPDATE || fileId || yjs-update)` →
		//   port → worker → WS →
		//   server broadcasts to all collab subscribers of fileId →
		//   WS → worker.#dispatchBinary →
		//   fan to every port registered for fileId →
		//   tab B's bridge → tab B's binary handler →
		//   collabDoc applies the Yjs update →
		//   CodeMirror re-renders.
		//
		// If ANY leg is broken (routing by fileId, port fan-out,
		// per-tab handler registration), tab B's `.cm-content`
		// doesn't update and this assertion times out.
		const marker = `SyncFromA_${Date.now().toString(36)}`;

		// Click into the editor to focus, then type.
		await tabA.locator('.cm-content').click();
		await tabA.keyboard.type(marker);

		// Tab B's editor eventually contains the marker.
		await expect(tabB.locator('.cm-content')).toContainText(marker, { timeout: 20_000 });

		await context.close();
	});
});
