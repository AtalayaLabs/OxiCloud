/**
 * Server-configuration store — hydrated once at SPA boot from
 * `GET /api/config`.
 *
 * Exposes feature-flag and server-status snapshots that the rest of
 * the app reads reactively to enable/disable optional UI. The most
 * consequential consumer today is the message bus: `useTopic`,
 * `useFolderTopic`, and `useReconnect` all return early when
 * `serverConfig.features.message_bus === false`, so a deployment
 * with the bus disabled produces zero WS traffic from the client.
 *
 * Boot order (see `hooks.client.ts`): this store's `load()` runs
 * alongside `initI18n()` before any route mounts, guaranteeing every
 * composable reads a real value (never the pre-load defaults).
 *
 * Failure to load `/api/config` (network error, 5xx) leaves the
 * defaults in place — every feature `true`, `readonly: false`. That's
 * the pre-flag behavior; downstream WS setup then hits its own
 * failure paths (503 for the endpoint if truly disabled, circuit
 * breaker after 20 retries) instead of crashing boot. A warn line is
 * logged either way so operators can spot the failure.
 */

import log from 'loglevel';

import { fetchServerConfig } from '$lib/api/endpoints/config';
import type { ServerAuth, ServerConfig, ServerFeatures, ServerStatus } from '$lib/api/types';

/** Sensible defaults for every field. Used before `load()` resolves
 *  and as the fallback if the fetch fails — every feature enabled,
 *  server status nominal. Matches the pre-`OXICLOUD_MESSAGEBUS_ENABLE`
 *  behavior so an SPA that can't reach the endpoint still tries the
 *  same code paths it always did. */
const DEFAULT_FEATURES: ServerFeatures = {
	message_bus: true,
	trash: true,
	search: true,
	sharing: true,
	music: true,
	places: true,
	faces: false,
	video_thumbnails: true,
	external_mounts: false
};

const DEFAULT_STATUS: ServerStatus = {
	readonly: false
};

/** Pre-load default for the auth tunables. `8` matches the server's
 *  historical hardcoded floor and the current `AuthConfig` default —
 *  so a client that never loads the config still enforces the same
 *  rule the server did before the feature landed. `opaque_mode`
 *  defaults to `'off'` — matching the server's default when the
 *  variable is unset — so pre-load code paths behave as though
 *  OPAQUE isn't configured. */
const DEFAULT_AUTH: ServerAuth = {
	min_password_length: 8,
	opaque_mode: 'off'
};

const cfgLog = log.getLogger('oxi:config');

class ServerConfigStore {
	/** Server version — populated after `load()`. `null` before. */
	version = $state<string | null>(null);
	/** Feature flags. Defaults are all-enabled so pre-load code paths
	 *  don't accidentally hide UI while the fetch is in flight. */
	features = $state<ServerFeatures>({ ...DEFAULT_FEATURES });
	/** Auth-related tunables the setup / register / change-password
	 *  forms read to gate submit locally. Default matches the server's
	 *  historical floor (8) so pre-load form validation isn't accidentally
	 *  more permissive than the server. */
	auth = $state<ServerAuth>({ ...DEFAULT_AUTH });
	/** Server-status snapshot. Live changes after `load()` propagate
	 *  through the `X-Server-Status` header (see
	 *  `stores/serverStatus.svelte.ts` — separate store, updated by
	 *  `apiFetch`). This store's `server_status` reflects only the
	 *  boot snapshot; consumers that need live status should read
	 *  the other store. */
	serverStatus = $state<ServerStatus>({ ...DEFAULT_STATUS });
	/** `true` once `load()` has resolved (success OR failure). Guards
	 *  callers that want to skip work until the boot snapshot is in. */
	loaded = $state(false);

	async load(): Promise<void> {
		try {
			const cfg: ServerConfig = await fetchServerConfig();
			this.version = cfg.version;
			this.features = cfg.features;
			this.auth = cfg.auth ?? { ...DEFAULT_AUTH };
			this.serverStatus = cfg.server_status;
			cfgLog.debug('server config loaded', {
				version: cfg.version,
				message_bus: cfg.features.message_bus,
				min_password_length: this.auth.min_password_length
			});
		} catch (err) {
			// Fall through to defaults — SPA still boots. Any feature
			// actually disabled server-side will surface as a 404 at
			// call time (which is fine — that's how the guards are
			// designed to be observable).
			cfgLog.warn('server config fetch failed — using defaults', { error: err });
		} finally {
			this.loaded = true;
		}
	}
}

export const serverConfig = new ServerConfigStore();
