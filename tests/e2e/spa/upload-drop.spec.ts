import { test, expect } from "./coverage-helpers";
import type { Page } from "@playwright/test";
import {
  apiCreateFolder,
  apiCreateSharedDrive,
  apiDeleteDrive,
  apiLogin,
  apiTrashFile,
  apiTrashFolder,
} from "./helpers";

/**
 * Browser upload from the UI — both entry points — proving §9's
 * browser-mtime plumbing end to end.
 *
 * Covers the drop handler (no coverage anywhere else in the suite) and
 * the hidden-file-input picker, with identical assertions over both. See
 * `TRANSPORTS` at the foot of the file for why the parity matters.
 *
 * # Why `setInputFiles` cannot prove the mtime
 *
 * `files.spec.ts` already uploads via `setInputFiles`, and that test is
 * worth keeping — it proves the picker reaches the server. But it cannot
 * say anything about the DATE, because `setInputFiles` offers no control
 * over `lastModified`: Playwright synthesises the `File` objects and the
 * browser stamps them with the moment of synthesis, i.e. "now".
 * Asserting `modified_at == lastModified` on top of that would pass
 * whether or not §9's plumbing exists at all — worse than no test.
 *
 * Only a `File` constructed with an explicit `lastModified` separates
 * "the browser's mtime survived" from "the server stamped NOW()", which
 * is why both helpers below build their own `File` objects: `pickFiles`
 * assigns `input.files` from a `DataTransfer`, `dropFiles` dispatches a
 * real `DragEvent`.
 *
 * # How the drop is synthesised
 *
 * A real OS drag is not drivable from Playwright, so the `DataTransfer`
 * is built inside the page and dispatched as `dragover` + `drop`.
 *
 * **It must be a REAL `DataTransfer`.** A previous attempt (branch
 * `upload-drop-tests`) passed a plain object with `files`/`items`/`types`
 * and every test failed at construction:
 *
 *     Failed to construct 'DragEvent': Failed to read the 'dataTransfer'
 *     property from 'DragEventInit': Failed to convert value to
 *     'DataTransfer'.
 *
 * `DragEventInit.dataTransfer` is WebIDL-typed, so no duck-typed stand-in
 * is accepted. `new DataTransfer()` + `items.add(file)` is both simpler
 * and more faithful — it populates `files`, sets `types` to `['Files']`
 * (which is what the page's window-level `drop` guard checks), and
 * leaves `items[0].webkitGetAsEntry()` returning `null`. That last part
 * matters: `collectDroppedEntries()` returns null when it sees no
 * directory, which is exactly the flat-file branch
 * (`uploadBatch(dt.files)`) these tests mean to exercise.
 *
 * # Not covered here
 *
 * The directory-drop branch (`webkitGetAsEntry()` yielding a real
 * `FileSystemDirectoryEntry`, walked and recreated server-side). A real
 * `DataTransfer`'s `items` are read-only, so the entry shims that branch
 * needs cannot be installed on one — it would require the duck-typed
 * object the browser rejects. That branch stays uncovered rather than
 * covered by a test that constructs an impossible event.
 */

/**
 * Dropped mtime: 2021-06-15T10:30:00Z. Chosen for three properties —
 * comfortably inside the server's trust bounds (after 2000-01-01, not
 * ahead of now), on an exact second so §9's round-half-up from
 * milliseconds is a no-op, and far enough from "now" that a server-side
 * `NOW()` stamp could never coincide with it.
 */
const DROP_MTIME_MS = Date.UTC(2021, 5, 15, 10, 30, 0);
const DROP_MTIME_WIRE = "2021-06-15T10:30:00Z";

/** A second, distinct mtime — see the multi-file test for why. */
const DROP_MTIME_2_MS = Date.UTC(2019, 0, 2, 3, 4, 5);
const DROP_MTIME_2_WIRE = "2019-01-02T03:04:05Z";

type DropPayload = {
  name: string;
  text: string;
  type: string;
  lastModified: number;
};

/**
 * Scratch folder per test. Dropping into the `/files` root would push
 * other specs' fixtures out of the virtualised list — a silent
 * "getByTestId not visible" failure in an unrelated file — so each test
 * gets its own target and trashes it afterwards (which cascades to the
 * dropped files inside).
 */
let scratchId: string | undefined;

