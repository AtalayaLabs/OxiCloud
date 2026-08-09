<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import type { Pathname } from '$app/types';
	import { page, updated } from '$app/state';
	import { onMount } from 'svelte';
	import '$lib/styles/app.css';
	import AppShell from '$lib/components/AppShell.svelte';
	import DialogHost from '$lib/components/DialogHost.svelte';
	import Toaster from '$lib/components/Toaster.svelte';
	import { isLogoutInProgress, setPasswordChangeRequiredHandler } from '$lib/api/client';
	import { onSessionCleared } from '$lib/auth/session-broadcast';
	import { session } from '$lib/stores/session.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { hashUrlToPath } from '$lib/utils/hashRedirect';
	import { killLegacyServiceWorker } from '$lib/utils/killLegacyServiceWorker';
	import { getOidcProviders, type OidcProviders } from '$lib/api/endpoints/auth';

	let { children } = $props();

	// A new build was deployed (`_app/version.json` changed, polled per
	// svelte.config.js) — reload so an open tab never keeps running stale code
	// after a rebuild. Deferred while a progress notification (e.g. an upload) is
	// in flight so we don't interrupt it; this effect re-runs when that clears
	// (notifications are reactive) and reloads then.
	$effect(() => {
		if (!updated.current) return;
		const busy = ui.notifications.some((n) => n.progress !== undefined);
		if (!busy) location.reload();
	});

	// Routes reachable without an authenticated session.
	const PUBLIC_PREFIXES = ['/login', '/device', '/s/', '/nextcloud'];

	function isPublic(pathname: string): boolean {
		return PUBLIC_PREFIXES.some((p) => pathname === p || pathname.startsWith(p));
	}

	let ready = $state(false);
	// Providers info fetched at boot so the guard below can enact the
	// SSO-only server-side policy (`auto_redirect_if_standalone_oidc`) on
	// SPA client-nav paths the middleware in interfaces/web/mod.rs can't
	// see. The middleware only fires on full HTTP loads to /login; when
	// the layout guard is about to `goto('/login')` we short-circuit to
	// the IdP directly if the server tells us to. Null until fetched;
	// the guard waits for it before deciding.
	let providers = $state<OidcProviders | null>(null);

	// Wire the fetch-interceptor's mandatory-mode handler to a soft
	// `goto()` so a stale-tab request that surfaces a 403
	// `PasswordChangeRequired` routes to `/profile` without a full
	// page reload — preserving the SPA session, drives cache, etc.
	// The `next=` carries the intended destination so the profile
	// page can bounce back after the change lands. Falls back to
	// `window.location` if no `next` context (see the default handler
	// in client.ts).
	setPasswordChangeRequiredHandler(() => {
		const path = page.url.pathname + page.url.search;
		void goto(resolve(`/profile?forcePasswordChange=1&next=${encodeURIComponent(path)}`), {
			replaceState: true
		});
	});

	onMount(async () => {
		// Cross-tab logout — when ANOTHER tab logs out, wipe our
		// session store and bounce to /login synchronously. Without
		// this the natural 401-on-next-request path still catches
		// it, just with visible delay for an idle tab. See
		// `docs/plan/dpop.md` Gate 8.
		//
		// The cleanup closure returned by `onSessionCleared` is
		// intentionally not wired to `onDestroy` — the root layout
		// only unmounts on hot-reload, and a leaked BroadcastChannel
		// there is far cheaper than the risk of missing an
		// invalidation event during teardown.
		onSessionCleared(() => {
			// A BroadcastChannel dispatches to every OTHER instance
			// on the same channel — including OTHER instances in the
			// SAME tab (the API only skips the exact sender instance,
			// not the whole tab). So `broadcastSessionCleared()` fired
			// from `logout()` on this tab re-enters here. When THIS
			// tab initiated the logout, `AppShell.onLogout` has already
			// navigated to `/login?source=logged_out`; running the bare
			// `/login` goto below would clobber the query string (Ed's
			// missing "Successfully signed out" banner). Only handle
			// broadcasts from OTHER tabs.
			if (isLogoutInProgress()) return;
			session.reset();
			// `replaceState: true` so the back button doesn't return
			// the user to the now-dead protected page they were on.
			void goto(resolve('/login'), { replaceState: true });
		});

		await killLegacyServiceWorker();

		// Register the DPoP-signing Service Worker (see
		// `src/service-worker.ts`). It attaches a DPoP proof to every
		// same-origin `/api/*` request the browser initiates on its
		// own — `<img src>` thumbnails, `<a href download>` downloads,
		// `EventSource` streams — replacing the server-side content
		// allowlist (Gate C in `docs/plan/dpop.md`).
		//
		// `type: 'module'` matches SvelteKit's ESM output. Failure is
		// swallowed to `console.debug`: unbound sessions and legacy
		// browsers without SW support keep working; only bound sessions
		// in `DPOP=required` mode see 401s on browser-direct URLs,
		// which is the same failure mode as before this SW existed.
		//
		// AWAIT `navigator.serviceWorker.ready` before allowing the
		// splash to lift on a first-ever visit. Without this, the SPA
		// renders while the SW is still installing → thumbnails and
		// downloads on that very first page load fire unsigned and
		// 401. Second visit onward the SW is already installed, so
		// `.ready` resolves in ~1ms — the wait is a one-shot cost per
		// browser profile.
		if ('serviceWorker' in navigator) {
			void navigator.serviceWorker
				.register('/service-worker.js', { type: 'module' })
				.catch((err) => console.debug('DPoP service worker registration failed', err));
			try {
				await navigator.serviceWorker.ready;
			} catch {
				/* SW registration failed — unbound sessions still work;
				   bound sessions in `required` mode see 401s on browser-
				   direct URLs. Same fail-open shape as pre-SW. */
			}
			// Soft-reload once if we landed uncontrolled. Covers two
			// cases the SW itself can't fix:
			//   * First-ever visit — page loaded before any SW existed,
			//     so `controller` is null even after `.ready`.
			//   * Force-refresh (Cmd-Shift-R) — the browser deliberately
			//     delivers the top-level document with no SW controller;
			//     `clients.claim()` can't attach retroactively.
			// A single reload brings the page under SW control.
			//
			// `sessionStorage` guard prevents an infinite loop when the
			// reload doesn't fix it (SW registration is genuinely broken
			// — bad URL, CSP block, quota). Cleared once `controller` is
			// set so a LATER force-refresh in the same session can also
			// self-heal (one reload per uncontrolled-landing).
			const SW_RELOAD_KEY = 'sw-reload-once';
			if (!navigator.serviceWorker.controller) {
				if (!sessionStorage.getItem(SW_RELOAD_KEY)) {
					sessionStorage.setItem(SW_RELOAD_KEY, '1');
					location.reload();
					return;
				}
				/* reload already attempted this session — give up gracefully */
			} else {
				sessionStorage.removeItem(SW_RELOAD_KEY);
			}
		}

		// The instant HTML boot splash has done its job — the app is mounted, so
		// the route (login renders immediately; protected routes show their own
		// loading state) is already in the DOM behind it.
		document.getElementById('app-splash')?.remove();

		// Redirect old `#/...` bookmarks to the new path before anything else.
		if (typeof location !== 'undefined' && location.hash.startsWith('#/')) {
			// hashUrlToPath returns a dynamic in-app path string; resolve() is typed
			// for known route ids, so assert it as a Pathname (same precedent as the
			// post-login redirect target).
			const mapped = hashUrlToPath(location.hash);
			if (mapped) await goto(resolve(mapped as Pathname), { replaceState: true });
		}
		// Parallel — providers is a public endpoint independent of session state.
		const [, prov] = await Promise.all([session.load(), getOidcProviders()]);
		providers = prov;
		ready = true;
	});

	// Guard: once the session is known, bounce unauthenticated users off
	// protected routes. Runs client-side only (ssr=false).
	$effect(() => {
		if (!ready) return;
		// During an explicit logout `AppShell.onLogout` has already picked
		// the destination (`/login?source=logged_out`) and issued the
		// navigation. `session.reset()` inside that flow flips
		// `session.isAuthenticated` to false, which fires THIS effect
		// reactively — if we don't bail, we race the pending goto with a
		// `/login?redirect=<current path>` nav and last-write-wins clobbers
		// the "signed out" banner (Ed's report: URL landed as
		// `?redirect=%2Ffiles%2F…` instead of `?source=logged_out`).
		if (isLogoutInProgress()) return;
		const path = page.url.pathname;
		if (session.isAuthenticated || isPublic(path)) return;

		// SSO-only auto-redirect: mirror what the server-side /login
		// middleware does for direct HTTP loads. Full-page navigation
		// (`window.location`) so we hit the OxiCloud backend fresh — that
		// endpoint 307s to the IdP with a fresh state + PKCE challenge.
		// `goto()` would keep us in the SPA and never leave.
		if (providers?.auto_redirect_to_oidc && providers.authorize_endpoint) {
			window.location.replace(providers.authorize_endpoint);
			return;
		}
		void goto(resolve(`/login?redirect=${encodeURIComponent(path)}`), { replaceState: true });
	});

	// Mandatory change-password guard. When the backend has set
	// `force_password_change_at_next_login` (admin reset), the SPA MUST
	// keep the user on the profile page until they pick a new password.
	// The backend also refuses every non-allowlisted endpoint with 403
	// PasswordChangeRequired — this guard is the UX side of that lock,
	// so the user sees the password form instead of a wall of 403s
	// wherever they clicked.
	//
	// Runs AFTER the unauthenticated guard so we don't misroute a
	// still-loading session. Skips the guard on `/login` too — a
	// signed-out user on the login form doesn't yet have a session
	// state to consult, and if `session.mustChangePassword` is true
	// on `/login` (rare — happens if the user reloaded post-login
	// but before the profile navigation completed), the login-form
	// success handler will route to `/profile?forcePasswordChange=1`
	// on its own.
	$effect(() => {
		if (!ready) return;
		if (!session.isAuthenticated) return;
		if (!session.mustChangePassword) return;
		const path = page.url.pathname;
		if (path === '/profile' || isPublic(path)) return;
		// Preserve the intended destination so the profile page can
		// bounce back once the password is successfully changed.
		const next = encodeURIComponent(path);
		void goto(resolve(`/profile?forcePasswordChange=1&next=${next}`), { replaceState: true });
	});
</script>

{#if isPublic(page.url.pathname)}
	{@render children()}
{:else if ready && session.isAuthenticated}
	<AppShell {children} />
{:else if ready}
	{@render children()}
{:else}
	<div class="app-loading" aria-busy="true">Loading…</div>
{/if}

<Toaster />
<DialogHost />

<style>
	.app-loading {
		display: grid;
		place-items: center;
		min-height: 100vh;
		color: var(--color-text-muted);
	}
</style>
