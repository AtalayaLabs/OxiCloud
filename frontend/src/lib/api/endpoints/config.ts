/**
 * `GET /api/config` — public server-configuration discovery.
 *
 * Called once at SPA boot from `hooks.client.ts` to hydrate the
 * `serverConfig` reactive store. Feature flags and server-status live
 * side-by-side on the response so a single round-trip primes the FE
 * for the whole session. Subsequent live status changes propagate
 * through the `X-Server-Status` response header (same shape).
 *
 * Unauthenticated — no session cookie required. Nothing on this
 * endpoint is per-user or privacy-sensitive.
 */

import { apiJson } from '$lib/api/client';
import type { ServerConfig } from '$lib/api/types';

export function fetchServerConfig(): Promise<ServerConfig> {
	return apiJson<ServerConfig>('/api/config');
}