test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test.afterEach(async ({ page }) => {
  // Best-effort: a failed test may have left the session or the folder
  // in a state where this cannot succeed, and a teardown error must not
  // mask the real assertion failure.
  if (scratchId) {
    await apiTrashFolder(page, scratchId).catch(() => {});
    scratchId = undefined;
  }
});

function uniq(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/** Create a scratch folder, open it, and wait until it accepts uploads. */
async function openEmptyScratchFolder(
  page: Page,
  prefix: string,
): Promise<string> {
  const folder = await apiCreateFolder(page, uniq(prefix));
  scratchId = folder.id;
  await page.goto(`/files/${folder.id}`);
  // `empty-state` renders only once `load()` has completed with zero
  // items, so it doubles as "this folder is ready to accept uploads".
  // Waiting on the dropzone element instead would race `load()`:
  // `guardUploadFolderReady()` refuses an upload whose folder id has not
  // resolved yet, so the drop would be silently dropped with a toast.
  await expect(page.getByTestId("empty-state")).toBeVisible({
    timeout: 15_000,
  });
  return folder.id;
}

/**
 * Drive the hidden file input — the "classic" picker path.
 *
 * **Deliberately not `setInputFiles`.** That API offers no control over
 * `lastModified`: Playwright synthesises the `File` objects and the
 * browser stamps them with the moment of synthesis, i.e. "now". An mtime
 * assertion on top of that would pass whether or not §9's plumbing
 * exists, so `files.spec.ts`'s `setInputFiles` test proves the upload
 * works but can say nothing about the date.
 *
 * Assigning `input.files` from a `DataTransfer`'s `FileList` is the
 * documented way to get a `File` with a chosen `lastModified` into an
 * input, and it preserves the value exactly (verified in Chromium).
 * `onUpload` then reads `input.files` and calls the same `uploadBatch`
 * the drop path uses, so this exercises the real handler rather than a
 * shortcut around it.
 */
async function pickFiles(page: Page, payloads: DropPayload[]): Promise<void> {
  await page.evaluate((files) => {
    const input = document.querySelector<HTMLInputElement>(
      '[data-testid="files-upload-file-input"]',
    );
    if (!input) throw new Error("files-upload-file-input element not found");
    const dt = new DataTransfer();
    for (const f of files) {
      dt.items.add(
        new File([f.text], f.name, {
          type: f.type,
          lastModified: f.lastModified,
        }),
      );
    }
    input.files = dt.files;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  }, payloads);
}

/**
 * Build a real DataTransfer in-page and dispatch dragover + drop on an
 * arbitrary element.
 *
 * The target is a parameter because the destination folder is decided by
 * WHICH element the drop lands on: the view-wide dropzone means "the open
 * folder", a folder row or a breadcrumb crumb means that folder. Both
 * events go to the same element, as in a real drag — the handler accepts
 * on `dragover` (preventDefault) and reads the files on `drop`.
 */
async function dropFilesOn(
  page: Page,
  selector: string,
  payloads: DropPayload[],
): Promise<void> {
  await page.evaluate(
    ({ selector, files }) => {
      const dt = new DataTransfer();
      for (const f of files) {
        dt.items.add(
          new File([f.text], f.name, {
            type: f.type,
            lastModified: f.lastModified,
          }),
        );
      }
      const zone = document.querySelector(selector);
      if (!zone) throw new Error(`${selector} element not found`);
      // `dragover` first, matching a real drag's sequence — the page's
      // handler calls preventDefault() there to advertise that it accepts
      // the drop, and drives the drop overlay.
      for (const type of ["dragover", "drop"]) {
        zone.dispatchEvent(
          new DragEvent(type, {
            bubbles: true,
            cancelable: true,
            dataTransfer: dt,
          }),
        );
      }
    },
    { selector, files: payloads },
  );
}

/** Drop on the view-wide zone — i.e. "into the folder that is open". */
async function dropFiles(page: Page, payloads: DropPayload[]): Promise<void> {
  await dropFilesOn(page, '[data-testid="files-dropzone"]', payloads);
}

/**
 * Look one file up in a folder's listing, server-side.
 *
 * `undefined` for "not in this folder" is load-bearing: every
 * destination test below asserts the file IS in the folder aimed at AND is
 * NOT in the folder it would have landed in before per-target drops existed.
 */
async function serverFile(
  page: Page,
  folderId: string,
  fileName: string,
): Promise<{ id: string; modified_at: string } | undefined> {
  return page.evaluate(
    async ({ folderId, fileName }) => {
      // Session-cookie auth, same as `browserFetch` in helpers.ts; a GET
      // needs no CSRF header.
      const res = await fetch(`/api/folders/${folderId}/resources?limit=50`, {
        credentials: "same-origin",
      });
      if (!res.ok) throw new Error(`listing failed: ${res.status}`);
      const page_ = (await res.json()) as {
        items: { resource: { id: string; name: string; modified_at: string } }[];
      };
      const hit = page_.items.find((i) => i.resource.name === fileName);
      return hit
        ? { id: hit.resource.id, modified_at: hit.resource.modified_at }
        : undefined;
    },
    { folderId, fileName },
  );
}

/** Read one file's `modified_at` back from the server by folder listing. */
async function serverModifiedAt(
  page: Page,
  folderId: string,
  fileName: string,
): Promise<string | undefined> {
  return (await serverFile(page, folderId, fileName))?.modified_at;
}

/**
 * Both upload entry points, same assertions.
 *
 * The parity is the point: `onDrop` and `onUpload` are separate handlers
 * that converge on `uploadBatch`, and §9's `appendSourceMtime` lives
 * further down still, in the endpoint wrapper. A regression in any one of
 * those three layers would break one transport, both, or the shared tail
 * — running identical assertions over both is what localises it.
 */
const TRANSPORTS = [
  { label: "os drop", prefix: "Drop", send: dropFiles },
  { label: "browser picker", prefix: "Pick", send: pickFiles },
] as const;

for (const { label, prefix, send } of TRANSPORTS) {
  test(`${label}: upload preserves the browser mtime`, async ({ page }) => {
    const folderId = await openEmptyScratchFolder(page, prefix);
    const name = `${prefix.toLowerCase()}-june-2021.txt`;

    await send(page, [
      {
        name,
        text: "one file, one explicit mtime\n",
        type: "text/plain",
        lastModified: DROP_MTIME_MS,
      },
    ]);

    // The row appearing proves the whole client path fired: handler →
    // uploadBatch → POST /api/files/upload → 201 → reload().
    await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });

    // §9: `appendSourceMtime` reads `file.lastModified` and appends
    // `source_modified_at` BEFORE the `file` part — ordering that matters,
    // because the server stops reading multipart fields once `file`
    // consumes the stream. If that field is dropped, reordered, or never
    // sent, the server stamps NOW() and this fails with a today-ish
    // timestamp instead of the 2021 one.
    expect(await serverModifiedAt(page, folderId, name)).toBe(DROP_MTIME_WIRE);
  });

  test(`${label}: multiple files each keep their own mtime`, async ({
    page,
  }) => {
    const folderId = await openEmptyScratchFolder(page, `${prefix}Multi`);
    const first = `${prefix.toLowerCase()}-multi-2021.txt`;
    const second = `${prefix.toLowerCase()}-multi-2019.txt`;

    // Two DIFFERENT mtimes in one gesture. A single shared value would
    // still pass if `uploadBatch` hoisted one file's mtime across the
    // whole batch — the distinct values are what make that regression
    // detectable.
    await send(page, [
      {
        name: first,
        text: "first\n",
        type: "text/plain",
        lastModified: DROP_MTIME_MS,
      },
      {
        name: second,
        text: "second\n",
        type: "text/plain",
        lastModified: DROP_MTIME_2_MS,
      },
    ]);

    await expect(page.getByTestId(first)).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId(second)).toBeVisible({ timeout: 15_000 });

    expect(await serverModifiedAt(page, folderId, first)).toBe(DROP_MTIME_WIRE);
    expect(await serverModifiedAt(page, folderId, second)).toBe(
      DROP_MTIME_2_WIRE,
    );
  });
}

