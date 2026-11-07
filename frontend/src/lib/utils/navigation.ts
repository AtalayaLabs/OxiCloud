import { resolve } from '$app/paths';
import type { Pathname, ResolvedPathname } from '$app/types';

/**
 * Resolve a runtime-known pathname through SvelteKit's base-path handling.
 *
 * `resolve()` is typed as a per-route overload set: passing the whole
 * `Pathname` union makes TypeScript distribute the argument tuple over every
 * route in the app. Once the route table grows past ~25 pathnames that
 * inference collapses and every `resolve(runtimePath)` call is rejected, even
 * though the runtime behaviour is unchanged ("prefix the base path").
 *
 * Keep literal call sites on the route-checked overloads; route them through
 * this helper only when the value comes from runtime (a query parameter, a
 * hash, an API response). The parameter stays `Pathname`, so an unknown string
 * is still caught at the call boundary.
 */
export function resolvePath(path: Pathname): ResolvedPathname {
	// @ts-expect-error union-typed Pathname overflows resolve()'s overload inference
	return resolve(path);
}
