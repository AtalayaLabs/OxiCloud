/** Profile / account endpoints — ported from views/profile/profile.js. */
import type { SelfUser } from '$lib/api/types';
import { t } from '$lib/i18n/index.svelte';
import {
	changePassword as sdkChangePassword,
	createAppPassword as sdkCreateAppPassword,
	listAppPasswords as sdkListAppPasswords,
	revokeAppPassword as sdkRevokeAppPassword,
	updateProfile as sdkUpdateProfile,
	updateUserImage as sdkUpdateUserImage
} from '../generated/openapi/sdk.gen';
import type { AppPasswordSummaryDto, UpdateProfileDto } from '../generated/openapi/types.gen';

export async function updateProfile(patch: UpdateProfileDto): Promise<SelfUser> {
	// Phase 4 — SDK call. The pre-migration contract branched on three
	// response statuses to pick an i18n key:
	//   * 409 "already claimed" → profile.username_immutable_error
	//   * 409 "taken"           → profile.username_taken_error
	//   * 403                   → profile.edit_oidc_managed
	//   * everything else       → raw message
	//
	// The three Vitest pins at `profile.test.ts` for these branches
	// still cover the exact i18n-key selection, so swapping the
	// transport underneath must preserve byte-identical mapping.
	//
	// SDK throws the parsed body on non-2xx (same pattern as
	// `endpoints/auth.ts`), so we read `.response.status` + `.data`
	// off the thrown object directly — no second JSON parse.
	try {
		const data = await sdkUpdateProfile({
			body: patch as never,
			throwOnError: true
		});
		return data as SelfUser;
	} catch (e) {
		const err = e as {
			response?: { status?: number };
			data?: { message?: string; error?: string };
		};
		const status = err.response?.status ?? 0;
		const msg = (err.data?.message || err.data?.error || '').toLowerCase();
		if (status === 409) {
			const key = msg.includes('already claimed')
				? 'profile.username_immutable_error'
				: 'profile.username_taken_error';
			const fallback = msg.includes('already claimed')
				? "Your username has already been set and can't be changed."
				: 'That username is already taken.';
			throw new Error(t(key, fallback), { cause: e });
		}
		if (status === 403) {
			throw new Error(
				t(
					'profile.edit_oidc_managed',
					'Your profile is managed by your identity provider. Update it there; changes appear on your next sign-in.'
				),
				{ cause: e }
			);
		}
		throw new Error(err.data?.message || err.data?.error || `profile update failed: ${status}`, {
			cause: e
		});
	}
}

export async function changePassword(currentPw: string, newPw: string): Promise<void> {
	// Phase 4 — SDK call. Preserves the pre-migration contract: on
	// non-2xx, throw a simple `"password change failed: <status>"`
	// error (no body-shape branching here — the FE's `/profile` page
	// surfaces the raw error to the user via its own toast layer).
	//
	// The OPAQUE re-mint below runs ONLY after the server-side
	// change-password commits — same ordering as before. Session is
	// preserved by the backend's `revoke_other_user_sessions` so the
	// opaque sync's session-authenticated register endpoints are
	// reachable straight away.
	try {
		await sdkChangePassword({
			body: { current_password: currentPw, new_password: newPw },
			throwOnError: true
		});
	} catch (e) {
		const err = e as { response?: { status?: number } };
		throw new Error(`password change failed: ${err.response?.status ?? 0}`, { cause: e });
	}
	// Re-mint the OPAQUE envelope under the new passphrase — SAME
	// session is still valid after change_password (the backend now
	// preserves the caller's session via `revoke_other_user_sessions`;
	// only OTHER devices are logged out). That means the session-
	// authenticated register endpoints are reachable straight away,
	// no 401 race like the earlier `revoke_all_user_sessions` shape.
	//
	// This is the PRIMARY migration path: the envelope transitions
	// straight from OLD-password bound to NEW-password bound with no
	// null intermediate. `opaque_migrated_at` stays intact, admin
	// dashboards don't see a spurious "unmigrated" blip. Non-fatal
	// on failure — silent-migration on next legacy login (post
	// `oxicloud-cli opaque reset` recovery) is the fallback.
	//
	// Dynamic import keeps the ~200 KiB `@serenity-kit/opaque` WASM
	// bundle out of the profile route's initial chunk — the module
	// only loads for users who actually reach the change-password
	// success path.
	const { syncOpaqueEnvelope } = await import('$lib/api/endpoints/opaque');
	await syncOpaqueEnvelope(newPw);
}