/**
 * WHERE the files land — the two targets that aren't the open folder.
 *
 * The regression both tests pin: until 2026-11-13 the row and crumb
 * handlers bailed out of any external drag (`if
 * (!e.dataTransfer?.types.includes(DRAG_TYPE)) return; // external file
 * drop → page dropzone`), so OS files bubbled up to the view-wide zone and
 * landed in the OPEN folder regardless of what the pointer was over.
 * Aiming at a folder silently did something else — the one failure mode a
 * file manager must not have.
 *
 * Hence two assertions per test, and the SECOND is the load-bearing one:
 * present in the intended folder AND absent from the open one. "Present in
 * the target" alone would also pass on the old code whenever the target
 * happened to be the open folder.
 *
 * No picker counterpart: `onUpload` reads a hidden input that has no
 * pointer and therefore no destination but the open folder, so the
 * `TRANSPORTS` parity above doesn't apply here.
 */
const AIMED_DROP_TEXT = "aimed at a folder, not at the view\n";

test("os drop on a folder row uploads into that folder, not the open one", async ({
  page,
}) => {
  const parent = await apiCreateFolder(page, uniq("DropOnRow"));
  scratchId = parent.id;
  const childName = uniq("row-target");
  const child = await apiCreateFolder(page, childName, parent.id);

  await page.goto(`/files/${parent.id}`);
  // The child's row rendering proves `load()` finished — which is also what
  // `guardUploadFolderReady()` needs before it will accept an upload.
  await expect(page.getByTestId(childName)).toBeVisible({ timeout: 15_000 });

  const name = `${childName}.txt`;
  await dropFilesOn(page, `[data-testid="${childName}"]`, [
    {
      name,
      text: AIMED_DROP_TEXT,
      type: "text/plain",
      lastModified: DROP_MTIME_MS,
    },
  ]);

  // Nothing appears in the open view — the file is one level down — so the
  // server listing is the only observation point. Polling also covers the
  // upload being in flight.
  await expect
    .poll(() => serverModifiedAt(page, child.id, name), { timeout: 15_000 })
    .toBe(DROP_MTIME_WIRE);
  expect(await serverModifiedAt(page, parent.id, name)).toBeUndefined();
});

