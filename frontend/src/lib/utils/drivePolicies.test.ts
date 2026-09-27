import { describe, it, expect, vi } from 'vitest';

// The defs call `t()` for labels; the resolver needs no locale loaded for
// these assertions, so stub it to the fallback it is always given.
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (_k: string, a?: unknown, b?: unknown) => (typeof a === 'string' ? a : (b as string)) ?? _k
}));

import {
	isDefaultable,
	isPolicyImplied,
	policyControl,
	policyDefs,
	readAllPolicies,
	readPolicyDays
} from './drivePolicies';

const def = (key: string) => policyDefs.find((d) => d.key === key)!;

describe('policy knob declarations', () => {
	it('treats the day cap as a scalar and everything else as a checkbox', () => {
		expect(policyControl(def('max_public_link_days'))).toBe('days');
		expect(policyControl(def('forbid_public_links'))).toBe('bool');
		expect(policyControl(def('require_public_link_password'))).toBe('bool');
	});

	it('excludes read_only from defaults for both kinds', () => {
		// An operational state, not a standing posture: a default that froze
		// every drive from one toggle has no legitimate use.
		expect(isDefaultable(def('read_only'), 'personal')).toBe(false);
		expect(isDefaultable(def('read_only'), 'shared')).toBe(false);
	});

	it('excludes the owner-roster lock from personal defaults only', () => {
		// Membership on a personal drive is immutable regardless — the guard
		// fires before any policy is read, so comparing it would produce
		// findings nobody can act on.
		expect(isDefaultable(def('forbid_owner_role_change'), 'personal')).toBe(false);
		expect(isDefaultable(def('forbid_owner_role_change'), 'shared')).toBe(true);
	});

	it('keeps every other knob defaultable for both kinds', () => {
		for (const d of policyDefs) {
			if (d.key === 'read_only' || d.key === 'forbid_owner_role_change') continue;
			expect(isDefaultable(d, 'personal')).toBe(true);
			expect(isDefaultable(d, 'shared')).toBe(true);
		}
	});
});

describe('isPolicyImplied', () => {
	const vals = (over: Record<string, unknown> = {}) =>
		({ ...readAllPolicies({}), ...over }) as ReturnType<typeof readAllPolicies>;

	it('greys a direct child when its parent is on', () => {
		const v = vals({ forbid_public_links: true });
		expect(isPolicyImplied(def('max_public_link_days'), v)).toBe(true);
		expect(isPolicyImplied(def('require_public_link_password'), v)).toBe(true);
	});

	it('greys a GRANDchild — the chain is walked, not one link of it', () => {
		// `forbid_sharing` covers public links, which cover the two link
		// refinements. Checking a single level would leave the cap and the
		// password requirement enabled while `forbid_public_links` still
		// reads false in the stored bag — offering an admin a control that
		// can no longer do anything.
		const v = vals({ forbid_sharing: true });
		expect(isPolicyImplied(def('forbid_public_links'), v)).toBe(true);
		expect(isPolicyImplied(def('max_public_link_days'), v)).toBe(true);
		expect(isPolicyImplied(def('require_public_link_password'), v)).toBe(true);
	});

	it('leaves unrelated knobs alone', () => {
		const v = vals({ forbid_sharing: true });
		expect(isPolicyImplied(def('read_only'), v)).toBe(false);
		expect(isPolicyImplied(def('include_in_photo_index'), v)).toBe(false);
	});

	it('does not treat a non-zero day cap as an active parent', () => {
		// The value map carries a number for this knob; truthiness would
		// make any cap read as "parent on" for anything chained to it.
		const v = vals({ max_public_link_days: 30 });
		expect(isPolicyImplied(def('require_public_link_password'), v)).toBe(false);
	});
});

describe('readPolicyDays', () => {
	it('reads a positive integer', () => {
		expect(readPolicyDays({ max_public_link_days: 30 }, 'max_public_link_days')).toBe(30);
	});

	it('treats absent, zero and negative as no cap', () => {
		// `null` (no cap) must stay distinct from `0`, which would mean
		// "expire immediately" — a value the editor should never produce.
		expect(readPolicyDays({}, 'max_public_link_days')).toBeNull();
		expect(readPolicyDays({ max_public_link_days: 0 }, 'max_public_link_days')).toBeNull();
		expect(readPolicyDays({ max_public_link_days: -5 }, 'max_public_link_days')).toBeNull();
	});

	it('survives a malformed bag rather than throwing', () => {
		// The column is a permissive JSONB bag; a bad value must not break
		// the editor, matching readPolicyBool's lenient contract.
		expect(readPolicyDays({ max_public_link_days: 'thirty' }, 'max_public_link_days')).toBeNull();
		expect(readPolicyDays({ max_public_link_days: NaN }, 'max_public_link_days')).toBeNull();
	});
});

describe('readAllPolicies', () => {
	it('narrows per control kind, not per key name', () => {
		const got = readAllPolicies({
			forbid_public_links: true,
			max_public_link_days: 7
		});
		expect(got.forbid_public_links).toBe(true);
		expect(got.max_public_link_days).toBe(7);
		// Unset boolean reads as false; unset scalar reads as null. Collapsing
		// the two would make "no cap" indistinguishable from "cap of 0".
		expect(got.require_public_link_password).toBe(false);
	});

	it('gives every declared knob a value', () => {
		const got = readAllPolicies({}) as Record<string, unknown>;
		for (const d of policyDefs) {
			expect(Object.hasOwn(got, d.key)).toBe(true);
		}
	});
});
