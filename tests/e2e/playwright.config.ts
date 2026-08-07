import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';
import { loadEnv } from './load-env';

const startScript = path.join(__dirname, 'start-server.sh');

const commonEnv = loadEnv(path.join(__dirname, '../common/server.env'));

console.log(`starting playwright with env BUILD_TARGET=${process.env.BUILD_TARGET ?? "debug"}`);

const workspace=process.env.GITHUB_WORKSPACE ?? path.join(__dirname, '../..');

export default defineConfig({
  testDir: './scenarios',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: 1,
  reporter: process.env.CI ? [['line'], ['github'], ['html']] : [ ['list'], ['html']],

  globalSetup: require.resolve('./global-setup'),
  globalTeardown: require.resolve('./global-teardown'),

  use: {
    // 127.0.0.1, not `localhost`: the server binds IPv4 (127.0.0.1:8087) and
    // on CI runners `localhost` resolves to ::1 (IPv6) first, so requests get
    // ECONNREFUSED and the webServer readiness check below times out.
    baseURL: 'http://127.0.0.1:8087',
    trace: 'on-first-retry',
    headless: true,
    // take a screenshot on failure
    screenshot: 'only-on-failure',
    // Tile/file rows expose a `data-testid` of the item name (kept only in e2e
    // builds via VITE_E2E); makes getByTestId + codegen prefer stable selectors.
    testIdAttribute: 'data-testid',
  },

  expect: {
    toHaveScreenshot: { maxDiffPixelRatio: 0.01 },
  },

  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],

  webServer: {
    command: process.env.BUILD_TARGET
      ? `bash "${startScript}" "${workspace}/target/${process.env.BUILD_TARGET}/oxicloud"`
      : `bash "${startScript}" cargo run`,
    // Poll the dedicated readiness probe, not `/`: `/ready` returns 200 as soon
    // as the DB pool is live, whereas `/` depends on the SPA build being present
    // and can 404 (Playwright only treats 2xx/3xx/400-403 as "ready").
    url: 'http://127.0.0.1:8087/ready',
    timeout: 600_000,
    reuseExistingServer: false,
    cwd: '../..',
    stdout: 'inherit',
    stderr: 'inherit',
    env: {
      ...commonEnv,
      OXICLOUD_SERVER_PORT: '8087',
      OXICLOUD_STORAGE_PATH: './tests/e2e/storage',
      // Verbose startup so a CI webServer-readiness timeout shows where the
      // server stalls (DB connect, migrations, bind) instead of nothing.
      RUST_LOG: 'info,oxicloud=debug,sqlx=warn,tower_http=info',
      // OPAQUE substrate is off for the E2E suite — Hurl exercises it via
      // `tests/common/server.env`; the SPA-facing coverage suite doesn't
      // need the boot-time init nor the ~200 KiB WASM client. Blanking
      // the inherited commonEnv values takes the DI factory's
      // `effective_mode == Off` short-circuit.
      OXICLOUD_AUTH_OPAQUE_MODE: 'off',
      OXICLOUD_AUTH_OPAQUE_SERVER_SETUP: '',
    },
  },
});