test("os drop on an ancestor crumb uploads into THAT ancestor", async ({
  page,
}) => {
  // Three levels deep, because two cannot tell the behaviours apart: with a
  // single ancestor, "lands in the crumb you aimed at" and "lands in the
  // nearest ancestor" / "lands in the drive root" are the same folder. The
  // middle crumb is the target precisely because it is neither end.
  const top = await apiCreateFolder(page, uniq("DropOnCrumb"));
  scratchId = top.id;
  const mid = await apiCreateFolder(page, uniq("crumb-mid"), top.id);
  const leaf = await apiCreateFolder(page, uniq("crumb-leaf"), mid.id);

  // Open the LEAF: `mid` and `top` are then reachable only as crumbs, which
  // is the whole point — this is the only gesture that uploads outside the
  // folder you are looking at without navigating first.
  await page.goto(`/files/${leaf.id}`);
  await expect(page.getByTestId("empty-state")).toBeVisible({
    timeout: 15_000,
  });
  // The crumb chain resolves in its own request, so `empty-state` can be up
  // before the crumbs exist — dropping then would miss the element.
  await expect(page.getByTestId(`folder-breadcrumb-${mid.id}`)).toBeVisible({
    timeout: 15_000,
  });

  const name = `${uniq("crumb-drop")}.txt`;
  await dropFilesOn(page, `[data-testid="folder-breadcrumb-${mid.id}"]`, [
    {
      name,
      text: AIMED_DROP_TEXT,
      type: "text/plain",
      lastModified: DROP_MTIME_MS,
    },
  ]);

  await expect
    .poll(() => serverModifiedAt(page, mid.id, name), { timeout: 15_000 })
    .toBe(DROP_MTIME_WIRE);
  // Not in the folder on screen (the old behaviour), and not in the crumb
  // one further up either.
  expect(await serverModifiedAt(page, leaf.id, name)).toBeUndefined();
  expect(await serverModifiedAt(page, top.id, name)).toBeUndefined();
});

/**
 * Drop on a DRIVE row in the sidebar → upload into that drive's root.
 *
 * The one destination that isn't on the page. `AppShell` cannot upload —
 * the uploader (progress, abort, reload) lives on `/files` — so this is the
 * only test that covers the handoff: the sidebar extracts the drop, stages
 * it in `pendingUpload`, navigates to the drive root, and the page performs
 * it once that folder has loaded. Three things can break independently
 * (extract, stage+navigate, drain), and all three fail this test.
 *
 * A REAL drive, not a stubbed `/api/drives`: a made-up `root_folder_id`
 * renders a sidebar row fine and 404s the upload.
 */
