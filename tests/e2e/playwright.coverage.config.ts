import { defineConfig, devices } from '@playwright/test';
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
    command: process.env.BUILD_TARGET
      ? `bash "${startScript}" "${workspace}/target/${process.env.BUILD_TARGET}/oxicloud"`
      : `bash "${startScript}" cargo run --features plugins`,
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
