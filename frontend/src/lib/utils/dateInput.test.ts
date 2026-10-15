/**
 * Tests for `expiryToIso` + `todayIso`, moved out of
 * `$lib/api/endpoints/grants.test.ts` when the helpers were extracted
 * to `$lib/utils/dateInput.ts` as generic date-picker plumbing (not
 * grant-specific).
 */
import { describe, it, expect } from 'vitest';
import { expiryToIso, todayIso } from './dateInput';

describe('expiryToIso', () => {
	it('converts a date to an ISO string at UTC midnight', () => {
		expect(expiryToIso('2030-01-02')).toBe('2030-01-02T00:00:00.000Z');
	});
	it('returns null for empty input', () => {
		expect(expiryToIso(null)).toBeNull();
		expect(expiryToIso(undefined)).toBeNull();
		expect(expiryToIso('')).toBeNull();
	});
});

describe('todayIso', () => {
	it('returns a YYYY-MM-DD string parseable by Date', () => {
		const iso = todayIso();
		expect(iso).toMatch(/^\d{4}-\d{2}-\d{2}$/);
		expect(Number.isNaN(new Date(iso).getTime())).toBe(false);
	});
});
