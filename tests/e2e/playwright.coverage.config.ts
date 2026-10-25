import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';
import { loadEnv } from './load-env';

/**
 * Coverage e2e suite — drives the **SvelteKit** SPA (built to `static-dist/`
 * with Istanbul instrumentation via `COVERAGE=1`) and collects per-test
 * `window.__coverage__` into `.nyc_output/` for an nyc report.
 *
 * TWO backends are started side-by-side (ports 8088 and 8089) because the
 * shared-worker-bus feature is opt-in: the default suite exercises the
 * production shape (opt-in OFF → per-tab mode) against 8088, while the
 * message-bus specs that assert `SharedWorkerBridge` is live run against
 * 8089 with `OXICLOUD_ENABLE_SHARED_WORKER_BUS=true`. Running the
 * opt-in flag suite-wide broke every spec that used
 * `waitForLoadState('networkidle')` — the SharedWorker's eagerly-opened
 * connection kept Playwright's "no network for 500 ms" counter from
 * firing. Isolating the opt-in behaviour to its own project keeps the
 * other 125 specs on a quiet backend.
 *
 * Both backends share one Postgres (sqlx migrations are idempotent + use
 * an advisory lock to serialise concurrent startups) but have separate
 * storage dirs — specs don't cross backends so filesystem isolation is
 * clean. `globalSetup` seeds the admin against :8088 and the shared DB
 * exposes it to :8089 too.
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

/** Shared env for both backends — everything except port, storage path,
 *  and the shared-worker-bus flag. Keeps the two webServer entries in
 *  sync so a knob added here reaches both. */
const sharedBackendEnv = {
  ...commonEnv,
  OXICLOUD_STATIC_PATH: './static-dist',
  OXICLOUD_ENABLE_PLUGINS: 'true',
  // Explicitly clear OXICLOUD_METRICS_LISTEN so a developer's `.env`
  // value (typical: `127.0.0.1:9090`) doesn't leak in via the parent-
  // process env Playwright merges here — the test server would collide
  // with the developer's own running instance on that port and main.rs
  // would hard-fail. Empty string is the config-parser's "disabled"
  // sentinel.
  OXICLOUD_METRICS_LISTEN: '',
};

/** Specs that MUST run against the shared-worker-bus-enabled backend
 *  (port 8089). Includes the fallback spec: it deletes
 *  `window.SharedWorker` via `addInitScript` and expects the FE to engage
 *  the mandatory handshake-timeout fallback even though the SERVER flag
 *  is on — i.e. the fallback test depends on the server flag being `true`
 *  so the factory would actually try shared mode in the first place. */
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
    // Project-level `baseURL` overrides this per-project (per-tab 8088
    // vs shared-worker 8089). Kept at the suite level as the sensible
    // default for anything that forgets to set it.
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
      // Default project — opts the backend OUT of shared-worker-bus so
      // the production-shape (opt-in default) is what every non-
      // message-bus spec drives. Points at the per-tab backend on
      // :8088; the project-level `baseURL` override wins over the top-
      // level `use.baseURL`.
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
        baseURL: 'http://127.0.0.1:8088',
      },
      testIgnore: SHARED_WORKER_SPECS,
    },
    {
      // Shared-worker-bus project — the Gate-4/5 specs that assert
      // `SharedWorkerBridge` is live and the fan-out/reconnect
      // properties hold. Points at the opt-in-ON backend on :8089.
      // Includes the fallback spec: it needs the server flag ON (so
      // the factory tries shared mode) and simulates browser
      // non-support via `addInitScript({ delete window.SharedWorker })`.
      name: 'chromium-shared-worker',
      use: {
        ...devices['Desktop Chrome'],
        baseURL: 'http://127.0.0.1:8089',
      },
      testMatch: SHARED_WORKER_SPECS,
    },
  ],

  webServer: [
    {
      // Per-tab backend — the default (opt-in off). Serves the main
      // :8088 suite.
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
        ...sharedBackendEnv,
        OXICLOUD_SERVER_PORT: '8088',
        OXICLOUD_STORAGE_PATH: './tests/e2e/storage-spa',
      },
    },
    {
      // Shared-worker-bus backend — opt-in flag ON. Serves the Gate-4/5
      // message-bus specs on :8089.
      //
      // Isolated storage dir so blob files don't collide with :8088;
      // DB is shared with :8088 (sqlx migrations are idempotent +
      // advisory-locked, `globalSetup` seeds TEST_ADMIN against :8088
      // and the shared DB exposes it to :8089 too).
      command: process.env.BUILD_TARGET
        ? `bash "${startScript}" "${workspace}/target/${process.env.BUILD_TARGET}/oxicloud"`
        : `bash "${startScript}" cargo run --features plugins`,
      url: 'http://127.0.0.1:8089/ready',
      timeout: 600_000,
      reuseExistingServer: false,
      cwd: '../..',
      stdout: 'pipe',
      stderr: 'pipe',
      env: {
        ...sharedBackendEnv,
        OXICLOUD_SERVER_PORT: '8089',
        OXICLOUD_STORAGE_PATH: './tests/e2e/storage-spa-shared',
        // The whole reason this second backend exists.
        OXICLOUD_ENABLE_SHARED_WORKER_BUS: 'true',
      },
    },
  ],
});
