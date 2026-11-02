/**
 * Coverage for `errorMessage()` — the PascalCase `error_type` →
 * user-facing i18n copy mapping. Keyed on the wire `error_type` value
 * from `ErrorKind::wire_name()` (NOT on message text, per the project's
 * error contract).
 *
 * These pins exist specifically because this switch sat silently broken
 * for weeks: it was authored against `ErrorKind::as_str()` output
 * ("Quota Exceeded", "Access Denied", …) and never updated when the
 * server moved to `wire_name()` PascalCase. Every case returned `null`,
 * every toast fell through to the raw operator message
 * ("Drive quota exceeded: 419764746 + 207929028 > 536870912 bytes").
 * No existing test caught it because nothing exercised the switch; the
 * i18n keys this module declares were orphans.
 *
 * The specific-case `+page.svelte` branches (login/profile/upgrade on
 * `EmailNotVerified`, `PasswordLoginDisabled`, `AccountDeactivated`, …)
 * were and are correct — those have always been PascalCase. The gap was
 * only here, in the generic cross-page fallback.
 */
import { describe, expect, it, vi } from 'vitest';

// Stub $lib/i18n before importing — same pattern as loginError.test.ts.
// `t` echoes the fallback string so we can assert on the sentence copy
// without pulling in the full locale bag.
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (_key: string, ...rest: unknown[]) => {
		for (let i = rest.length - 1; i >= 0; i--) {
			if (typeof rest[i] === 'string') return rest[i] as string;
		}
		return _key;
	}
}));

// `errors.ts` imports the `ui` store at module scope for `errorToast`.
// We don't exercise that path here, but Vite needs the import to resolve.
vi.mock('$lib/stores/ui.svelte', () => ({
	ui: { notify: vi.fn() }
}));

import { ApiError } from '$lib/api/client';
import { errorMessage } from './errors';

const RESOURCE = '/api/dummy';

/**
 * Each row names ONE `wire_name()` value the switch handles. If a
 * reviewer adds a new case to the switch, the matching ErrorKind
 * must appear here too — these rows ARE the inventory, and
 * `expected !== fallback` is the "the switch actually fired" invariant.
 *
 * `Conflict` and `AlreadyExists` share a case arm — both must produce
 * the same conflict copy, so the pin trips if one is dropped from the
 * fall-through.
 */
const WIRE_TO_COPY: Array<{ errorType: string; expected: string }> = [
	{
		errorType: 'QuotaExceeded',
		expected: 'Not enough space. Free some up, or ask an administrator to raise the quota.'
	},
	{
		errorType: 'AccessDenied',
		expected: "You don't have permission to do that."
	},
	{
		errorType: 'NotFound',
		expected: 'That item no longer exists.'
	},
	{
		errorType: 'AlreadyExists',
		expected: 'An item with that name is already there.'
	},
	{
		errorType: 'Conflict',
		expected: 'An item with that name is already there.'
	},
	{
		errorType: 'TransientBackend',
		expected: 'Storage is temporarily unavailable. Try again.'
	}
];

describe('errorMessage — PascalCase error_type → user copy', () => {
	// Parameterised so each wire value is a named row in the test output;
	// a casing regression on any one of them (e.g. `TransientBackend` →
	// `transient_backend`) names which one failed, not just "the switch".
	for (const { errorType, expected } of WIRE_TO_COPY) {
		it(`${errorType} → specific friendly copy`, () => {
			const err = new ApiError(500, 'Server Error', RESOURCE, errorType, 'raw operator msg');
			expect(errorMessage(err)).toBe(expected);
		});
	}

	it('falls through to the server message for an unmapped error_type', () => {
		// `InvalidInput` is a real wire value with no custom copy — the
		// contract is that unmapped types return the server's own message
		// (better than a generic "something went wrong" for cases nobody
		// has written copy for yet). If that fallback ever flips to a
		// generic sentence, this pin fails and names the regression.
		const err = new ApiError(400, 'Bad Request', RESOURCE, 'InvalidInput', 'raw operator msg');
		expect(errorMessage(err)).toBe('raw operator msg');
	});

	it('returns the Error.message when there is no errorType at all', () => {
		// A `fetch`-level network failure never carries a wire body, so
		// errorType is undefined. The generic Error.message path has to
		// still work — if it ever did an `instanceof ApiError && !errorType`
		// → `null` early-return by mistake, this trips.
		const err = new Error('network down');
		expect(errorMessage(err)).toBe('network down');
	});

	it('stringifies a non-Error throwable', () => {
		// Not an ApiError, not an Error — just something the Promise
		// rejected with. The function has to return a string either way;
		// pin so a future refactor to tighten the type does not silently
		// break on `throw "literal"`.
		expect(errorMessage('plain string')).toBe('plain string');
	});

	it('rejects stale snake_case error_type (dead-switch regression guard)', () => {
		// THIS is the regression the whole file exists for. Pre-fix, the
		// switch cases were `'Quota Exceeded'` (space-separated), so the
		// server's PascalCase `QuotaExceeded` fell through to the raw
		// operator message. If a reviewer "cleans up" the switch to
		// space-case again, this pin fails because the mapper would
		// return the raw message (`'raw operator msg'`) instead of the
		// friendly copy.
		const err = new ApiError(
			507,
			'Insufficient Storage',
			RESOURCE,
			'QuotaExceeded',
			'raw operator msg'
		);
		expect(errorMessage(err)).not.toBe('raw operator msg');
		expect(errorMessage(err)).toContain('Not enough space');
	});
});
