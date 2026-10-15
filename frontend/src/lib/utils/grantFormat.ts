/**
 * Grant / share display helpers.
 *
 * Lives in `utils/` rather than next to the grants HTTP wrappers in
 * `$lib/api/endpoints/grants.ts`: no network, pure role-string
 * mapping. Phase 4 of the OpenAPI migration retires the endpoints
 * wrapper; having this helper outside that tree means the retirement
 * doesn't take the role-collapse logic with it.
 */
import type { ShareRole } from '$lib/api/endpoints/grants';

/**
 * Map a backend role string to the role the UI exposes. The server may emit the
 * full enum (`owner`/`editor`/`viewer`/`commenter`/`contributor`); the picker
 * only shows Owner/Editor/Viewer, so collapse the two unexposed roles to their
 * closest neighbour rather than render an unknown option.
 */
export function displayRole(role: string | undefined): ShareRole {
	if (role === 'owner' || role === 'editor' || role === 'viewer') return role;
	if (role === 'contributor') return 'editor';
	if (role === 'commenter') return 'viewer';
	return 'viewer';
}
