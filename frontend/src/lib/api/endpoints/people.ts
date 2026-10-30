/** People (faces) endpoints — ported from features/library/people.js. */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import { serverConfig } from '$lib/stores/serverConfig.svelte';

export type { PersonDto as Person } from '../generated/openapi/types.gen';
import type { PersonDto as Person } from '../generated/openapi/types.gen';

/**
 * List identity clusters. The feature is gated on `OXICLOUD_ENABLE_FACES` —
 * when it is off the route 404s; callers treat that as "faces disabled".
 */
export async function fetchPeople(): Promise<Person[]> {
	const res = await apiFetch('/api/people', { credentials: 'same-origin' });
	if (!res.ok) throw new Error(`people failed: ${res.status}`);
	return (await res.json()) as Person[];
}

/**
 * Read the advertised People capability without probing a disabled route.
 * Used to reveal the People tab only when the backend can serve it. Reads
 * from `/api/config.features.faces` (hydrated at SPA boot by
 * `serverConfig.load()`), replacing the retired
 * `/api/auth/status.faces_enabled` field.
 */
export function peopleEnabled(): boolean {
	return serverConfig.features.faces === true;
}

/** File ids of the photos a person appears in. */
export async function fetchPersonPhotos(personId: string): Promise<string[]> {
	const res = await apiFetch(`/api/people/${personId}/photos`, { credentials: 'same-origin' });
	if (!res.ok) throw new Error(`person photos failed: ${res.status}`);
	return (await res.json()) as string[];
}

/** Rename a person, or pass `null` to clear the name. */
export async function renamePerson(personId: string, name: string | null): Promise<void> {
	const res = await apiFetch(`/api/people/${personId}`, {
		method: 'PATCH',
		credentials: 'same-origin',
		headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
		body: JSON.stringify({ name })
	});
	if (!res.ok) throw new Error(`rename failed: ${res.status}`);
}
