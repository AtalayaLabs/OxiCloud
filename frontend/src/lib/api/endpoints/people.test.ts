import { beforeEach, expect, it, vi } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// Phase 4: `peopleEnabled` → `getAuthStatus` → `getSystemStatus` (generated
// SDK). The SDK call bypasses apiFetch entirely, so the `/api/auth/status`
// probe must be mocked at the SDK layer. Everything else in `people.ts`
// still routes through apiFetch, so the shared mock on top stays.
vi.mock('../generated/openapi/sdk.gen', () => ({ getSystemStatus: vi.fn() }));
import { apiFetch } from '$lib/api/client';
import { getSystemStatus } from '../generated/openapi/sdk.gen';
import { peopleEnabled, fetchPeople, fetchPersonPhotos, renamePerson } from './people';
const request = vi.mocked(apiFetch);
const sdkGetSystemStatus = vi.mocked(getSystemStatus);
beforeEach(() => {
	request.mockReset();
	sdkGetSystemStatus.mockReset();
});

it.each([true, false])(
	'uses the advertised faces capability (%s), without probing /people',
	async (enabled) => {
		sdkGetSystemStatus.mockResolvedValue({
			initialized: true,
			faces_enabled: enabled,
			admin_count: 1,
			registration_allowed: true
		} as never);
		expect(await peopleEnabled()).toBe(enabled);
		expect(sdkGetSystemStatus).toHaveBeenCalledOnce();
		// `/people` is NOT probed — the capability came from /auth/status alone.
		expect(request).not.toHaveBeenCalled();
	}
);
it('does not expose People if the server cannot advertise its availability', async () => {
	// `getAuthStatus` catches any throw and returns the "initialized, no
	// faces" fallback, so `peopleEnabled` resolves to false without
	// bubbling the server failure to the SPA.
	sdkGetSystemStatus.mockRejectedValue(new Error('503'));
	expect(await peopleEnabled()).toBe(false);
	expect(sdkGetSystemStatus).toHaveBeenCalledOnce();
	expect(request).not.toHaveBeenCalled();
});
it('lists people and photo ids and serializes renames', async () => {
	const people = [{ id: 'p', face_count: 1, is_hidden: false }];
	request.mockResolvedValueOnce(Response.json(people));
	expect(await fetchPeople()).toEqual(people);
	request.mockResolvedValueOnce(Response.json(['f']));
	expect(await fetchPersonPhotos('p')).toEqual(['f']);
	request.mockResolvedValueOnce(new Response(null, { status: 204 }));
	await renamePerson('p', null);
	expect(request).toHaveBeenLastCalledWith(
		'/api/people/p',
		expect.objectContaining({ method: 'PATCH', body: '{"name":null}' })
	);
});