test("os drop on another drive in the sidebar uploads into that drive's root", async ({
  page,
}) => {
  const drive = await apiCreateSharedDrive(page, uniq("DropDrive"));
  const name = `${uniq("drive-drop")}.txt`;
  // Somewhere else to be standing: the drop must NOT land here.
  const away = await apiCreateFolder(page, uniq("DropDriveAway"));
  scratchId = away.id;

  try {
    await page.goto(`/files/${away.id}`);
    await expect(page.getByTestId("empty-state")).toBeVisible({
      timeout: 15_000,
    });
    // The picker renders only with more than one drive — the new shared one
    // plus the account's personal drive — so its row appearing also confirms
    // the drive was really created.
    const row = page.getByTestId(`drive-picker-row-${drive.id}`);
    await expect(row).toBeVisible({ timeout: 15_000 });

    await dropFilesOn(page, `[data-testid="drive-picker-row-${drive.id}"]`, [
      {
        name,
        text: AIMED_DROP_TEXT,
        type: "text/plain",
        lastModified: DROP_MTIME_MS,
      },
    ]);

    // Dropping on a drive navigates there: an upload the user cannot see
    // land is one they will do twice.
    await expect(page).toHaveURL(new RegExp(`/files/${drive.root_folder_id}`), {
      timeout: 15_000,
    });
    await expect
      .poll(() => serverModifiedAt(page, drive.root_folder_id, name), {
        timeout: 20_000,
      })
      .toBe(DROP_MTIME_WIRE);
    expect(await serverModifiedAt(page, away.id, name)).toBeUndefined();
  } finally {
    // A drive with anything LIVE in it cannot be deleted (409), so the file
    // is trashed first — `is_empty` ignores trashed rows, which is why this
    // does NOT empty the trash: that bin is shared with every other spec
    // running against this database.
    //
    // All best-effort: a teardown error must not mask the assertion that
    // actually failed.
    const uploaded = await serverFile(page, drive.root_folder_id, name).catch(
      () => undefined,
    );
    if (uploaded) await apiTrashFile(page, uploaded.id).catch(() => {});
    await apiDeleteDrive(page, drive.id).catch(() => {});
  }
});

/**
 * A drop on a page with no upload route is refused — and SAYS so.
 *
 * `/admin` stands in for "anywhere that isn't Files". Two halves, and the
 * first is the one that matters: the browser's default action for an
 * unhandled file drop is to NAVIGATE to the dropped file, replacing the SPA
 * and any unsaved state with it. The app-wide guard swallows that. The
 * second half is the explanation plus a way out, because a drag that
 * silently does nothing reads as a broken app.
 */
test("an os drop outside Files is swallowed, explained, and uploads nothing", async ({
  page,
}) => {
  // `apiCreateFolder` with no parent returns the caller's home root as
  // `parent_id` — the folder a stray upload would most plausibly land in,
  // and therefore the one to prove stayed empty.
  const probe = await apiCreateFolder(page, uniq("AdminDropProbe"));
  scratchId = probe.id;
  const homeRoot = probe.parent_id;
  expect(homeRoot).not.toBeNull();

  await page.goto("/admin");
  await expect(page.getByTestId("appshell-nav-admin-dashboard-link")).toBeVisible({
    timeout: 15_000,
  });

  const name = `${uniq("admin-drop")}.txt`;
  // On `body`, not a dropzone: the guard listens at window level, so this is
  // the "dropped on a page that has no uploader" case.
  await dropFilesOn(page, "body", [
    {
      name,
      text: "should go nowhere\n",
      type: "text/plain",
      lastModified: DROP_MTIME_MS,
    },
  ]);

  // `.first()`: the toaster is a stack, and a stray toast from page load
  // would otherwise make this a strict-mode violation rather than a result.
  const toast = page.locator('[data-testid^="toaster-toast-"]').first();
  await expect(toast).toContainText("Uploads only work in Files", {
    timeout: 15_000,
  });
  // Still on /admin: no navigation to the dropped file, and no silent
  // redirect either.
  await expect(page).toHaveURL(/\/admin/);

  // The way out. Clicking it is also the only check that the action is wired
  // to a navigation rather than just rendering a label.
  await page.locator('[data-testid^="toaster-action-btn-"]').first().click();
  await expect(page).toHaveURL(/\/files/, { timeout: 15_000 });

  // Nothing was uploaded anywhere reachable: not the home root…
  expect(await serverModifiedAt(page, homeRoot as string, name)).toBeUndefined();
  // …and not the scratch folder either.
  expect(await serverModifiedAt(page, probe.id, name)).toBeUndefined();
});
