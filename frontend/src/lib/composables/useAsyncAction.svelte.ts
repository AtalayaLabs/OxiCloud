/**
 * Reusable async-action state machine for custom modals.
 *
 * Codifies the shape every custom-content modal needs when wrapping a
 * mutating action behind a submit button: a `busy` latch while the
 * request is in flight (disable submit + input to prevent double-fire),
 * an `error` string that renders inline on failure (keeping the modal
 * open so the user can edit their input and retry without losing it),
 * and a `clearError()` the input's `oninput` can call so a stale error
 * doesn't hover over the field while the user is fixing it.
 *
 * Mirrors the state shape `dialogs.svelte.ts` already runs internally
 * for `promptDialog` callers that pass `action` — see
 * `frontend/AGENTS.md § Dialog error UX` for the full UX contract.
 * Reach for this composable in a `.svelte` component that owns its own
 * form (ShareDialog, MoveDialog, DrivePoliciesModal, QuotaEditor, …)
 * instead of hand-rolling the three bits of state and risking drift
 * across modals.
 *
 * Usage:
 *
 * ```svelte
 * <script>
 *     const asyncDlg = useAsyncAction();
 *     let email = $state('');
 *     async function submit() {
 *         const ok = await asyncDlg.run(async () => {
 *             await createShare({ email, ... });
 *         });
 *         if (ok) onClose?.();
 *     }
 * </script>
 *
 * <Modal {open} title="Share" onclose={() => !asyncDlg.busy && onClose?.()}>
 *     <input
 *         bind:value={email}
 *         disabled={asyncDlg.busy}
 *         oninput={asyncDlg.clearError}
 *         aria-invalid={asyncDlg.error ? 'true' : undefined}
 *     />
 *     {#if asyncDlg.error}<p role="alert">{asyncDlg.error}</p>{/if}
 *     <button onclick={submit} disabled={asyncDlg.busy}>Share</button>
 * </Modal>
 * ```
 *
 * `run()` returns `true` on success, `false` on reject. The caller
 * decides what to do on success (usually `onClose()`); the composable
 * owns the UX state and nothing else.
 */
import { errorMessage } from '$lib/utils/errors';

export interface AsyncAction {
	/** True while the current `run()` call is in flight. */
	readonly busy: boolean;
	/**
	 * Inline error message from the last failed `run()`. Cleared by
	 * `clearError()` or the next successful `run()`. Pass to the dialog's
	 * `aria-describedby` + a `role="alert"` paragraph for a11y.
	 */
	readonly error: string | null;
	/**
	 * Execute an async action with managed busy + error state. Returns
	 * `true` on success, `false` when the action threw. The caller
	 * typically closes the dialog on `true` and leaves it open on
	 * `false` (the inline error already renders via `error`).
	 */
	run(fn: () => Promise<void>): Promise<boolean>;
	/**
	 * Clear the inline error. Wire to the input's `oninput` so a stale
	 * "already exists" message doesn't hover over the field while the
	 * user is editing their fix.
	 */
	clearError(): void;
}

export function useAsyncAction(): AsyncAction {
	let busy = $state(false);
	let error = $state<string | null>(null);

	return {
		get busy() {
			return busy;
		},
		get error() {
			return error;
		},
		async run(fn: () => Promise<void>): Promise<boolean> {
			busy = true;
			error = null;
			try {
				await fn();
				busy = false;
				return true;
			} catch (e) {
				busy = false;
				error = errorMessage(e);
				return false;
			}
		},
		clearError() {
			if (error) error = null;
		}
	};
}
