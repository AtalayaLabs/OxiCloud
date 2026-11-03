import { goto } from '$app/navigation';
import { resolve } from '$app/paths';
import { logout } from '$lib/api/endpoints/auth';
import { setLogoutInProgress } from '$lib/api/client';
import { session } from '$lib/stores/session.svelte';

/**
 * Explicit sign-out, shared by the avatar dropdown and the mobile Settings
 * page. `?source=logged_out` tells the login page to show the success notice
 * and skip its pointless post-logout session probes.
 */
export async function performLogout(): Promise<void> {
	// Gate ambient requests before the POST so nothing fires between teardown
	// and the /login mount. Module state survives soft navigation, so a stale
	// reactive fetch still no-ops while the transition runs.
	setLogoutInProgress(true);
	let postLogoutUrl: string | undefined;
	try {
		({ postLogoutUrl } = await logout());
	} catch {
		/* clear locally regardless */
	}
	if (postLogoutUrl) {
		// IdP end-session: a full-page navigation; the SPA reboots on /login.
		window.location.replace(postLogoutUrl);
		return;
	}
	session.reset();
	await goto(resolve('/login?source=logged_out'));
}
