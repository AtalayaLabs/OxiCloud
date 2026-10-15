/**
 * Tests for the display helpers that moved out of
 * `$lib/api/endpoints/favorites.ts` — see that file's header comment
 * for the rationale. The three `describe` blocks below used to live
 * in `endpoints/favorites.test.ts` and were migrated verbatim.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { dateBucket, sizeBucket, typeLabel } from './resourceBuckets';

describe('dateBucket', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		vi.setSystemTime(new Date('2024-06-15T12:00:00Z'));
	});
	afterEach(() => vi.useRealTimers());

	it('buckets by recency and falls back to the year', () => {
		expect(dateBucket(Date.now())).toBeTruthy();
		expect(dateBucket(Date.now() - 3 * 86_400_000)).toBeTruthy();
		expect(dateBucket(Date.now() - 20 * 86_400_000)).toBeTruthy();
		expect(dateBucket('2019-06-15')).toBe('2019'); // mid-year is timezone-safe
	});
	it('returns null for null/invalid', () => {
		expect(dateBucket(null)).toBeNull();
		expect(dateBucket(undefined)).toBeNull();
		expect(dateBucket('not a date')).toBeNull();
	});
});

describe('sizeBucket', () => {
	it('maps byte ranges to distinct labels', () => {
		const labels = [
			sizeBucket(null),
			sizeBucket(0),
			sizeBucket(500),
			sizeBucket(50 * 1_048_576),
			sizeBucket(500 * 1_048_576),
			sizeBucket(2 * 1_073_741_824),
			sizeBucket(10 * 1_073_741_824)
		];
		expect(new Set(labels).size).toBe(labels.length); // all distinct
		labels.forEach((l) => expect(l).toBeTruthy());
	});
});

describe('typeLabel', () => {
	it('maps known categories and passes through unknown ones', () => {
		expect(typeLabel('PDF')).toBe('PDF');
		expect(typeLabel('Image')).toBeTruthy();
		expect(typeLabel('Weird')).toBe('Weird');
	});
});
