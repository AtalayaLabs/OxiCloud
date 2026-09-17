import { base } from '$app/paths';

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
