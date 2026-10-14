/**
 * Tests for `displayRole`, moved out of `$lib/api/endpoints/grants.test.ts`
 * when the helper was extracted to `$lib/utils/grantFormat.ts`.
 */
import { describe, it, expect } from 'vitest';
import { displayRole } from './grantFormat';

describe('displayRole', () => {
	it('passes through canonical roles', () => {
		expect(displayRole('owner')).toBe('owner');
		expect(displayRole('editor')).toBe('editor');
		expect(displayRole('viewer')).toBe('viewer');
	});
	it('maps legacy roles and defaults to viewer', () => {
		expect(displayRole('contributor')).toBe('editor');
		expect(displayRole('commenter')).toBe('viewer');
		expect(displayRole('mystery')).toBe('viewer');
		expect(displayRole(undefined)).toBe('viewer');
	});
});
