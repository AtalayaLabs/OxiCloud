/**
 * Vitest for `useAsyncAction`. Pins the state machine every custom-
 * modal consumer relies on — see `frontend/AGENTS.md § Dialog error UX`.
 * A regression here would silently break the "stay open on error with
 * inline message" UX across every modal built on the composable, so
 * these pins have to be unforgiving on the state transitions.
 */
import { describe, expect, it, vi } from 'vitest';

vi.mock('$lib/utils/errors', () => ({
	errorMessage: (e: unknown) => (e instanceof Error ? e.message : String(e))
}));

import { useAsyncAction } from './useAsyncAction.svelte';

describe('useAsyncAction', () => {
	it('starts with no busy and no error', () => {
		const a = useAsyncAction();
		expect(a.busy).toBe(false);
		expect(a.error).toBeNull();
	});

	it('busy flips true during the action and back to false on resolve', async () => {
		const a = useAsyncAction();
		let seenBusy = false;
		const result = a.run(async () => {
			// Capture `busy === true` while the fn is in flight. Needs a
			// microtask gap so the Promise microtask ordering doesn't
			// resolve-then-read.
			await Promise.resolve();
			seenBusy = a.busy;
		});
		expect(await result).toBe(true);
		expect(seenBusy).toBe(true);
		expect(a.busy).toBe(false);
		expect(a.error).toBeNull();
	});

	it('surfaces the error message on reject and keeps modal-open semantics', async () => {
		const a = useAsyncAction();
		const ok = await a.run(async () => {
			throw new Error('An item with that name is already there.');
		});
		// `run` returns `false` so the caller does NOT close the modal.
		expect(ok).toBe(false);
		// `error` is set to the friendly message; busy clears so the
		// user can retype.
		expect(a.error).toBe('An item with that name is already there.');
		expect(a.busy).toBe(false);
	});

	it('clears the previous error on a fresh run', async () => {
		const a = useAsyncAction();
		await a.run(async () => {
			throw new Error('bad');
		});
		expect(a.error).toBe('bad');
		// Second run with a success — error should clear even without
		// an explicit `clearError()` call, because `run()` resets at
		// its top. Mirrors the dialogs store's own reset on resolve.
		const ok = await a.run(async () => {
			/* success */
		});
		expect(ok).toBe(true);
		expect(a.error).toBeNull();
	});

	it('clearError() wipes the error, no-op when already null', async () => {
		const a = useAsyncAction();
		await a.run(async () => {
			throw new Error('oops');
		});
		expect(a.error).toBe('oops');
		a.clearError();
		expect(a.error).toBeNull();
		// Idempotent — second call doesn't throw or set anything.
		a.clearError();
		expect(a.error).toBeNull();
	});

	it('stringifies a non-Error throwable', async () => {
		// Mirror of `errorMessage`'s shape-tolerance: a `throw "literal"`
		// still yields a readable message. Not best practice to throw
		// non-Errors but the state machine must not crash on them.
		const a = useAsyncAction();
		await a.run(async () => {
			throw 'plain string';
		});
		expect(a.error).toBe('plain string');
	});
});
