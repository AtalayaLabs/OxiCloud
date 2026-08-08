import { test as base, Page, expect } from '@playwright/test';
import { startStack, Stack } from '../fixtures/oxicloud-stack';

/**
 * When `OXICLOUD_E2E_CONTAINERS=1`, each Playwright worker boots its own
 * isolated DB + app stack via Testcontainers (see `playwright.containers.
 * config.ts`). Otherwise the legacy single-server webServer flow is used
 * and this fixture is an inert pass-through.
 */
const USE_CONTAINERS = process.env.OXICLOUD_E2E_CONTAINERS === '1';

type WorkerFixtures = {
  /** The per-worker isolated stack, or `null` in the legacy webServer flow. */
  stack: Stack | null;
};

/**
 * Extended `test` fixture with two responsibilities:
 *
 *  1. `stack` (worker-scoped) — in container mode, starts a dedicated
 *     DB + app stack per worker, seeds its admin, and tears it down at
 *     worker exit. The app instance is reused across every test the worker
 *     runs, so container startup is paid once per worker, not per test.
 *  2. `page` — fails any test that produces an unhandled browser-side JS
 *     error (SyntaxError, ReferenceError, uncaught rejection, etc.).
 *
 * Import `test` from this module instead of `@playwright/test` so every spec
 * gets both behaviours automatically without per-file boilerplate.
 */
export const test = base.extend<object, WorkerFixtures>({
    stack: [
        async ({}, use) => {
            if (!USE_CONTAINERS) {
                await use(null);
                return;
            }
            const stack = await startStack();
            try {
                await seedAdmin(stack.baseURL);
                await use(stack);
            } finally {
                await stack.stop();
            }
        },
        // Worker-scoped: booting Postgres + the app container (image build on
        // first run, migrations on boot) can take well over the default 30s
        // fixture timeout. Match the 180s container startup budget in startStack.
        { scope: 'worker', timeout: 200_000 },
    ],

    // Point relative `page.goto('/')` at the per-worker stack when present;
    // otherwise fall back to the baseURL configured in the project (the
    // legacy webServer at :8087).
    baseURL: async ({ stack }, use, testInfo) => {
        await use(stack ? stack.baseURL : testInfo.project.use.baseURL);
    },

    page: async ({ page }, use) => {
        const jsErrors: Error[] = [];
        page.on('pageerror', (err) => jsErrors.push(err));
        await use(page);
        if (jsErrors.length > 0) {
            throw new Error(
                `${jsErrors.length} unhandled JS error(s) on page:\n` +
                jsErrors.map((e) => `  • ${e.message}`).join('\n')
            );
        }
    },
});

export const TEST_ADMIN = {
  username: 'admin',
  email: 'testadmin@example.com',
  password: 'TestPassword1!',
};

/**
 * Create the first-admin account via the public `POST /api/setup` route.
 * Idempotent: a 409 (admin already exists) is treated as success so the
 * call is safe to retry and to run once per worker.
 *
 * Shared by `global-setup.ts` (legacy flow, single server) and the
 * worker-scoped `stack` fixture (container flow, one server per worker).
 */
export async function seedAdmin(baseURL: string, admin = TEST_ADMIN): Promise<void> {
  const res = await fetch(`${baseURL}/api/setup`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      username: admin.username,
      email: admin.email,
      password: admin.password,
    }),
  });
  if (!res.ok && res.status !== 409) {
    throw new Error(`Admin setup failed: ${res.status} ${await res.text()}`);
  }
}

/**
 * Authenticate the page's browser context, ready for subsequent
 * `page.goto()` calls to load already-signed-in.
 *
 * Uses the SPA's real login flow (`page.goto('/login')` → fill form
 * → submit) rather than a bare `POST /api/auth/login`, so this works
 * correctly under both auth modes the test env supports:
 *
 *   * `OXICLOUD_AUTH_OPAQUE_MODE=off` — SPA does legacy login,
 *     server accepts.
 *   * `OXICLOUD_AUTH_OPAQUE_MODE=migrate` — first login legacy-
 *     succeeds + silently mints an OPAQUE envelope (Phase 2 hook);
 *     every subsequent login the SPA detects the envelope via
 *     `/api/auth/opaque/login/lookup` and does the full KE1/KE3
 *     OPAQUE handshake. Legacy `POST /api/auth/login` would 403
 *     with `opaque_migrated_use_opaque` (Phase 4) from the second
 *     login on — that's what the old bare-POST apiLogin used to
 *     hit as soon as OPAQUE went from `off` to `migrate`.
 *   * `OXICLOUD_DPOP_MODE=required` — the SPA computes and sends
 *     `dpop_jkt` in the login body; the session is created bound.
 *     A bare-POST wouldn't include it, so subsequent requests
 *     wouldn't get DPoP-signed. Going through the SPA keeps the
 *     end-to-end flow honest.
 *
 * Overhead vs the old direct POST: ~200-500 ms per test to load
 * `/login`, submit, and wait for the post-login redirect. Runs
 * once per test (from `beforeEach`), so the total suite tax is
 * modest and the coverage payoff is real.
 */
