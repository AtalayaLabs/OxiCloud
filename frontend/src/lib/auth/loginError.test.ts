/**
 * Coverage for the `?login_error=<key>` → user-facing copy mapping. The
 * i18n `t()` module is stubbed to echo the fallback string verbatim so
 * the assertions read naturally without pulling in the full i18n bag.
 */
import { describe, expect, it, vi } from 'vitest';

// Stub $lib/i18n before importing the module under test — the `t`
// function in the real module needs the i18n bag to be initialized;
// here we just want to see which fallback string each case returns.
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (_key: string, ...rest: unknown[]) => {
		// Real `t` signatures: t(key, fallback) or t(key, params, fallback).
		// Whichever shape is used, the fallback is the last string arg.
		for (let i = rest.length - 1; i >= 0; i--) {
			if (typeof rest[i] === 'string') return rest[i] as string;
		}
		return _key;
	}
}));

import { loginErrorMessage } from './loginError';

describe('loginErrorMessage', () => {
	// ── OIDC-callback rejection reasons the backend emits post-refactor
	//    (project_oidc_callback_error_specific_reasons memory). These
	//    were the whole point of the refactor: distinct, targeted copy
	//    rather than the misleading "sign-in link expired" bucket.
	it('email_not_verified_at_idp → verify-at-IdP prompt', () => {
		const msg = loginErrorMessage('email_not_verified_at_idp');
		expect(msg).toMatch(/verified/i);
		expect(msg).toMatch(/identity provider/i);
	});

	it('email_verification_required → server policy + admin hint', () => {
		const msg = loginErrorMessage('email_verification_required');
		expect(msg).toMatch(/verified/i);
		expect(msg).toMatch(/administrator/i);
	});

	// ── Auto-link refusals (docs/plan/oidc-account-linking.md § Auto-link)
	it('auto_link_disabled → server-policy explanation', () => {
		expect(loginErrorMessage('auto_link_disabled')).toMatch(/auto-link/i);
	});

	it('auto_link_email_not_verified → verify-then-retry', () => {
		const msg = loginErrorMessage('auto_link_email_not_verified');
		expect(msg).toMatch(/verify|verified/i);
	});

	it('already_linked_elsewhere → admin escalation', () => {
		expect(loginErrorMessage('already_linked_elsewhere')).toMatch(/administrator/i);
	});

	it('email_ambiguous → admin escalation', () => {
		expect(loginErrorMessage('email_ambiguous')).toMatch(/administrator/i);
	});

	// ── Generic callback failure buckets — kept for backward compat +
	//    for any AccessDenied path that didn't get its own reason yet.
	it('callback_denied → link-expired copy', () => {
		expect(loginErrorMessage('callback_denied')).toMatch(/expired|already used|try/i);
	});

	it('callback_failed → generic retry', () => {
		expect(loginErrorMessage('callback_failed')).toMatch(/try again/i);
	});

	// ── Forward-compat: unknown keys never blank out. A new backend
	//    reason without an explicit case here still surfaces SOMETHING
	//    the user can act on (retry).
	it('unknown key → generic non-empty fallback', () => {
		const msg = loginErrorMessage('never_heard_of_this_reason');
		expect(msg).toBeTruthy();
		expect(msg.length).toBeGreaterThan(10);
	});

	it('empty key → generic non-empty fallback', () => {
		expect(loginErrorMessage('')).toBeTruthy();
	});
});
