import { describe, it, expect } from 'vitest';
import { isAtLeastAdmin, isOwner } from './roles';

describe('isAtLeastAdmin', () => {
	// The regression this helper exists to prevent. Three places tested
	// `role === 'admin'` to decide whether to show admin UI — the sidebar
	// link, the command palette entry and the profile page — so introducing
	// a role ABOVE admin hid the admin area from the one account that
	// certainly owns the instance.
	it('admits the owner, who outranks admin', () => {
		expect(isAtLeastAdmin('owner')).toBe(true);
		expect(isAtLeastAdmin('admin')).toBe(true);
	});

	it('refuses anyone below admin', () => {
		expect(isAtLeastAdmin('user')).toBe(false);
		expect(isAtLeastAdmin('anonymous')).toBe(false);
	});

	// A stale tab or a rolling deploy can show this bundle a role it has
	// never heard of. Resolving downward only hides UI (a reload fixes it);
	// resolving upward would unlock UI the user may not be entitled to.
	it('treats an unknown or missing role as least privileged', () => {
		expect(isAtLeastAdmin('superuser')).toBe(false);
		expect(isAtLeastAdmin('')).toBe(false);
		expect(isAtLeastAdmin(undefined)).toBe(false);
		expect(isAtLeastAdmin(null)).toBe(false);
	});
});

describe('isOwner', () => {
	it('is narrower than isAtLeastAdmin', () => {
		expect(isOwner('owner')).toBe(true);
		// An admin reaches admin surfaces but is not the owner — the
		// distinction that drives the badge and owner-only affordances.
		expect(isOwner('admin')).toBe(false);
		expect(isOwner('user')).toBe(false);
		expect(isOwner(undefined)).toBe(false);
	});
});