export async function apiLogin(page: Page, admin = TEST_ADMIN): Promise<void> {
  // Idempotence check — many specs' beforeEach + test body both call
  // apiLogin; the old bare-POST version was a no-op on a live
  // session, and callers depend on that. Under UI-driven login,
  // navigating to /login when already authenticated triggers the
  // SPA's layout guard to redirect away → the login form never
  // renders → the fill() below times out. Probe /api/auth/me FIRST:
  // 2xx means we're already signed in as SOMEONE. If that's the
  // right admin, no-op; otherwise fall through to a fresh login.
  const probe = await page.request.get('/api/auth/me').catch(() => null);
  if (probe?.ok()) {
    const body = (await probe.json().catch(() => ({}))) as { username?: string };
    if (body.username === admin.username) return;
  }

  await page.goto('/login');
  // Wait for the SPA's boot probes (`getOidcProviders` +
  // `getAuthStatus` in `login/+page.svelte::onMount`) to complete
  // BEFORE touching the form. Otherwise the boot `$effect` fires
  // MID-FILL — when `booting` flips from true to false, the
  // auto-focus effect steals focus back to the identifier input,
  // and any remaining characters of the password-fill land in
  // the username field. Symptom: username="adminTestPassword1!",
  // password="", submit-button shows "Send sign-in link" → SPA
  // fires magic-link/send with the concatenated identifier and
  // login never completes.
  //
  // `networkidle` waits for the network to have no more than 0
  // requests in flight for 500 ms. By that point providers
  // + status have landed and `booting = false` has already
  // stabilised → the auto-focus effect fired ONCE (harmlessly,
  // before we touch the form), never again during our fills.
  await page.waitForLoadState('networkidle');
  await page.getByTestId('login-username-input').fill(admin.username);
  await page.getByTestId('login-password-input').fill(admin.password);
  await page.getByTestId('login-submit-btn').click();
  // Post-login the SPA's `goto(redirectTarget)` sends the user
  // to `/files` (default) or a `?redirect=` target — OR to
  // `/profile?forcePasswordChange=1` when the backend has stamped
  // `force_password_change_at_next_login=true` on this account
  // (usually because a prior admin-reset test flipped it). Match
  // any post-login destination that ISN'T `/login` itself. The
  // 15s ceiling covers the OPAQUE-post-migration path: WASM load
  // + KE1 + KE3 + Argon2id.
  await page.waitForURL((url) => !url.pathname.startsWith('/login'), {
    timeout: 15_000,
    waitUntil: 'commit'
  });
}

/**
 * Build the `x-csrf-token` header for a cookie-authenticated, state-changing
 * request. The server uses a double-submit cookie: login sets a non-HttpOnly
 * `oxicloud_csrf` cookie, and POST/PUT/DELETE must echo its value in the
 * header (see `csrf_middleware`). Returns `{}` if no cookie is present (e.g.
 * not logged in), letting the caller's request fail with the real 4xx.
 */
async function csrfHeaders(page: Page): Promise<Record<string, string>> {
  const cookies = await page.context().cookies();
  const token = cookies.find((c) => c.name === 'oxicloud_csrf')?.value;
  return token ? { 'x-csrf-token': token } : {};
}

/** A folder as returned by the API (subset we use). */
export type ApiFolder = { id: string; parent_id: string | null };

/**
 * Create a folder via the API and return it. `parentId` omitted ⇒ the folder
 * is created in the caller's home (root) folder, and the returned `parent_id`
 * is that home folder's id (handy as the target for "root" file uploads, which
 * require an explicit folder id). Requires the page to already be
 * authenticated (see `apiLogin`).
 */
export async function apiCreateFolder(
  page: Page,
  name: string,
  parentId?: string,
): Promise<ApiFolder> {
  const res = await page.request.post('/api/folders', {
    headers: await csrfHeaders(page),
    data: parentId ? { name, parent_id: parentId } : { name },
  });
  if (!res.ok()) {
    throw new Error(`apiCreateFolder(${name}) failed: ${res.status()} ${await res.text()}`);
  }
  return (await res.json()) as ApiFolder;
}

/**
 * Create a regular user via the admin API. Requires the page to be authenticated
 * as an admin (see `apiLogin`). Returns the created username. Handy for tests
 * that need a second account (sharing, group membership, recipient search).
 */
