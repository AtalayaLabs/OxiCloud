import * as fs from 'fs';
import * as path from 'path';
import { seedAdmin } from './helpers';

/**
 * Global setup for the SPA coverage suite:
 *  1. Wipe `.nyc_output/` so each run starts from a clean coverage slate
 *     — SKIPPED when `OXICLOUD_SHARED_WORKER_MODE=1` so the two-pass
 *     sequential run (per-tab then shared-worker, see
 *     `playwright.coverage.config.ts`) accumulates coverage across both
 *     backends instead of the second pass clobbering the first.
 *  2. Seed the admin account against the SPA server at :8088 — idempotent
 *     via 409 handling, so the second pass is a no-op on the shared DB.
 */
export default async function globalSetup() {
  const nycDir = path.join(__dirname, '..', '.nyc_output');
  if (process.env.OXICLOUD_SHARED_WORKER_MODE !== '1') {
    fs.rmSync(nycDir, { recursive: true, force: true });
    fs.mkdirSync(nycDir, { recursive: true });
  } else {
    fs.mkdirSync(nycDir, { recursive: true });
  }

  await seedAdmin('http://127.0.0.1:8088');
}
