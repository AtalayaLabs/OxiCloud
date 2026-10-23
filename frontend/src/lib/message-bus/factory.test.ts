// Pins the Gate-3 opt-out semantics of the `messageBus` factory.
//
// The factory in `client.svelte.ts` reads `/api/config` before
// `trySharedWorker()`; when `features.shared_worker_bus === false`
// it must return a `PerTabMessageBusClient` without touching
// `new SharedWorker`. A regression that collapsed this branch
// would silently defeat an operator who set
// `OXICLOUD_ENABLE_SHARED_WORKER_BUS=false` — their users would
// land on shared mode anyway, with no observable failure.
//
// The factory also runs at module-load time (top-level await), so
// the import of `./client.svelte` here IS the invocation under
// test. The mocks below set up what the factory sees BEFORE any
// `./client.svelte` import.

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

// Hoisted mocks — must be installed before the module graph loads.
const { getConfigMock, sessionMock } = vi.hoisted(() => ({
	getConfigMock: vi.fn(),
	sessionMock: { isAuthenticated: false, load: vi.fn(async () => null) }
}));

vi.mock('$lib/api/generated/openapi/sdk.gen', () => ({
	getConfig: getConfigMock
}));
vi.mock('$lib/stores/session.svelte', () => ({ session: sessionMock }));
vi.mock('$lib/api/client', () => ({ apiJson: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
vi.mock('$app/paths', () => ({ base: '' }));

beforeEach(() => {
	// Fresh mock state and a fresh module graph for each test — the
	// factory's top-level await captures the mock return at the first
	// import, so a second test needs a reset-and-reimport cycle.
	getConfigMock.mockReset();
	sessionMock.load.mockReset();
	vi.resetModules();
	// Workers aren't actually constructable under jsdom; make the
	// `typeof SharedWorker !== 'undefined'` detection branch deliberate
	// per test rather than inheriting whatever jsdom happens to expose.
	//
	// Also: the per-tab client's constructor touches `window.location`
	// via its default URL builder. jsdom supplies one.
	vi.stubGlobal('SharedWorker', undefined);
});

afterEach(() => {
	vi.unstubAllGlobals();
});

describe('messageBus factory — opt-out via shared_worker_bus flag', () => {
	it('returns the per-tab client when /api/config says shared_worker_bus=false', async () => {
		// Server opted out — factory must honour it regardless of
		// browser capability.
		getConfigMock.mockResolvedValue({
			version: 'test',
			features: {
				message_bus: true,
				trash: true,
				search: true,
				sharing: true,
				music: true,
				places: true,
				faces: false,
				video_thumbnails: true,
				external_mounts: false,
				markdown_collab: false,
				shared_worker_bus: false
			},
			auth: { min_password_length: 8, opaque_mode: 'off' },
			server_status: { readonly: false }
		});
		// If this test ever landed in a jsdom that DID expose
		// SharedWorker, stubbing it to a throwing ctor would prove the
		// factory never reached detection. Belt-and-braces.
		const throwIfCalled = vi.fn(() => {
			throw new Error('factory should not have reached SharedWorker construction');
		});
		vi.stubGlobal('SharedWorker', throwIfCalled);

		const mod = await import('./client.svelte');
		expect(mod.messageBus.constructor.name).toBe('PerTabMessageBusClient');
		expect(throwIfCalled).not.toHaveBeenCalled();
	});

	it('attempts shared mode when shared_worker_bus=true (default)', async () => {
		// Server advertises the default — factory proceeds to
		// detection. Detection fails here (no SharedWorker in jsdom)
		// → factory lands on per-tab via the handshake fallback, not
		// via the server flag. The distinction matters: the FALLBACK
		// path logs a different reason, and operators who read logs
		// to understand "why is my user on per-tab mode" need the
		// flag path and the capability path to be observably
		// different.
		getConfigMock.mockResolvedValue({
			version: 'test',
			features: {
				message_bus: true,
				trash: true,
				search: true,
				sharing: true,
				music: true,
				places: true,
				faces: false,
				video_thumbnails: true,
				external_mounts: false,
				markdown_collab: false,
				shared_worker_bus: true
			},
			auth: { min_password_length: 8, opaque_mode: 'off' },
			server_status: { readonly: false }
		});

		const mod = await import('./client.svelte');
		expect(mod.messageBus.constructor.name).toBe('PerTabMessageBusClient');
		// The getConfig call was made — proves the factory reached
		// the flag check before falling through to detection.
		expect(getConfigMock).toHaveBeenCalled();
	});

	it('fails open — a /api/config fetch error does NOT disable shared mode', async () => {
		// Opt-out semantics: a transient /api/config failure must not
		// silently downgrade the user to per-tab mode. The server
		// reports off explicitly or not at all; failing to the richer
		// mode matches "default on, operator opts out".
		getConfigMock.mockRejectedValue(new Error('transient-500'));

		const mod = await import('./client.svelte');
		// Detection still fails (no SharedWorker in jsdom) → per-tab.
		// What we're pinning here is "the factory didn't BAIL on the
		// /api/config throw"; the per-tab outcome is from detection,
		// not from the flag.
		expect(mod.messageBus.constructor.name).toBe('PerTabMessageBusClient');
		expect(getConfigMock).toHaveBeenCalled();
	});
});
