// =============================================================
// SPA · collab write permission is asked, never inherited
// =============================================================
// THE bug (Ed, 2026-11-14): a collab editor opened read-only for a
// user who owned the drive. Renaming the file worked, creating files
// worked, a different file edited fine — and that one file stayed
// read-only across reloads, with nothing logged anywhere. Only
// terminating the SharedWorker from `chrome://inspect#workers`
// cleared it.
//
// Cause: write permission came off the `rt.subscribe` ack, and in
// shared-worker mode that ack is computed ONCE per topic, cached by
// the worker, and replayed verbatim to every later subscriber
// (`worker-core.ts::#handleSubscribe`). So the SECOND subscriber was
// told what the FIRST was allowed to do. Nothing re-asked while any
// port still held the topic — and a closed tab's port lingers up to
// ~75 s before the liveness sweep reaps it, which is why a reload
// changed nothing while killing the worker fixed it.
//
// Two properties pinned here, both in a browser because both are
// about the worker that only exists in one:
//
//   1. a second tab opening the same file gets ITS OWN permission,
//      not the first tab's;
//   2. a session change (logout → login as someone else in the same
//      window) does not inherit the previous identity's permission.
//
// THE SHARED WORKER IS THE POINT. This file is named
// `message-bus-*` on purpose: `playwright.coverage.config.ts` runs
// those specs ONLY in the shared-worker pass
// (`OXICLOUD_SHARED_WORKER_MODE=1`) and every other spec only in
// per-tab mode. In per-tab mode there is no worker, no cross-tab
// cache, and this spec would pass against the bug — so each test
// asserts `impl === 'SharedWorkerBridge'` before testing anything.
//
// Both tabs live in ONE browser context: that is what makes them
// share a worker. Two contexts would each get their own and, again,
// pass against the bug.
//
// Server-side counterpart: `tests/api/rt_bus_check.sh` S30 pins
// `rt.collab_capabilities` itself (owner true, viewer false,
// unreadable refused). Unit counterparts: `worker-core.test.ts`
// (late joiner gets no capabilities; `session-changed` clears) and
// `collabDoc.test.ts` (asks for its own, fails closed, re-asks on
// reconnect).

import type { Page } from '@playwright/test';

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';
import { apiCreateFolder } from './helpers';

type BusChainSnapshot = {
	ok: boolean;
	impl: string;
	state: string;
};

