import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';
import { loadEnv } from './load-env';

/**
 * Coverage e2e suite — drives the **SvelteKit** SPA (built to `static-dist/`
 * with Istanbul instrumentation via `COVERAGE=1`) and collects per-test
 * `window.__coverage__` into `.nyc_output/` for an nyc report.
 *
 * The server is a debug `cargo run` on port 8088 serving the instrumented
 * SPA build. Production-shape end-to-end: OPAQUE + DPoP `required` are
 * both inherited from `tests/common/server.env` — the previous legacy
 * `./static` vanilla-frontend suite that overrode them off has been
 * retired.
 *
 * Build the instrumented SPA first:
 *   (cd frontend && COVERAGE=1 VITE_E2E=1 npm run build)
 */
const startScript = path.join(__dirname, 'start-server-spa.sh');
const commonEnv = loadEnv(path.join(__dirname, '../common/server.env'));
const workspace = process.env.GITHUB_WORKSPACE ?? path.join(__dirname, '../..');

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

  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],

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
    },
  },
});
