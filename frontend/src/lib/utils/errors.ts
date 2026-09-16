/** Error-handling helpers shared across pages and components. */
import { ApiError } from '$lib/api/client';
import { t } from '$lib/i18n/index.svelte';
import { ui } from '$lib/stores/ui.svelte';

/**
 * Human sentences for the `error_type` values the backend emits
 * (`ErrorKind::as_str`, a wire contract — see `src/domain/errors.rs`).
 *
 * Keyed on `error_type` and never on message text, per the project's error
 * contract. The server's own message is accurate but written for an operator:
 * "Drive quota exceeded: 419764746 + 207929028 > 536870912 bytes" tells a user
 * nothing they can act on. These say what happened and, where there is one,
 * what to do about it.
 *
 * Only the types a user can meet and reason about are listed. Anything else
 * falls through to the server's message, which is better than a generic
 * "something went wrong" for the cases nobody has written copy for yet.
 */
function messageForErrorType(errorType: string): string | null {
	switch (errorType) {
		case 'Quota Exceeded':
			return t(
				'errors.quota_exceeded',
				'Not enough space. Free some up, or ask an administrator to raise the quota.'
			);
		case 'Access Denied':
			return t('errors.access_denied', "You don't have permission to do that.");
		case 'Not Found':
			return t('errors.not_found', 'That item no longer exists.');
		case 'Already Exists':
		case 'Conflict':
			return t('errors.conflict', 'An item with that name is already there.');
		case 'Transient Backend':
			return t('errors.transient_backend', 'Storage is temporarily unavailable. Try again.');
		default:
			return null;
	}
}

/** Normalise an unknown thrown value into a human-readable message. */
export function errorMessage(e: unknown): string {
	if (e instanceof ApiError && e.errorType) {
		const friendly = messageForErrorType(e.errorType);
		if (friendly) return friendly;
	}
	return e instanceof Error ? e.message : String(e);
}

/**
 * Raise an error toast for a caught value — the canonical catch-block handler.
 * Replaces the repeated `ui.notify(e instanceof Error ? e.message : String(e), 'error')`.
 */
export function errorToast(e: unknown): void {
	ui.notify(errorMessage(e), 'error');
}