function uniq(p: string): string {
	return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/** Password for the throwaway users this spec creates. Matches the one
 *  `helpers.ts::apiAdminCreateUser` uses, so a future switch to that helper
 *  needs no change here. */
const USER_PASSWORD = 'TestPassword1!';

async function probeBus(tab: Page): Promise<BusChainSnapshot | null> {
	return tab.evaluate(async () => {
		const w = window as unknown as { oxi?: { checkBusChain?: () => Promise<unknown> } };
		if (!w.oxi?.checkBusChain) return null;
		return (await w.oxi.checkBusChain()) as BusChainSnapshot;
	});
}

/** Assert this tab is on the shared bus — without it the spec is vacuous. */
async function expectSharedBus(tab: Page): Promise<void> {
	await expect
		.poll(
			async () => {
				const snapshot = await probeBus(tab);
				return snapshot?.ok === true && snapshot.state === 'connected' ? snapshot.impl : null;
			},
			{ timeout: 20_000, message: 'bus did not reach connected on the shared worker' }
		)
		.toBe('SharedWorkerBridge');
}

/** POST JSON from inside the page, so the request carries the tab's own
 *  session (and the service worker's DPoP signature). */
async function postJson(
	tab: Page,
	url: string,
	body: unknown
): Promise<{ ok: boolean; status: number; body: string }> {
	return tab.evaluate(
		async ({ url, payload }) => {
			const csrf = document.cookie.match(/(?:^|; )oxicloud_csrf=([^;]+)/)?.[1] ?? '';
			const res = await fetch(url, {
				method: 'POST',
				credentials: 'same-origin',
				headers: {
					'content-type': 'application/json',
					...(csrf ? { 'x-csrf-token': csrf } : {})
				},
				body: payload
			});
			return { ok: res.ok, status: res.status, body: await res.text() };
		},
		{ url, payload: JSON.stringify(body) }
	);
}

async function uploadMarkdown(
	tab: Page,
	folderId: string,
	name: string,
	body: string
): Promise<string> {
	const b64 = Buffer.from(body).toString('base64');
	const res = await tab.evaluate(
		async ({ b64, name, folderId }) => {
			const csrf = document.cookie.match(/(?:^|; )oxicloud_csrf=([^;]+)/)?.[1] ?? '';
			const binary = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
			const form = new FormData();
			form.set('folder_id', folderId);
			form.set('file', new Blob([binary], { type: 'text/markdown' }), name);
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
	const parsed = JSON.parse(res.body) as { id?: string; file?: { id: string } };
	const id = parsed.id ?? parsed.file?.id;
	if (!id) throw new Error(`unexpected upload response: ${res.body}`);
	return id;
}

/** Create a user and return its id.
 *
 *  Reads the id off the CREATE response (`{ user: { id } }`) rather than
 *  looking it up afterwards: `GET /api/admin/users` answers with a page
 *  envelope — `{ users: [{ user: {…} }], total, limit, offset }` — and the
 *  first version of this helper iterated it as a bare array, which is a
 *  shape assumption the create response makes unnecessary. One round trip,
 *  nothing to drift against.
 *
 *  `helpers.ts::apiAdminCreateUser` does the same POST but returns only the
 *  username, and the grant API needs a subject id. */
async function createUser(
	adminTab: Page,
	username: string
): Promise<{ username: string; id: string }> {
	const res = await postJson(adminTab, '/api/admin/users', {
		username,
		password: USER_PASSWORD,
		email: `${username}@example.test`,
		role: 'user',
		quota_bytes: 1073741824
	});
	if (!res.ok) throw new Error(`create user failed: ${res.status} ${res.body}`);
	const parsed = JSON.parse(res.body) as { user?: { id?: string }; id?: string };
	const id = parsed.user?.id ?? parsed.id;
	if (!id) throw new Error(`unexpected create-user response: ${res.body}`);
	return { username, id };
}

/** Wait for the collab editor to mount, then report whether it is
 *  read-only. `collab-editor-readonly` is the badge `CollabEditor`
 *  renders when `!canWrite || syncState !== 'synced'`. */
async function openEditor(tab: Page, fileId: string): Promise<void> {
	await tab.goto(`/collab/${fileId}`);
	await expect(tab.locator('.cm-content')).toBeVisible({ timeout: 20_000 });
}

test.describe('SPA · collab capabilities are per caller, not per topic', () => {
	test('a second tab gets its own permission, not the first tab’s', async ({ browser }) => {
		test.setTimeout(120_000);

		// ── Owner side: file + a user with Viewer on it ──────────────
		const adminContext = await browser.newContext();
		const adminTab = await adminContext.newPage();
		await uiLogin(adminTab, TEST_ADMIN);

		const scratch = await apiCreateFolder(adminTab, uniq('caps-scratch'));
		const home = scratch.parent_id;
		if (!home) throw new Error('could not resolve home folder id');
		const fileName = `${uniq('caps')}.md`;
		const fileId = await uploadMarkdown(adminTab, home, fileName, '# caps\n\nbaseline\n');

		const { username, id: userId } = await createUser(adminTab, uniq('capsu'));

		// Viewer FIRST — the state tab A will cache.
		const viewerGrant = await postJson(adminTab, '/api/grants', {
			subject: { type: 'user', id: userId },
			resource: { type: 'file', id: fileId },
			role: 'viewer'
		});
		expect(viewerGrant.ok, `viewer grant failed: ${viewerGrant.status} ${viewerGrant.body}`).toBe(
			true
		);

		// ── The user's own session: TWO tabs, ONE worker ─────────────
		const userContext = await browser.newContext();
		const tabA = await userContext.newPage();
		const tabB = await userContext.newPage();
		await uiLogin(tabA, { username, email: `${username}@example.test`, password: USER_PASSWORD });
		await tabB.goto('/files');
		await expectSharedBus(tabA);
		await expectSharedBus(tabB);

		// Tab A opens the file as a Viewer and STAYS OPEN. Holding the
		// topic is what keeps the worker's cache entry alive — close it
		// and the entry is dropped, the next subscribe is a real one,
		// and the bug cannot reproduce. This is the detail that makes
		// the obvious version of this test (open, close, re-open) pass
		// against the bug.
		await openEditor(tabA, fileId);
		await expect(tabA.getByTestId('collab-editor-readonly')).toBeVisible({ timeout: 20_000 });

		// ── The grant changes while tab A holds the topic ────────────
		// An additive Editor grant on the same file: the effective role
		// goes up without needing a role-update endpoint.
		const editorGrant = await postJson(adminTab, '/api/grants', {
			subject: { type: 'user', id: userId },
			resource: { type: 'file', id: fileId },
			role: 'editor'
		});
		expect(editorGrant.ok, `editor grant failed: ${editorGrant.status} ${editorGrant.body}`).toBe(
			true
		);

		// ── Tab B opens the same file ────────────────────────────────
		// Its subscribe is a LATE JOIN on a live topic, so before the
		// fix it was answered from the worker's cache with tab A's
		// `can_write: false` and came up read-only for a user who may
		// now write. After the fix it asks `rt.collab_capabilities`
		// for itself.
		await openEditor(tabB, fileId);
		await expect(tabB.getByTestId('collab-editor-readonly')).toHaveCount(0, { timeout: 20_000 });

		// Tab A is deliberately NOT asserted to flip: it learns on its
		// next reconnect or reopen, which is the documented behaviour
		// (`CollabDoc.#refreshCapabilities` runs on mount and on
		// `onReconnect`). Asserting otherwise would pin a promise the
		// code does not make.

		await userContext.close();
		await adminContext.close();
	});

	// A SMOKE TEST, not the pin for the session-change fix — and the
	// difference matters, so read this before trusting it.
	//
	// `CollabDoc.destroy()` unsubscribes (`collabDoc.ts`), and the worker
	// deletes a topic entry when its last port drops it
	// (`worker-core.ts::#handleUnsubscribe`). Navigating away from the
	// editor before logging out therefore leaves NOTHING cached for the next
	// identity to inherit — so this test would pass with or without the
	// `session-changed` envelope.
	//
	// What it does earn: it proves the clear does not BREAK the happy path.
	// A `#handleSessionChanged` that wiped `#perPort` but left `#topics`
	// would leave the next subscribe answered from a table whose ports are
	// gone — the editor would hang un-acked, and this test would fail.
	//
	// The discriminating coverage for point 3 is in `worker-core.test.ts`
	// ("drops topics, unsubscribes upstream, and re-subscribes fresh" /
	// "stops routing binary frames for the old session" / idempotency) and
	// `shared-worker-bridge.test.ts`. It stays at that layer deliberately:
	// what the envelope uniquely prevents is subscription and EVENT
	// leakage, and the permission half is already closed by stripping
	// `capabilities` from the cache. To watch the event leak in a browser
	// you would need a stale tab still receiving frames after the logout —
	// but `onSessionCleared` also redirects that tab to `/login`, so the
	// editor unmounts and there is nothing left to observe. The worker's
	// topic table is not exposed to the page either (`checkBusChain`
	// reports impl / state / latency only).
	test('a logout then login as the owner lands writable (session-change smoke)', async ({
		browser
	}) => {
		test.setTimeout(120_000);

		// Same fixture: a file the admin owns, with a Viewer grant for
		// a throwaway user.
		const adminContext = await browser.newContext();
		const adminTab = await adminContext.newPage();
		await uiLogin(adminTab, TEST_ADMIN);

		const scratch = await apiCreateFolder(adminTab, uniq('caps2-scratch'));
		const home = scratch.parent_id;
		if (!home) throw new Error('could not resolve home folder id');
		const fileId = await uploadMarkdown(
			adminTab,
			home,
			`${uniq('caps2')}.md`,
			'# caps2\n\nbaseline\n'
		);
		const { username, id: userId } = await createUser(adminTab, uniq('capsu2'));
		const grant = await postJson(adminTab, '/api/grants', {
			subject: { type: 'user', id: userId },
			resource: { type: 'file', id: fileId },
			role: 'viewer'
		});
		expect(grant.ok, `viewer grant failed: ${grant.status} ${grant.body}`).toBe(true);

		// One window. The Viewer opens the file — worker caches the
		// topic for this identity.
		const context = await browser.newContext();
		const tab = await context.newPage();
		await uiLogin(tab, { username, email: `${username}@example.test`, password: USER_PASSWORD });
		await expectSharedBus(tab);
		await openEditor(tab, fileId);
		await expect(tab.getByTestId('collab-editor-readonly')).toBeVisible({ timeout: 20_000 });

		// Log out properly — the `logout` endpoint plus the cross-tab
		// `session-cleared` broadcast is what tells the worker to drop
		// everything. Navigating to /login would not: the worker is
		// keyed by script URL and survives any navigation.
		await tab.goto('/files');
		await tab.getByTestId('appshell-user-menu-btn').click();
		await tab.getByTestId('appshell-user-menu-logout-btn').click();
		await tab.waitForURL('**/login**', { timeout: 20_000 });

		// Same window, different identity — the file's OWNER, who may
		// certainly write it.
		await uiLogin(tab, TEST_ADMIN);
		await expectSharedBus(tab);
		await openEditor(tab, fileId);

		// Writable, because the admin owns the file and asks for itself.
		// NOT a proof that the inheritance was fixed — see the note above
		// the test name for why this cannot reproduce it, and where the
		// real pin lives.
		await expect(tab.getByTestId('collab-editor-readonly')).toHaveCount(0, { timeout: 20_000 });

		await context.close();
		await adminContext.close();
	});
});
