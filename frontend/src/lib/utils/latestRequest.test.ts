import { describe, expect, it } from 'vitest';
import { latestRequest } from './latestRequest';

// #924: share page, command palette and shared-with-me let a slow, older
// response overwrite a newer one (list + URL jumped back to folder A).
// These pin the guard those pages now route every load through.
describe('latestRequest', () => {
	it('a newer request supersedes and aborts the older one', () => {
		const loads = latestRequest();
		const a = loads.next();
		const b = loads.next();
		expect(loads.isCurrent(a)).toBe(false);
		expect(a.signal.aborted).toBe(true);
		expect(loads.isCurrent(b)).toBe(true);
		expect(b.signal.aborted).toBe(false);
	});

	it('cancel aborts the in-flight request and makes it stale', () => {
		const loads = latestRequest();
		const a = loads.next();
		loads.cancel();
		expect(a.signal.aborted).toBe(true);
		expect(loads.isCurrent(a)).toBe(false);
	});
});
