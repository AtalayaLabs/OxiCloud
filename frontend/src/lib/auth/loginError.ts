/**
 * Stable `?login_error=<key>` translation table.
 *
 * The OIDC callback handler (`src/interfaces/api/handlers/auth_handler.rs`)
 * emits one of these snake_case keys on any rejection redirect. This
 * module maps each to localized copy the login page renders on mount.
 *
 * Adding a new backend reason: pick a matching snake_case key in the
 * handler, add a case here + an `auth.login_error_<key>` i18n entry.
 * Unknown keys silently fall back to the generic copy — a new backend
 * reason without a FE entry surfaces something the user can act on,
 * not a blank string.
 *
 * Extracted from `routes/login/+page.svelte` so a Vitest unit can
 * exercise the mapping in isolation without a browser.
 */
import { t } from '$lib/i18n/index.svelte';

export function loginErrorMessage(key: string): string {
	switch (key) {
		case 'auto_link_disabled':
			return t(
				'auth.login_error_auto_link_disabled',
				'This server does not auto-link SSO accounts. Sign in with your existing credentials, then connect SSO from your profile.'
			);
		case 'auto_link_email_not_verified':
			return t(
				'auth.login_error_auto_link_email_not_verified',
				'Your SSO provider did not confirm your email address. Verify your email at your identity provider, then try again.'
			);
		case 'already_linked_elsewhere':
			return t(
				'auth.login_error_already_linked_elsewhere',
				'A local account with this email already exists and is linked to a different SSO identity. Contact your administrator.'
			);
		case 'email_ambiguous':
			return t(
				'auth.login_error_email_ambiguous',
				'Multiple local accounts match this email address. Contact your administrator to resolve.'
			);
		case 'callback_denied':
			return t(
				'auth.login_error_callback_denied',
				'Your sign-in link expired or was already used. Please try signing in again.'
			);
		case 'callback_failed':
			return t(
				'auth.login_error_callback_failed',
				"SSO sign-in couldn't complete. Please try again."
			);
		case 'email_not_verified_at_idp':
			return t(
				'auth.login_error_email_not_verified_at_idp',
				'Your identity provider reports that your email address is not verified. Confirm your email at your identity provider, then try signing in again.'
			);
		case 'email_verification_required':
			return t(
				'auth.login_error_email_verification_required',
				'This server requires a verified email. Your identity provider did not include an email-verification claim. Contact your administrator.'
			);
		default:
			return t('auth.login_error_generic', 'SSO sign-in was refused. Please try again.');
	}
}