export async function apiAdminCreateUser(page: Page, username: string): Promise<string> {
  const res = await page.request.post('/api/admin/users', {
    headers: await csrfHeaders(page),
    data: {
      username,
      password: 'TestPassword1!',
      email: `${username}@example.test`,
      role: 'user',
      quota_bytes: 1073741824,
    },
  });
  if (!res.ok()) {
    throw new Error(`apiAdminCreateUser(${username}) failed: ${res.status()} ${await res.text()}`);
  }
  return username;
}

/**
 * Create a group via the API (requires an authenticated admin/manager). Returns
 * the group name. Useful for sharing-with-group and group-membership tests.
 */
export async function apiCreateGroup(page: Page, name: string): Promise<string> {
  const res = await page.request.post('/api/groups', {
    headers: await csrfHeaders(page),
    data: { name, description: null },
  });
  if (!res.ok()) {
    throw new Error(`apiCreateGroup(${name}) failed: ${res.status()} ${await res.text()}`);
  }
  return name;
}

/** Move a folder to trash via the API (DELETE /api/folders/{id}). */
export async function apiTrashFolder(page: Page, folderId: string): Promise<void> {
  const res = await page.request.delete(`/api/folders/${folderId}`, {
    headers: await csrfHeaders(page),
  });
  if (!res.ok()) {
    throw new Error(`apiTrashFolder(${folderId}) failed: ${res.status()} ${await res.text()}`);
  }
}

/**
 * Record an access in the user's "recent" list (POST /api/recent/{type}/{id})
 * so the /recent route has deterministic content. Best-effort: a non-2xx is
 * tolerated so callers don't fail on a recents quirk.
 */
export async function apiRecordRecent(
  page: Page,
  itemType: 'file' | 'folder',
  id: string,
): Promise<void> {
  await page.request
    .post(`/api/recent/${itemType}/${id}`, { headers: await csrfHeaders(page) })
    .catch(() => {});
}

/** Empty the trash via the API (DELETE /api/trash/empty) for a clean slate. */
export async function apiEmptyTrash(page: Page): Promise<void> {
  const res = await page.request.delete('/api/trash/empty', { headers: await csrfHeaders(page) });
  if (!res.ok()) {
    throw new Error(`apiEmptyTrash failed: ${res.status()} ${await res.text()}`);
  }
}

/**
 * Flip the caller's `ui_preferences.hide_dotfiles` server-side. Used by the
 * dotfile-filter e2e spec to establish a known state at test start and to
 * clean up at teardown so sibling tests aren't polluted by a leftover
 * "hidden" mode (the preference is persistent across sessions because it's
 * stored on `auth.users.ui_preferences`, not in localStorage).
 *
 * PATCHes only `hide_dotfiles`; siblings in the bag (view_mode, future
 * keys) survive the shallow-merge on the server side.
 */
export async function apiSetHideDotfiles(page: Page, hide: boolean): Promise<void> {
  const res = await page.request.patch('/api/auth/me/profile', {
    headers: await csrfHeaders(page),
    data: { ui_preferences: { hide_dotfiles: hide } },
  });
  if (!res.ok()) {
    throw new Error(`apiSetHideDotfiles(${hide}) failed: ${res.status()} ${await res.text()}`);
  }
}

/** A file to seed: its name, MIME type, and raw bytes. */
export type SeedFile = { name: string; mimeType: string; body: Buffer };

/**
 * Upload one file via the API into `folderId`. The target folder is
 * **required**: the server resolves the file's owner from its parent folder
 * and rejects an upload with no `folder_id` ("folder_id is required to
 * determine file owner"). For a "root" file, pass the home folder's id — the
 * `parent_id` returned by `apiCreateFolder(name)`.
 *
 * The `folder_id` field is sent before `file` because the upload handler
 * parses the multipart stream in order and permission-checks the target folder
 * before spooling the body.
 */
export async function apiUploadFile(
  page: Page,
  file: SeedFile,
  folderId: string,
): Promise<void> {
  const filePart = { name: file.name, mimeType: file.mimeType, buffer: file.body };
  const res = await page.request.post('/api/files/upload', {
    headers: await csrfHeaders(page),
    multipart: { folder_id: folderId, file: filePart },
  });
  if (!res.ok()) {
    throw new Error(`apiUploadFile(${file.name}) failed: ${res.status()} ${await res.text()}`);
  }
}

/**
 * A small library of files spanning common types (text, markdown, JSON, CSV,
 * PNG image, PDF), so a recording starts from a browser that exercises the
 * different icons / previews / row renderers. Bytes are tiny but valid.
 */
