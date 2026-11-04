/**
 * Armed-state wording for a FILE delete confirm.
 *
 * `ConfirmButton` turns into "Really?" on the first click and acts on the
 * second. That one word is the whole confirmation, and it reads the same
 * whether the next click moves a file to the trash or destroys it — which
 * is exactly the distinction a confirm exists to draw. With
 * `features.trash` off there is no trash to move to, so the delete is
 * final.
 *
 * Call this from markup or a `$derived`, never from a plain `const` at
 * component init: it reads `serverConfig.features`, which hydrates after
 * the first paint, so a value captured once can be wrong for the whole
 * session.
 *
 * Scope is deliberately FILES. Not playlists (deleting one leaves the
 * tracks), not drives or users or app passwords — those are irreversible
 * regardless of the trash flag, so keying their wording off it would be
 * misleading. Trash-page purges are likewise always permanent and do not
 * use this.
 */
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import { t } from '$lib/i18n/index.svelte';

export function deleteConfirmLabel(): string {
	return serverConfig.features.trash
		? t('common.really', 'Really?')
		: t('common.permanently', 'Permanently?');
}
