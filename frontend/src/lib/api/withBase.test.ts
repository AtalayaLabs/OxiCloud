/**
 * `withBase` under a non-empty base path (subpath deployments). The rest of
 * the client suite runs with the default `base = ''`, which already pins the
 * identity behaviour at the root — this file mocks the base to a prefix and
 * pins the gluing rules.
 */
import { describe, expect, it, vi } from 'vitest';

vi.mock('$app/paths', () => ({ base: '/oxicloud' }));

import { withBase } from './client';

describe('withBase under base=/oxicloud', () => {
	it('prefixes root-relative paths', () => {
		expect(withBase('/api/files/123')).toBe('/oxicloud/api/files/123');
		expect(withBase('/login?source=session_expired')).toBe(
			'/oxicloud/login?source=session_expired'
		);
	});

	it('never double-prefixes', () => {
		expect(withBase('/oxicloud/api/files/123')).toBe('/oxicloud/api/files/123');
		expect(withBase('/oxicloud')).toBe('/oxicloud');
	});

	it('leaves absolute URLs untouched', () => {
		expect(withBase('https://example.com/api/x')).toBe('https://example.com/api/x');
	});

	it('does not treat a lookalike prefix as already prefixed', () => {
		// `/oxicloudfoo` is a DIFFERENT root-relative path, not the base —
		// it must still be prefixed.
		expect(withBase('/oxicloudfoo/api')).toBe('/oxicloud/oxicloudfoo/api');
	});
});
