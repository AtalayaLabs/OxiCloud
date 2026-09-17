import { beforeEach, expect, it, vi } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch } from '$lib/api/client';
import { peopleEnabled, fetchPeople, fetchPersonPhotos, renamePerson } from './people';
const request = vi.mocked(apiFetch);
beforeEach(() => request.mockReset());

it.each([true, false])(
	'uses the advertised faces capability (%s), without probing /people',
	async (enabled) => {
		request.mockResolvedValue(Response.json({ initialized: true, faces_enabled: enabled }));
		expect(await peopleEnabled()).toBe(enabled);
		expect(request).toHaveBeenCalledOnce();
		expect(request).toHaveBeenCalledWith('/api/auth/status', expect.anything());
	}
);
it('does not expose People if the server cannot advertise its availability', async () => {
	request.mockResolvedValue(new Response(null, { status: 503 }));
	expect(await peopleEnabled()).toBe(false);
	expect(request).toHaveBeenCalledOnce();
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