export const SAMPLE_FILES = {
  text: (): SeedFile => ({
    name: 'notes.txt',
    mimeType: 'text/plain',
    body: Buffer.from('Hello from the codegen seed.\nLine two.\n'),
  }),
  markdown: (): SeedFile => ({
    name: 'README.md',
    mimeType: 'text/markdown',
    body: Buffer.from('# Seeded\n\nA **markdown** file for the file browser.\n'),
  }),
  json: (): SeedFile => ({
    name: 'config.json',
    mimeType: 'application/json',
    body: Buffer.from(JSON.stringify({ seeded: true, items: [1, 2, 3] }, null, 2)),
  }),
  csv: (): SeedFile => ({
    name: 'data.csv',
    mimeType: 'text/csv',
    body: Buffer.from('id,name,size\n1,alpha,10\n2,beta,20\n'),
  }),
  png: (): SeedFile => ({
    name: 'pixel.png',
    mimeType: 'image/png',
    // 1×1 transparent PNG.
    body: Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
      'base64',
    ),
  }),
  pdf: (): SeedFile => ({
    name: 'sample.pdf',
    mimeType: 'application/pdf',
    body: Buffer.from(
      '%PDF-1.1\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n' +
        '2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n' +
        '3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]>>endobj\n' +
        'trailer<</Root 1 0 R>>\n%%EOF\n',
    ),
  }),
};

/**
 * Seed a representative tree of folders and files of different types for the
 * authenticated user, so a codegen recording (or a test) starts from a
 * populated file browser. Idempotent enough for one run per worker; re-running
 * creates duplicate names (the backend allows them). Requires `apiLogin` first.
 *
 * Layout created in the user's home folder:
 *
 *   config.json                 (home/root)
 *   pixel.png                   (home/root)
 *   Documents/                  README.md, notes.txt
 *   Documents/Reports/          data.csv, sample.pdf
 *   Images/                     pixel.png
 *
 * Returns the created folder ids (plus the resolved `home` id) so callers can
 * deep-link or assert.
 */
export async function seedFilesAndFolders(
  page: Page,
): Promise<{ home: string; documents: string; reports: string; images: string }> {
  const documents = await apiCreateFolder(page, 'Documents');
  const reports = await apiCreateFolder(page, 'Reports', documents.id);
  const images = await apiCreateFolder(page, 'Images');

  // Created-at-root folders carry the home folder id as their parent — use it
  // as the target for the "root" files (uploads require an explicit folder).
  const home = documents.parent_id;
  if (!home) {
    throw new Error('seedFilesAndFolders: could not resolve home folder id from a root folder');
  }

  await apiUploadFile(page, SAMPLE_FILES.json(), home);
  await apiUploadFile(page, SAMPLE_FILES.png(), home);
  await apiUploadFile(page, SAMPLE_FILES.markdown(), documents.id);
  await apiUploadFile(page, SAMPLE_FILES.text(), documents.id);
  await apiUploadFile(page, SAMPLE_FILES.csv(), reports.id);
  await apiUploadFile(page, SAMPLE_FILES.pdf(), reports.id);
  await apiUploadFile(page, SAMPLE_FILES.png(), images.id);

  return { home, documents: documents.id, reports: reports.id, images: images.id };
}

/**
 * Log in as the test admin and wait until the main app is fully initialized.
 *
 * We wait for two things after the login redirect:
 *  1. `#sidebar` — confirms the main HTML has loaded.
 *  2. `#user-avatar-btn .user-vignette` — confirms that `setupUserMenu()` has
 *     run and mounted the avatar vignette.  This is the earliest reliable
 *     signal that the click-handler on the avatar button is attached, so any
 *     subsequent test that opens the user menu will not race against JS startup.
 *
 * Without (2), CI (Ubuntu + Xvfb) occasionally clicks the button before the
 * event listener is registered because the JS runtime is slower than on macOS.
 */
export async function loginAsAdmin(page: Page) {
  await goToLoginPage(page);
  await page.locator('#login-username').fill(TEST_ADMIN.username);
  await page.locator('#login-password').fill(TEST_ADMIN.password);
  await page.locator('#login-submit').click();
  await expect(page.locator('#sidebar')).toBeVisible({ timeout: 15_000 });
  // Wait for the JS app to initialise: avatar vignette present ⟹ click handler attached.
  await expect(page.locator('#user-avatar-btn .user-vignette')).toBeAttached({ timeout: 10_000 });
}

/**
 * Navigate to `/` and land on the login panel, handling the language selector
 * if it appears (fresh localStorage). The admin account is guaranteed to exist
 * because globalSetup created it before any test ran.
 */
export async function goToLoginPage(page: Page) {
  await page.goto('/');

  // Both panels start with .hidden — wait for JS to reveal one.
  // Use expect() (5 s default) rather than waitForSelector() (30 s) so a JS
  // crash fails fast instead of hanging for the full test timeout.
  await expect(
    page.locator('#language-panel:not(.hidden), #login-panel:not(.hidden)').first()
  ).toBeAttached();

  if (await page.locator('#language-panel').isVisible()) {
    await page.locator('#language-continue').click();
  }

  await expect(page.locator('#login-panel')).toBeVisible();
}
