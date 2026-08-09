import { seedAdmin } from './spa/helpers';

/**
 * Legacy webServer flow only: seed the admin against the single server
 * Playwright started at :8087. The container flow seeds per-worker inside
 * the `stack` fixture instead, and does not use this global setup.
 */
export default async function globalSetup() {
  // 127.0.0.1, not `localhost`: matches the server's IPv4 bind; on CI runners
  // `localhost` resolves to ::1 (IPv6) first and the seed request is refused.
  await seedAdmin('http://127.0.0.1:8087');
}
