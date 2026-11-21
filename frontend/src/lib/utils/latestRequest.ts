/**
 * Latest-wins guard for async loads (#924).
 *
 * `next()` aborts the previous in-flight request and hands out a fresh
 * controller; after every `await`, `isCurrent(ctl)` tells the caller
 * whether a newer request superseded it. Checking `isCurrent` — not just
 * relying on the abort — keeps a late response out even when a wrapper
 * resolves instead of rejecting on abort.
 */
export function latestRequest() {
	let current: AbortController | null = null;
	return {
		next(): AbortController {
			current?.abort();
			current = new AbortController();
			return current;
		},
		isCurrent(ctl: AbortController): boolean {
			return ctl === current;
		},
		cancel(): void {
			current?.abort();
			current = null;
		}
	};
}
