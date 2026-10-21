import { test, expect } from "./coverage-helpers";
import type { Page } from "@playwright/test";
import { apiCreateFolder, apiLogin, apiTrashFolder } from "./helpers";

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

/** Build a real DataTransfer in-page and dispatch dragover + drop. */
async function dropFiles(page: Page, payloads: DropPayload[]): Promise<void> {
  await page.evaluate((files) => {
    const dt = new DataTransfer();
    for (const f of files) {
      dt.items.add(
        new File([f.text], f.name, {
          type: f.type,
          lastModified: f.lastModified,
        }),
      );
    }
    const zone = document.querySelector('[data-testid="files-dropzone"]');
    if (!zone) throw new Error("files-dropzone element not found");
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
  }, payloads);
}

/** Read one file's `modified_at` back from the server by folder listing. */
async function serverModifiedAt(
  page: Page,
  folderId: string,
  fileName: string,
): Promise<string | undefined> {
  return page.evaluate(
    async ({ folderId, fileName }) => {
      // Session-cookie auth, same as `browserFetch` in helpers.ts; a GET
      // needs no CSRF header.
      const res = await fetch(`/api/folders/${folderId}/resources?limit=50`, {
        credentials: "same-origin",
      });
      if (!res.ok) throw new Error(`listing failed: ${res.status}`);
      const page_ = (await res.json()) as {
        items: { resource: { name: string; modified_at: string } }[];
      };
      return page_.items.find((i) => i.resource.name === fileName)?.resource
        .modified_at;
    },
    { folderId, fileName },
  );
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
  { label: "drop", prefix: "Drop", send: dropFiles },
  { label: "file picker", prefix: "Pick", send: pickFiles },
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
