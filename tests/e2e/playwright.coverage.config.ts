import { defineConfig, devices } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';
import { loadEnv } from './load-env';

/**
 * Coverage e2e suite — drives the **SvelteKit** SPA (built to `static-dist/`
 * with Istanbul instrumentation via `COVERAGE=1`) and collects per-test
 * `window.__coverage__` into `.nyc_output/` for an nyc report.
 *
 * Two run modes, controlled by the `OXICLOUD_SHARED_WORKER_MODE` env var,
 * executed SEQUENTIALLY (same port, same storage, one backend at a time):
 *
 *   * **default (`OXICLOUD_SHARED_WORKER_MODE` unset)** —
 *     production-shape (opt-in OFF → per-tab mode). Backend boots with
 *     `OXICLOUD_ENABLE_SHARED_WORKER_BUS` unset; the suite runs every
 *     spec EXCEPT the `message-bus-*.spec.ts` ones. `networkidle`
 *     works here because nothing opens a WS until a consumer
 *     subscribes, and no subscribes fire on /login.
 *
 *   * **shared-worker (`OXICLOUD_SHARED_WORKER_MODE=1`)** — opt-in
 *     flag ON. Backend boots with `OXICLOUD_ENABLE_SHARED_WORKER_BUS=true`;
 *     the suite runs ONLY the `message-bus-*.spec.ts` specs (including
 *     the fallback test, which needs the server flag on for the factory
 *     to try shared mode in the first place before `addInitScript({
 *     delete window.SharedWorker })` deflects it).
 *
 * Running the opt-in flag suite-wide broke every spec that used
 * `waitForLoadState('networkidle')` (the SharedWorker's eagerly-opened
 * connection kept Playwright's "no network for 500 ms" counter from
 * firing). Running two backends in parallel on different ports caused
 * boot-time contention on shared state we don't fully understand yet.
 * Sequential same-port runs sidestep both: one backend at a time, each
 * run picks its env.
 *
 * Invocation:
 *   # per-tab suite:
 *   npx playwright test -c playwright.coverage.config.ts
 *
 *   # shared-worker suite:
 *   OXICLOUD_SHARED_WORKER_MODE=1 npx playwright test -c playwright.coverage.config.ts
 *
 * CI runs both back-to-back; the second run's `start-server-spa.sh`
 * wipes storage, globalSetup re-seeds (and 409-handles the existing
 * admin in the shared DB).
 *
 * Production-shape end-to-end: OPAQUE + DPoP `required` are both
 * inherited from `tests/common/server.env` — the previous legacy
 * `./static` vanilla-frontend suite that overrode them off has been
 * retired.
 *
 * Build the instrumented SPA first:
 *   (cd frontend && COVERAGE=1 VITE_E2E=1 npm run build)
 */
const startScript = path.join(__dirname, 'start-server-spa.sh');
const commonEnv = loadEnv(path.join(__dirname, '../common/server.env'));
const workspace = process.env.GITHUB_WORKSPACE ?? path.join(__dirname, '../..');

/** Shared-worker-bus mode toggle. Set `OXICLOUD_SHARED_WORKER_MODE=1`
 *  for the shared-worker run; anything else is per-tab mode. The
 *  bool-ness is boot-time; the config isn't reactive after import. */
const SHARED_WORKER_MODE = process.env.OXICLOUD_SHARED_WORKER_MODE === '1';

/** Spec files that only make sense against an opt-in-ON backend —
 *  they assert `SharedWorkerBridge` is live and exercise the shared-
 *  worker fan-out / reconnect / fallback properties. */
const SHARED_WORKER_SPECS = /message-bus-.*\.spec\.ts/;

