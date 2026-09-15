/**
 * `withBase` under a non-empty base path (subpath deployments). The rest of
 * the client suite runs with the default `base = ''`, which already pins the
 * identity behaviour at the root — this file mocks the base to a prefix and
 * pins the gluing rules.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

const network = vi.hoisted(() => {
	const mock = vi.fn<typeof fetch>();
	globalThis.fetch = mock;
	return mock;
});
vi.mock('$lib/auth/dpop-proof', () => ({
	buildDpopProof: vi.fn(async () => null),
	updateNonceFromResponse: vi.fn(),
	isDpopNonceChallenge: () => false
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

vi.mock('$app/paths', () => ({ base: '/oxicloud' }));

import { apiFetch, withBase } from './client';
import { renamePerson } from './generated/sdk.gen';
import { fetchAsset } from '$lib/utils/assets';

beforeEach(() => {
	network.mockReset();
	network.mockResolvedValue(Response.json({}));
});

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

describe('requests under base=/oxicloud', () => {
	it('prefixes endpoint requests and session refresh without doubling the base', async () => {
		network.mockResolvedValueOnce(new Response(null, { status: 401 }));
		await apiFetch('/oxicloud/api/people');
		expect(
			network.mock.calls.map(([input]) =>
				typeof input === 'string'
					? input
					: new URL(input instanceof URL ? input.href : input.url).pathname
			)
		).toEqual(['/oxicloud/api/people', '/oxicloud/api/auth/refresh', '/oxicloud/api/people']);
	});
	it('prefixes generated SDK operations', async () => {
		await renamePerson({ path: { id: 'person' }, body: { name: 'Alice' } });
		const [input, init] = network.mock.calls[0];
		expect(new Request(input, init).url).toBe(`${location.origin}/oxicloud/api/people/person`);
	});
	it('prefixes static assets once and rejects prefixed API paths', async () => {
		await fetchAsset('/locales/en.json');
		expect(network).toHaveBeenLastCalledWith('/oxicloud/locales/en.json', undefined);
		await fetchAsset('/oxicloud/locales/en.json');
		expect(network).toHaveBeenLastCalledWith('/oxicloud/locales/en.json', undefined);
		expect(() => fetchAsset('/oxicloud/api/files')).toThrow('API requests must use');
	});
});
