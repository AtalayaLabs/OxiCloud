import { base, resolve } from '$app/paths';
import type { Pathname } from '$app/types';

/**
 * The app-route pathname of a document pathname: strips the deployment
 * base path (`kit.paths.base`) so the result can be compared against
 * route-style literals (`/login`, `/admin`, `PUBLIC_PREFIXES`) and safely
 * re-prefixed later via `resolve()`/`goto`. Without this, every such
 * comparison silently fails under a subpath deployment — e.g. the auth
 * guard treats `/oxicloud/login` as a protected route and bounces it to
 * `/login?redirect=…` in an endless navigation loop.
 *
 * Identity at the root (`base === ''`) and for pathnames outside the base.
 */
export function appPath(pathname: string): string {
	if (base && (pathname === base || pathname.startsWith(`${base}/`))) {
		return pathname.slice(base.length) || '/';
	}
	return pathname;
}

/**
 * `resolve()` for a route chosen at RUNTIME rather than written as a literal.
 *
 * `resolve` is overloaded once per route so that a literal gets its params
 * type-checked. That is the right trade for `resolve('/files/[...path]')`,
 * but it means TypeScript has to match a call against every overload — and it
 * gives up once the union grows past an internal limit. Adding the `/player`
 * route crossed that line, and five unrelated call sites that pass a
 * `Pathname` variable (post-login redirect targets, the hash-URL migration)
 * stopped compiling at once.
 *
 * So the cast lives here, with the explanation, instead of being repeated at
 * each site with none. It casts the FUNCTION rather than the argument, which
 * sidesteps overload resolution altogether — casting the argument (to `never`
 * or to one route) still leaves TypeScript picking among the overloads, and
 * it picks badly. The value is a real `Pathname` either way, which is what
 * the signature actually requires.
 */
export function resolveDynamic(path: Pathname): string {
	return (resolve as (p: Pathname) => string)(path);
}

/** Prefix root-relative paths once for subpath deployments. */
export function withBase(path: string): string {
	if (!base || !path.startsWith('/') || path === base || path.startsWith(`${base}/`)) {
		return path;
	}
	return base + path;
}