/**
 * Pick the server command: reuse an already-built binary when one is there
 * and demonstrably usable, else `cargo run`.
 *
 * `cargo run` is a 5-minute rebuild whenever the last cargo invocation used a
 * different feature set — `cargo clippy --all-features` and
 * `just gen-openapi` (`--features dev_tools`) both do, so in practice an
 * ordinary dev loop pays that cost on nearly every e2e run even though
 * `target/debug/oxicloud` is sitting right there.
 *
 * Reuse is only safe if the binary is actually equivalent to what `cargo run`
 * would have produced, so BOTH guards must pass. Either failing falls back to
 * `cargo run`, which rebuilds correctly — a wasted 5 minutes is the acceptable
 * failure, silently testing the wrong server is not:
 *
 *  1. **Not stale.** Newer than every `.rs` under `src/`, than `migrations/`,
 *     and than `Cargo.toml`/`Cargo.lock`. Without this a forgotten rebuild
 *     makes the suite grade the previous commit — green on code that no
 *     longer exists, which is worse than no run at all.
 *  2. **Built with `--features plugins`.** This config sets
 *     `OXICLOUD_ENABLE_PLUGINS=true` and the admin Plugins tab specs install
 *     the hello fixture. The feature set is not recorded anywhere on disk
 *     (`--version` doesn't report it), so probe for linked wasmtime symbols:
 *     present in a plugins build, absent entirely otherwise.
 *
 * `BUILD_TARGET` keeps its old meaning exactly — an explicit "use
 * target/<profile>/oxicloud", unconditional, no probing. That is the CI path
 * (CI just built it, from a known feature set) and it stays untouched.
 * `E2E_BUILD=cargo` forces the cargo path when you want to be sure.
 */
function newestMtime(paths: string[]): number {
  let newest = 0;
  const visit = (p: string) => {
    let st: fs.Stats;
    try {
      st = fs.statSync(p);
    } catch {
      return; // absent is not newer than anything
    }
    if (st.isDirectory()) {
      for (const entry of fs.readdirSync(p)) visit(path.join(p, entry));
    } else if (st.mtimeMs > newest) {
      newest = st.mtimeMs;
    }
  };
  paths.forEach(visit);
  return newest;
}

function hasPluginsFeature(bin: string): boolean {
  // Byte scan rather than shelling out to `nm`/`strings`, so this works the
  // same wherever Node runs. ~0.2 s on a 116 MB debug binary, once per run.
  try {
    return fs.readFileSync(bin).includes('wasmtime');
  } catch {
    return false;
  }
}

/**
 * Refuse to run against a `static-dist/` older than the frontend sources.
 *
 * The suite serves `static-dist/`, which Vite only writes on an explicit
 * build — so editing a component and running Playwright tests the PREVIOUS
 * bundle. The failure is silent and reads as a product bug: a new
 * `data-testid` is simply absent, so the test either times out or, worse,
 * hits a tolerant guard and SKIPS.
 *
 * This bit twice in one session. First a dialog that did not exist in the
 * bundle, then a green run that never exercised the code it was meant to
 * verify. Both times the output looked fine.
 *
 * Throwing here is the point: a false green is worse than a failed run, and
 * the fix is one command. Not auto-built — a 40-second build triggered from
 * a config file is a surprise, and the explicit step is also the record of
 * WHICH bundle was tested.
 */
function assertFreshBundle(): void {
  const dist = path.join(workspace, 'static-dist');
  if (!fs.existsSync(path.join(dist, 'index.html'))) {
    throw new Error(
      '[e2e] static-dist/index.html is missing — build the instrumented SPA first:\n' +
        '        just fe-build-e2e\n'
    );
  }
  const distAge = newestMtime([dist]);
  const srcAge = newestMtime([path.join(workspace, 'frontend', 'src')]);
  if (srcAge > distAge) {
    throw new Error(
      `[e2e] static-dist/ is OLDER than frontend/src/ — the suite would test the previous bundle,\n` +
        `      and a missing data-testid reads as a product bug rather than a stale build.\n` +
        `        static-dist:  ${new Date(distAge).toISOString()}\n` +
        `        frontend/src: ${new Date(srcAge).toISOString()}\n` +
        `      Rebuild first:  just fe-build-e2e\n`
    );
  }
}

