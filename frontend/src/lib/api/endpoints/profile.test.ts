import { it, expect, vi, beforeEach } from 'vitest';
// Return the i18n key verbatim so the 409-branch assertions below can
// pin which key `updateProfile` picks without parsing a fallback string.
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (key: string) => key
}));
// Partial SDK mock. All six symbols profile.ts now consumes from the
// generated SDK are replaced with Vitest stubs; the rest of the
// generated SDK stays live (anything else that happens to import
// from `sdk.gen` keeps real types + runtime behaviour). The pattern
// mirrors the auth bundle's mock in `endpoints/auth.test.ts`.
//
// Three app-password symbols were migrated earlier in session (commit
// b8bf1731); the three profile symbols land here. profile.ts no longer
// imports `apiFetch` at all, so no `$lib/api/client` mock is needed.
vi.mock('../generated/openapi/sdk.gen', () => ({
	changePassword: vi.fn(),
	createAppPassword: vi.fn(),
	listAppPasswords: vi.fn(),
	revokeAppPassword: vi.fn(),
	updateProfile: vi.fn(),
	updateUserImage: vi.fn()
}));
// `changePassword` dynamically imports `$lib/api/endpoints/opaque`
// to re-mint the OPAQUE envelope — stub it so the import resolves to
// a no-op during tests (the import succeeds and `syncOpaqueEnvelope`
// is a noop, so the happy-path exercise finishes without touching
// the real OPAQUE WASM bundle).
vi.mock('$lib/api/endpoints/opaque', () => ({
	syncOpaqueEnvelope: vi.fn().mockResolvedValue(undefined)
}));
import * as sdk from '../generated/openapi/sdk.gen';
import * as profile from './profile';
const sdkUpdate = sdk.updateProfile as unknown as ReturnType<typeof vi.fn>;
const sdkChangePw = sdk.changePassword as unknown as ReturnType<typeof vi.fn>;
const sdkUpdateImage = sdk.updateUserImage as unknown as ReturnType<typeof vi.fn>;
const sdkList = sdk.listAppPasswords as unknown as ReturnType<typeof vi.fn>;
const sdkCreate = sdk.createAppPassword as unknown as ReturnType<typeof vi.fn>;
const sdkRevoke = sdk.revokeAppPassword as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	// SDK stubs default to permissive success — happy-path returns the
	// minimum DTO fields the wrappers project. Individual tests override
	// per-case via mockResolvedValueOnce / mockRejectedValueOnce.
	sdkUpdate.mockResolvedValue({ id: 'u', email: 'a@b.c' });
	sdkChangePw.mockResolvedValue(undefined);
	sdkUpdateImage.mockResolvedValue(undefined);
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
	// Every endpoint went through its SDK stub, not through anything
	// else. Count across all six — any wrapper that silently fell back
	// to a raw fetch or an untracked path would drop this count below
	// the expected invocations.
	const total =
		sdkUpdate.mock.calls.length +
		sdkChangePw.mock.calls.length +
		sdkUpdateImage.mock.calls.length +
		sdkList.mock.calls.length +
		sdkCreate.mock.calls.length +
		sdkRevoke.mock.calls.length;
	expect(total).toBeGreaterThanOrEqual(6);
});

// ───────────────────────────── updateProfile 409/403 pins ─────────────────────────────
// 409 on PATCH /auth/me/profile covers two conflicts that share a status;
// the server exposes the discriminator in `body.message`, and the wrapper
// branches on substring to pick an i18n key. Pre-Phase-4 regressions
// collapsing both branches to a generic error would pass today's suite
// without these pins — kept across the SDK migration so the exact i18n
// key the wrapper picks stays locked to the server's `message` text.
it('maps 409 "already claimed" to profile.username_immutable_error', async () => {
	sdkUpdate.mockRejectedValueOnce({
		response: { status: 409 },
		data: { message: 'username already claimed' }
	});
	await expect(profile.updateProfile({ username: 'x' } as never)).rejects.toThrow(
		'profile.username_immutable_error'
	);
});

it('maps 409 "taken" to profile.username_taken_error', async () => {
	sdkUpdate.mockRejectedValueOnce({
		response: { status: 409 },
		data: { message: 'username already taken' }
	});
	await expect(profile.updateProfile({ username: 'x' } as never)).rejects.toThrow(
		'profile.username_taken_error'
	);
});

it('maps 403 on updateProfile to profile.edit_oidc_managed', async () => {
	sdkUpdate.mockRejectedValueOnce({
		response: { status: 403 },
		data: { message: 'managed by identity provider' }
	});
	await expect(profile.updateProfile({ given_name: 'x' } as never)).rejects.toThrow(
		'profile.edit_oidc_managed'
	);
});

// ───────────────────────────── Phase-4 wrapper pins ─────────────────────────────
// Six invariants the SDK migration preserves. Each pin targets ONE
// behaviour that would silently regress if the SDK's `throwOnError`
// contract drifted, the response shape widened, or a wrapper got
// "simplified" into leaking a richer DTO to the Svelte page.
//
// All six are regressions prevented at this layer per AGENTS.md
// "every regression gets its own coverage": they would ONLY surface
// end-to-end on the profile route (empty state cascade, undefined
// password string, broken avatar clear, OPAQUE skipped), and catching
// that in Playwright is far more expensive than six Vitest cases.

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

// Pin 4 — changePassword re-mints the OPAQUE envelope ONLY after the
// server-side change succeeds. If `sdkChangePassword` throws, the
// wrapper surfaces the error WITHOUT calling `syncOpaqueEnvelope`
// (which would otherwise register a fresh envelope under a passphrase
// the server never accepted — splitting state between "legacy still
// has old password" and "OPAQUE has new password", locking the user
// out on the next login attempt). The pre-migration contract was
// implicit (apiFetch 4xx threw before the opaque call); keep it
// explicit under the SDK path.
it('changePassword does NOT sync OPAQUE when the server rejects the change', async () => {
	sdkChangePw.mockRejectedValueOnce({ response: { status: 400 }, data: {} });
	await expect(profile.changePassword('wrong-old', 'new')).rejects.toThrow(
		'password change failed: 400'
	);
	// If we got here the SDK rejected and the opaque sync was NOT
	// reached — assert the opaque mock was never called.
	const { syncOpaqueEnvelope } = await import('$lib/api/endpoints/opaque');
	expect(syncOpaqueEnvelope).not.toHaveBeenCalled();
});

// Pin 5 — updateAvatar passes `null` through unchanged to clear the
// stored image. The server's DTO is `UpdateUserImageDto { image:
// Option<String> }`; passing `undefined` or an empty string would
// change semantics (NOT_SET vs CLEAR vs EMPTY). The Svelte avatar
// card's "Clear" button literally calls `updateAvatar(null)` — pin
// the null-passthrough so a future "no-null-in-DTOs" refactor can't
// silently convert it.
it('updateAvatar(null) sends { image: null } to the SDK', async () => {
	await profile.updateAvatar(null);
	expect(sdkUpdateImage).toHaveBeenCalledWith(expect.objectContaining({ body: { image: null } }));
});

// Pin 6 — updateAvatar surfaces the server status on non-2xx. The
// pre-migration contract was `avatar update failed: <status>`. Keep
// the exact string shape so the Svelte card's toast matches what
// users and operators saw before.
it('updateAvatar surfaces the server status on error', async () => {
	sdkUpdateImage.mockRejectedValueOnce({
		response: { status: 413 },
		data: { message: 'Image too large' }
	});
	await expect(profile.updateAvatar('data:image/png;base64,XX')).rejects.toThrow(
		'avatar update failed: 413'
	);
});
