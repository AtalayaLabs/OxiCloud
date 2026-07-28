import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';
import { loadEnv } from './load-env';

/**
 * Coverage e2e suite — drives the **SvelteKit** SPA (built to `static-dist/`
 * with Istanbul instrumentation via `COVERAGE=1`) and collects per-test
 * `window.__coverage__` into `.nyc_output/` for an nyc report.
 *
 * This is separate from `playwright.config.ts` (which exercises the legacy
 * `./static` vanilla frontend). The server here is a debug `cargo run` with
 * `OXICLOUD_STATIC_PATH=./static-dist`, so the SPA — not the legacy app — is
 * served on :8088.
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
    // browser (CI). Mirrors playwright.containers.config.ts.
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
    stdout: 'inherit',
    stderr: 'inherit',
    env: {
      ...commonEnv,
      OXICLOUD_SERVER_PORT: '8088',
      OXICLOUD_STORAGE_PATH: './tests/e2e/storage-spa',
      // Serve the instrumented SvelteKit build, not the legacy ./static app.
      OXICLOUD_STATIC_PATH: './static-dist',
      // Enable the WASM plugin runtime so the admin Plugins tab is exercisable
      // (the suite installs the example hello plugin fixture).
      OXICLOUD_ENABLE_PLUGINS: 'true',
      // Explicitly disable OPAQUE for the SPA coverage suite. The Hurl API
      // suite (tests/common/server.env) exercises the OPAQUE substrate; this
      // suite is scoped to SPA UI coverage and doesn't need the ~200 KiB
      // WASM bundle nor the boot-time substrate init. Blank the two OPAQUE
      // env vars inherited from commonEnv so the DI factory takes the
      // `effective_mode == Off` short-circuit path.
      OXICLOUD_OPAQUE_MODE: 'off',
      OXICLOUD_OPAQUE_SERVER_SETUP: '',
    },
  },
});