function serverCommand(): string {
  if (process.env.BUILD_TARGET) {
    return `bash "${startScript}" "${workspace}/target/${process.env.BUILD_TARGET}/oxicloud"`;
  }

  const cargo = `bash "${startScript}" cargo run --features plugins`;
  if (process.env.E2E_BUILD === 'cargo') return cargo;

  const bin = path.join(workspace, 'target', 'debug', 'oxicloud');
  if (!fs.existsSync(bin)) return cargo;

  const binAge = fs.statSync(bin).mtimeMs;
  const srcAge = newestMtime([
    path.join(workspace, 'src'),
    path.join(workspace, 'migrations'),
    path.join(workspace, 'Cargo.toml'),
    path.join(workspace, 'Cargo.lock'),
  ]);
  if (srcAge > binAge) {
    console.log('[e2e] target/debug/oxicloud is older than the sources — rebuilding via cargo.');
    return cargo;
  }
  if (!hasPluginsFeature(bin)) {
    console.log(
      '[e2e] target/debug/oxicloud was not built with --features plugins — rebuilding via cargo.'
    );
    return cargo;
  }

  console.log(
    `[e2e] reusing target/debug/oxicloud (built ${new Date(binAge).toISOString()}). ` +
      'Set E2E_BUILD=cargo to force a rebuild.'
  );
  return `bash "${startScript}" "${bin}"`;
}

assertFreshBundle();

export default defineConfig({
  testDir: './spa',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: 1,
  reporter: process.env.CI ? [['line'], ['github'], ['html']] : [['list'], ['html']],

  globalSetup: require.resolve('./spa/global-setup'),
  globalTeardown: require.resolve('./global-teardown'),

  use: {
    baseURL: 'http://127.0.0.1:8088',
    trace: 'on-first-retry',
    headless: true,
    screenshot: 'only-on-failure',
    testIdAttribute: 'data-testid',
    // NixOS (and distros where Playwright's bundled chromium can't run) need a
    // system chromium via PW_CHROMIUM_PATH. Unset → Playwright's bundled
    // browser (CI).
    launchOptions: process.env.PW_CHROMIUM_PATH
      ? { executablePath: process.env.PW_CHROMIUM_PATH }
      : {},
  },

  projects: [
    {
      // Single project. Name reflects the mode so Playwright's output
      // (and HTML report) tells you which run you're looking at.
      name: SHARED_WORKER_MODE ? 'chromium-shared-worker' : 'chromium',
      use: { ...devices['Desktop Chrome'] },
      // Scope: per-tab mode runs everything EXCEPT the message-bus
      // specs; shared-worker mode runs ONLY those specs.
      ...(SHARED_WORKER_MODE
        ? { testMatch: SHARED_WORKER_SPECS }
        : { testIgnore: SHARED_WORKER_SPECS }),
    },
  ],

  webServer: {
    command: serverCommand(),
    url: 'http://127.0.0.1:8088/ready',
    timeout: 600_000,
    reuseExistingServer: false,
    cwd: '../..',
    stdout: 'pipe',
    stderr: 'pipe',
    env: {
      ...commonEnv,
      OXICLOUD_SERVER_PORT: '8088',
      OXICLOUD_STORAGE_PATH: './tests/e2e/storage-spa',
      // Serve the instrumented SvelteKit build (there is no legacy
      // ./static frontend anymore — retired with the scenarios
      // suite).
      OXICLOUD_STATIC_PATH: './static-dist',
      // Enable the WASM plugin runtime so the admin Plugins tab is exercisable
      // (the suite installs the example hello plugin fixture).
      OXICLOUD_ENABLE_PLUGINS: 'true',
      // OPAQUE + DPoP both inherit `migrate` and `required`
      // respectively from `tests/common/server.env` — production-
      // shape end-to-end. All specs in `spa/` drive the SPA through
      // UI interactions (page.click / page.fill / page.goto), so
      // every request lands as either an `apiFetch` call (page-
      // context, signed inline) or a browser-initiated subresource
      // (SW-signed via `service-worker.ts`). No Node-side
      // `page.request.*` in this suite — the scenarios suite that
      // needed those helpers was retired with the legacy vanilla
      // frontend.
      // Explicitly clear OXICLOUD_METRICS_LISTEN so a developer's
      // `.env` value (typical: `127.0.0.1:9090`) doesn't leak in via
      // the parent-process env Playwright merges here — the test
      // server would collide with the developer's own running
      // instance on that port and main.rs would hard-fail. Empty
      // string is the config-parser's "disabled" sentinel.
      OXICLOUD_METRICS_LISTEN: '',
      // Shared-worker-bus toggle. Spread so the key is absent (not
      // present-with-false) when disabled — the server's env parser
      // treats absence as "use the default (opt-in off)", which is
      // what we want for the per-tab run.
      ...(SHARED_WORKER_MODE ? { OXICLOUD_ENABLE_SHARED_WORKER_BUS: 'true' } : {}),
    },
  },
});
