import * as fs from 'fs';
import * as path from 'path';
import { seedAdmin } from './helpers';

/**
 * Global setup for the SPA coverage suite:
 *  1. Wipe `.nyc_output/` so each run starts from a clean coverage slate.
 *  2. Seed the admin account against the SPA server at :8088.
 */
export default async function globalSetup() {
  const nycDir = path.join(__dirname, '..', '.nyc_output');
  fs.rmSync(nycDir, { recursive: true, force: true });
  fs.mkdirSync(nycDir, { recursive: true });

  await seedAdmin('http://127.0.0.1:8088');
}
