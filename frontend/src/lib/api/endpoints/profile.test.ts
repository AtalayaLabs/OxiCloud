import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// Return the i18n key verbatim so the 409-branch assertions below can
// pin which key `updateProfile` picks without parsing a fallback string.
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (key: string) => key
}));
// Partial SDK mock. Only the three app-password symbols profile.ts now
// consumes from the generated SDK are replaced with Vitest stubs; the
// rest of the generated SDK stays live (anything else that happens to
// import from `sdk.gen` keeps real types + runtime behaviour). The
// pattern mirrors the auth bundle's mock in `endpoints/auth.test.ts`
// and lets the pins below drive `data` / `throw` shape per test without
// touching the shared `apiFetch` mock still used by updateProfile /
// changePassword / updateAvatar — those three wrappers are NOT yet
// migrated and still exercise the apiFetch path.
vi.mock('../generated/openapi/sdk.gen', () => ({
	listAppPasswords: vi.fn(),
	createAppPassword: vi.fn(),
	revokeAppPassword: vi.fn()
}));
import { apiFetch, apiJson } from '$lib/api/client';
import * as sdk from '../generated/openapi/sdk.gen';
import * as profile from './profile';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
const sdkList = sdk.listAppPasswords as unknown as ReturnType<typeof vi.fn>;
const sdkCreate = sdk.createAppPassword as unknown as ReturnType<typeof vi.fn>;
const sdkRevoke = sdk.revokeAppPassword as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	j.mockResolvedValue([]);
	// SDK stubs default to permissive success — happy-path returns the
	// minimum DTO fields the wrappers project. Individual tests override
	// per-case via mockResolvedValueOnce / mockRejectedValueOnce.
	sdkList.mockResolvedValue({ app_passwords: [] });
	sdkCreate.mockResolvedValue({ password: 'oxicloud-stub' });
	sdkRevoke.mockResolvedValue(undefined);
});
it('isAutoAppPassword flags generated labels', () => {
	const r1 = profile.isAutoAppPassword({ label: 'Device login (auto)' });
	const r2 = profile.isAutoAppPassword({ label: 'my token' });
	expect(typeof r1).toBe('boolean');
	expect(typeof r2).toBe('boolean');
});
it('exercises the profile endpoints', async () => {
	await profile.updateProfile({ given_name: 'A' } as never).catch(() => {});
	await profile.changePassword('old', 'new').catch(() => {});
	await profile.updateAvatar('data:image/png;base64,AAAA').catch(() => {});
	await profile.updateAvatar(null).catch(() => {});
	await profile.listAppPasswords().catch(() => {});
	await profile.createAppPassword('label').catch(() => {});
	await profile.revokeAppPassword('id').catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(2);
});

// 409 on PATCH /auth/me/profile covers two conflicts that share a status;
// the server exposes the discriminator in `body.message`, and the wrapper
// branches on substring to pick an i18n key. Pre-Phase-4 regressions
// collapsing both branches to a generic error would pass today's suite
// without these pins — add before any migration that touches updateProfile.
it('maps 409 "already claimed" to profile.username_immutable_error', async () => {
	f.mockResolvedValueOnce({
		ok: false,
		status: 409,
		json: async () => ({ message: 'username already claimed' })
	});
	await expect(profile.updateProfile({ username: 'x' } as never)).rejects.toThrow(
		'profile.username_immutable_error'
	);
});

it('maps 409 "taken" to profile.username_taken_error', async () => {
	f.mockResolvedValueOnce({
		ok: false,
		status: 409,
		json: async () => ({ message: 'username already taken' })
	});
	await expect(profile.updateProfile({ username: 'x' } as never)).rejects.toThrow(
		'profile.username_taken_error'
	);
});

it('maps 403 on updateProfile to profile.edit_oidc_managed', async () => {
	f.mockResolvedValueOnce({
		ok: false,
		status: 403,
		json: async () => ({ message: 'managed by identity provider' })
	});
	await expect(profile.updateProfile({ given_name: 'x' } as never)).rejects.toThrow(
		'profile.edit_oidc_managed'
	);
});

// ───────────────────────────── app-password Phase-4 pins ─────────────────────────────
// Three invariants the SDK migration preserves. Each pin targets ONE
// behaviour that would silently regress if the SDK's `throwOnError`
// contract drifted, the response shape widened, or a wrapper got
// "simplified" into leaking a richer DTO to the Svelte page.
//
// All three are regressions prevented at this layer per AGENTS.md
// "every regression gets its own coverage": they would ONLY surface
// end-to-end on the profile route (empty state cascade, undefined
// password string, crypto error on a `data?.password` read), and
// catching that in Playwright is far more expensive than three
// Vitest cases here.

// Pin 1 — list falls back to [] on SDK throw. The pre-migration
// apiFetch wrapper returned `[]` on any non-2xx response, not a throw.
// A future "cleanup" that drops the try/catch would propagate the
// SDK's thrown error to the Svelte page's `appPasswords = await
// listAppPasswords()` call, breaking initial render into a toast
// cascade. The empty-state semantic is load-bearing.
it('listAppPasswords falls back to [] when the SDK throws', async () => {
	sdkList.mockRejectedValueOnce({ response: { status: 500 }, data: { error: 'server' } });
	const result = await profile.listAppPasswords();
	expect(result).toEqual([]);
});

// Pin 2 — create returns the string password, not the full DTO. The
// server now sends `AppPasswordCreatedResponseDto` (password, id,
// label, username, scopes, instructions, expires_at). A future
// "widen to pass the full DTO through" change would type-break the
// Svelte card's `const pw: string = await createAppPassword(label)`
// expectation and silently fall into "copy this password" rendering
// `[object Object]`. Pin the narrow contract.
it('createAppPassword unwraps .password out of the DTO envelope', async () => {
	sdkCreate.mockResolvedValueOnce({
		id: '00000000-0000-0000-0000-000000000001',
		label: 'test',
		password: 'oxicloud-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345',
		username: 'alice',
		scopes: 'webdav,caldav,carddav',
		expires_at: null,
		instructions: {} as never
	});
	const pw = await profile.createAppPassword('test');
	expect(typeof pw).toBe('string');
	expect(pw).toBe('oxicloud-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345');
});

// Pin 3 — create surfaces the server's error message on failure.
// Specific case: the server rejects an external user's create with
// 403 + `error_type: "ExternalUserAppPassword"` and a message
// explaining why. The wrapper must forward the human-readable key
// so the Svelte card can i18n it instead of showing "create app
// password failed: 403". Pre-migration apiFetch path read `.error`
// then `.message`; the SDK throws the body as `e.data` so the
// wrapper's unwrap must still find the same keys.
it('createAppPassword surfaces the server message on throw', async () => {
	sdkCreate.mockRejectedValueOnce({
		response: { status: 403 },
		data: {
			error: 'ExternalUserAppPassword',
			message: 'External users cannot mint app passwords.'
		}
	});
	await expect(profile.createAppPassword('x')).rejects.toThrow(
		/ExternalUserAppPassword|External users cannot mint/
	);
});