export async function updateAvatar(image: string | null): Promise<void> {
	// Phase 4 — SDK call. Prior to this commit the handler at
	// `src/interfaces/api/handlers/auth_handler.rs:987` had a literal
	// `// TODO: add utoipa` comment and the SDK carried no generated
	// function for `/api/auth/me/image`; this migration ships the
	// utoipa annotation + SDK regen alongside the FE flip so all three
	// profile wrappers land in one commit.
	//
	// Null image clears the stored avatar — the server treats the
	// field uniformly (`UpdateUserImageDto { image: Option<String> }`)
	// so we pass it through. Response is an empty 200; the Svelte
	// profile page re-fetches `GET /me` on next session.load tick.
	try {
		await sdkUpdateUserImage({
			body: { image },
			throwOnError: true
		});
	} catch (e) {
		const err = e as { response?: { status?: number } };
		throw new Error(`avatar update failed: ${err.response?.status ?? 0}`, { cause: e });
	}
}

/**
 * Labels the server uses for sessions auto-generated when a Nextcloud-style
 * client authenticates (vs. user-created app passwords). Ported from
 * `views/profile/profile.js`'s `AUTO_LABELS`.
 */
const AUTO_LABELS = ['Nextcloud', 'Nextcloud (OIDC)'];

/** True when an app password was auto-generated by a client session login. */
export function isAutoAppPassword(pw: Pick<AppPasswordSummaryDto, 'label'>): boolean {
	return AUTO_LABELS.includes(pw.label);
}

export async function listAppPasswords(): Promise<AppPasswordSummaryDto[]> {
	// Phase 4 — SDK call. Swallow every error path to `[]` to preserve
	// the pre-migration contract (the apiFetch wrapper returned `[]` on
	// any non-2xx — not a `throw` — so the profile page can render the
	// empty state without a toast cascade on transient failures).
	//
	// Returns the SDK's `AppPasswordSummaryDto[]` directly — the
	// pre-cleanup local `AppPassword` interface was a strict subset
	// (dropped `prefix`, `scopes`, `expires_at`); the SDK type is
	// strictly more informative. The Svelte card still reads the same
	// four fields it did before (`id`, `label`, `created_at`,
	// `last_used_at`); `active` moves from optional to required
	// (always-present on the wire DTO), which is a tightening the
	// card already handles correctly.
	try {
		const data = await sdkListAppPasswords({ throwOnError: true });
		return data.app_passwords;
	} catch {
		return [];
	}
}

/** Returns the one-time generated password (shown once). */
export async function createAppPassword(label: string): Promise<string> {
	// Phase 4 — SDK call. Server returns `AppPasswordCreatedResponseDto`
	// which is a rich envelope (`password`, `username`, `scopes`,
	// `instructions`, `id`, `label`, `expires_at`). The pre-migration
	// contract exposed ONLY `.password` as a plain string, so the Svelte
	// page's "copy this once" card is unaware of the richer shape.
	//
	// Preserving that narrow contract on purpose: the Svelte card today
	// only renders the password + a copy button. Widening to pass the
	// full DTO would churn the UI without unlocking anything the admin
	// Hurl pin at `tests/api/app_passwords.hurl` hasn't already covered.
	// If the Svelte card ever grows an "advanced details" expander for
	// username / scopes / instructions, flip this to return the full
	// DTO (type is already in the SDK as `AppPasswordCreatedResponseDto`).
	//
	// Error mapping mirrors the pre-migration apiFetch path: the SDK
	// throws the PARSED body on non-2xx (same `error_type` / `message`
	// shape as `ErrorResponseDto`), so we read it directly off `e.data`
	// without a second JSON parse.
	try {
		const data = await sdkCreateAppPassword({ body: { label }, throwOnError: true });
		return data.password;
	} catch (e) {
		const err = e as {
			response?: { status?: number };
			data?: { error?: string; message?: string };
		};
		const msg = err.data?.error || err.data?.message;
		throw new Error(msg || `create app password failed: ${err.response?.status ?? 0}`, {
			cause: e
		});
	}
}

export async function revokeAppPassword(id: string): Promise<void> {
	// Phase 4 — SDK call. Server returns `AppPasswordRevokeResponseDto`
	// (`{status: "revoked", id}`); the wrapper ignores it because the
	// Svelte page just re-fetches the list after a successful revoke.
	try {
		await sdkRevokeAppPassword({ path: { id }, throwOnError: true });
	} catch (e) {
		const err = e as { response?: { status?: number } };
		throw new Error(`revoke app password failed: ${err.response?.status ?? 0}`, {
			cause: e
		});
	}
}
