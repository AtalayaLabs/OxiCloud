<script lang="ts">
	import { errorToast } from '$lib/utils/errors';
	import { relativeTimeAgo } from '$lib/utils/time';
	import { onMount, tick } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import type { Pathname } from '$app/types';
	import { page } from '$app/state';
	import { ApiError } from '$lib/api/client';
	import {
		changePassword,
		createAppPassword,
		isAutoAppPassword,
		listAppPasswords,
		revokeAppPassword,
		updateAvatar,
		updateProfile,
		type AppPassword,
		type ProfilePatch
	} from '$lib/api/endpoints/profile';
	import { fetchMe, getOidcProviders, startOidcLink, unlinkOidc } from '$lib/api/endpoints/auth';
	import { SUPPORTED_LOCALES, setLocale, t, type Locale } from '$lib/i18n/index.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { preferences } from '$lib/stores/preferences.svelte';
	import { session } from '$lib/stores/session.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { formatBytes } from '$lib/utils/format';
	import { formatDate } from '$lib/utils/display';
	import { resizeImageToDataUrl } from '$lib/utils/imageResize';

	let givenName = $state('');
	let familyName = $state('');
	let username = $state('');
	let preferredLocale = $state<string>('');
	let notifyOnShare = $state(true);
	// Batched into the profile save flow (same UX as
	// `notifyOnShare` above). The `preferences` store is still the
	// source of truth for the persisted value — this local mirrors it
	// on hydrate, and the diff feeds `patch.ui_preferences` on save
	// so the whole card follows one save discipline.
	let hideDotfiles = $state(false);

	let currentPw = $state('');
	let newPw = $state('');
	let confirmPw = $state('');

	let savingProfile = $state(false);
	let savingPassword = $state(false);

	let avatarBusy = $state(false);
	let passwordLoginEnabled = $state(true);
	// OIDC providers snapshot for the SSO link/unlink card. Populated
	// on mount; determines whether we render the "Connect SSO" card
	// (requires oidcEnabled) and what display label to show.
	let oidcEnabled = $state(false);
	let oidcProviderName = $state<string>('SSO');
	let ssoBusy = $state(false);

	// Avatar edit panel.
	let avatarEditOpen = $state(false);
	let avatarTab = $state<'url' | 'upload'>('url');
	let avatarUrl = $state('');
	let avatarPreview = $state<string | null>(null);
	let uploadedDataUrl = $state<string | null>(null);
	let avatarImgFailed = $state(false);

	let appPasswords = $state<AppPassword[]>([]);
	let appPwLoadFailed = $state(false);
	let generated = $state<{ label: string; password: string } | null>(null);
	let newLabel = $state('');
	let creatingPw = $state(false);
	let autoExpanded = $state(false);

	const isOidc = $derived(session.me?.full.federation_kind === 'oidc');
	const isLocal = $derived(!session.me?.full.federation_kind);
	const usernameClaimed = $derived(!!session.user?.username);
	const isAdmin = $derived(session.user?.role === 'admin');
	const canEditImage = $derived(session.me?.can_edit_image === true && isLocal);
	// Show the change-password card when the user CAN change their
	// local password: they have `password_hash` on file AND the
	// deployment offers password login (backend `change_password`
	// refuses on either count — see `AuthApplicationService::change_password`).
	// Distinct from the OLD `isLocal && passwordLoginEnabled` gate,
	// which refused any SSO-linked account regardless of whether they
	// carried a local password. Hybrid accounts (OIDC + local
	// password) are a legitimate posture and MUST be able to rotate
	// their local credential; the new gate lets them, and the backend
	// refusal covers the pure-SSO case where has_password is false.
	const showPasswordCard = $derived(
		(session.me?.full.has_password ?? false) && passwordLoginEnabled
	);

	// SSO card gates — see docs/plan/oidc-account-linking.md.
	// Connect: only when OIDC is enabled AND the user isn't already linked.
	// Disconnect: only when currently OIDC-linked AND the user has an
	// alternative auth method (password or OPAQUE-registered) — else
	// unlinking would lock them out.
	const canConnectSso = $derived(oidcEnabled && !session.me?.full.federation_kind);
	// Show the disconnect button whenever the user is OIDC-linked.
	// The backend guard (`AuthApplicationService::unlink_oidc`) is the
	// source of truth for the "no alternative auth" refusal — it also
	// checks `opaque_registered`, which isn't exposed on UserDto today
	// (deliberately kept off `/api/auth/me` to avoid leaking OPAQUE
	// adoption status through user-directory endpoints). The UI shows
	// the button unconditionally and surfaces the backend's 403 as a
	// user-facing "set a password first" prompt.
	const canDisconnectSso = $derived(session.me?.full.federation_kind === 'oidc');

	/**
	 * Mandatory change-password mode. TRUE when the backend has
	 * flagged the account (`session.mustChangePassword`) OR the URL
	 * carries `?forcePasswordChange=1` (arrived here from the login
	 * form / layout guard). Either signal locks the page into a
	 * single-purpose form: banner + password card only, other cards
	 * hidden. The URL param is a belt-and-braces alongside the store
	 * — a stale-tab session that lost the flag momentarily still
	 * shows the mandatory UI if the URL says so, and the layout guard
	 * will bounce a non-flagged user back off `/profile` naturally.
	 */
	const forceModeQueryParam = $derived(page.url.searchParams.get('forcePasswordChange') === '1');
	const mandatoryMode = $derived(session.mustChangePassword || forceModeQueryParam);
	/**
	 * Destination to bounce back to after a successful change. Only
	 * consulted in mandatory mode; caller-supplied via `?next=<encoded>`
	 * (added by the layout guard). Falls back to `/files` — the
	 * standard SPA landing point — when absent or when the value
	 * isn't a same-origin path (`startsWith('/')`).
	 */
	const nextAfterChange = $derived.by(() => {
		const raw = page.url.searchParams.get('next');
		if (!raw) return '/files';
		try {
			const decoded = decodeURIComponent(raw);
			return decoded.startsWith('/') ? decoded : '/files';
		} catch {
			return '/files';
		}
	});

	const storagePct = $derived.by(() => {
		const full = session.me?.full;
		if (!full || full.storage_quota_bytes <= 0) return 0;
		return Math.min(100, Math.round((full.storage_used_bytes / full.storage_quota_bytes) * 100));
	});
	const storageBarClass = $derived(
		storagePct > 90 ? 'bar__fill--red' : storagePct > 70 ? 'bar__fill--orange' : 'bar__fill--green'
	);
	const initials = $derived(
		(session.user?.username || session.user?.email || '?').slice(0, 2).toUpperCase()
	);

	const userPasswords = $derived(appPasswords.filter((p) => !isAutoAppPassword(p)));
	const autoPasswords = $derived(appPasswords.filter((p) => isAutoAppPassword(p)));

	/** Relative time (e.g. "3 days ago"); "Never" when absent. */
	const timeAgo = (value: string | null | undefined): string =>
		relativeTimeAgo(value, { empty: t('profile.never', 'Never'), invalidAsString: true });

	function hydrate() {
		const me = session.me;
		if (!me) return;
		// Public identity (name / handle) reads via `me.full.user`;
		// admin-visible extras (preferred_locale) via `me.full`;
		// self-only bag flags (notify_on_share) via `me` directly.
		// The three-level indirection makes the audience of each
		// field visible at the callsite (docs/plan/userdto-refactor.md).
		givenName = me.full.user.given_name ?? '';
		familyName = me.full.user.family_name ?? '';
		username = me.full.user.username ?? '';
		preferredLocale = me.full.preferred_locale ?? '';
		notifyOnShare = me.notify_on_share;
		// Source of truth is the preferences store, which itself
		// derives from `session.me.ui_preferences`. Reading through
		// the store here (rather than the raw bag) means a new
		// preference field just needs a getter in the store and its
		// own line here — no wire-format knowledge on the page.
		hideDotfiles = preferences.hideDotfiles;
	}

	async function saveProfile(e: SubmitEvent) {
		e.preventDefault();
		const me = session.me;
		if (!me) return;

		// Build a sparse patch of only the fields the user actually changed.
		// Sending empty strings the user never touched would 400 on the server.
		const patch: ProfilePatch = {};
		if (!usernameClaimed && username.trim() && username.trim() !== (me.full.user.username ?? '')) {
			patch.username = username.trim();
		}
		if (givenName.trim() !== (me.full.user.given_name ?? '')) patch.given_name = givenName.trim();
		if (familyName.trim() !== (me.full.user.family_name ?? ''))
			patch.family_name = familyName.trim();
		if ((preferredLocale || '') !== (me.full.preferred_locale ?? '')) {
			patch.preferred_locale = preferredLocale || undefined;
		}
		if (notifyOnShare !== me.notify_on_share) patch.notify_on_share = notifyOnShare;
		// Ship the diff as a partial `ui_preferences` patch — the
		// server does a shallow merge, so only the changed key is
		// touched; siblings set on other devices survive.
		if (hideDotfiles !== preferences.hideDotfiles) {
			patch.ui_preferences = { hide_dotfiles: hideDotfiles };
		}

		if (Object.keys(patch).length === 0) {
			ui.notify(t('profile.profile_no_changes', 'No changes to save.'), 'info');
			return;
		}

		savingProfile = true;
		try {
			// PATCH /me/profile echoes SelfUser (same shape as GET /me)
			// so the SPA absorbs the just-written state in one round
			// trip — no follow-up refresh needed. `session.user` is a
			// derived accessor over `session.me.full.user`, so it
			// updates in lockstep with the me assignment.
			const updated = await updateProfile(patch);
			session.me = updated;
			if (patch.preferred_locale) await setLocale(patch.preferred_locale as Locale);
			ui.notify(t('profile.saved', 'Profile saved'), 'success');
		} catch (err) {
			errorToast(err);
		} finally {
			savingProfile = false;
		}
	}

	async function savePassword(e: SubmitEvent) {
		e.preventDefault();
		if (newPw !== confirmPw) {
			ui.notify(t('profile.password_mismatch', 'Passwords do not match'), 'error');
			return;
		}
		if (newPw.length < 8) {
			ui.notify(
				t('profile.password_too_short', 'Password must be at least 8 characters.'),
				'error'
			);
			return;
		}
		if (newPw === currentPw) {
			// Fast client-side reject — the backend also enforces this
			// (400 `PasswordUnchanged`) but the SPA can save the round-
			// trip. Load-bearing in mandatory mode: silently accepting
			// same-as-current would clear the force flag without a real
			// rotation, defeating the "temporary password" pattern.
			ui.notify(
				t('profile.password_unchanged', 'New password must differ from the current one.'),
				'error'
			);
			return;
		}
		savingPassword = true;
		try {
			await changePassword(currentPw, newPw);
			currentPw = newPw = confirmPw = '';
			ui.notify(t('profile.password_updated', 'Password updated'), 'success');

			// If we're in mandatory mode, the backend just cleared the
			// force flag AND revoked all sessions. Refresh the session
			// so the layout guard lifts, then bounce to the intended
			// destination the layout captured on entry. Refresh order
			// matters: goto() before the session refresh would race
			// the layout's `mustChangePassword` derived and re-redirect
			// us right back to /profile.
			if (mandatoryMode) {
				try {
					const me = await fetchMe();
					if (me) session.setUser(me);
				} catch {
					/* stale session state is recoverable — the next request refreshes it */
				}
				await goto(resolve(nextAfterChange as Pathname), { replaceState: true });
			}
		} catch (err) {
			// Remap the backend's `PasswordUnchanged` error_type to a
			// specific, translatable message — the generic errorToast
			// would show the raw server string. Every other error
			// path still flows through errorToast.
			if (err instanceof ApiError && err.errorType === 'PasswordUnchanged') {
				ui.notify(
					t('profile.password_unchanged', 'New password must differ from the current one.'),
					'error'
				);
			} else {
				errorToast(err);
			}
		} finally {
			savingPassword = false;
		}
	}

	// In mandatory mode, focus the current-password input as soon as
	// the DOM is ready so the user can type without scrolling / clicking
	// around to find the form. `tick()` waits for the reactive render;
	// the null-check tolerates the (rare) case where the form isn't
	// mounted yet on first paint.
	onMount(async () => {
		if (!mandatoryMode) return;
		await tick();
		const el = document.querySelector<HTMLInputElement>(
			'[data-testid="profile-current-password-input"]'
		);
		el?.focus();
	});

	// ── Avatar edit panel ──────────────────────────────────────────────────
	function openAvatarEdit() {
		avatarEditOpen = true;
		avatarTab = 'url';
		avatarUrl = '';
		avatarPreview = null;
		uploadedDataUrl = null;
	}

	function closeAvatarEdit() {
		avatarEditOpen = false;
		uploadedDataUrl = null;
		avatarPreview = null;
	}

	async function onAvatarFile(e: Event) {
		const input = e.target as HTMLInputElement;
		const file = input.files?.[0];
		if (!file) return;
		try {
			const dataUrl = await resizeImageToDataUrl(file);
			uploadedDataUrl = dataUrl;
			avatarPreview = dataUrl;
		} catch (err) {
			uploadedDataUrl = null;
			avatarPreview = null;
			errorToast(err);
		} finally {
			input.value = '';
		}
	}

	async function commitAvatar(image: string | null) {
		avatarBusy = true;
		try {
			await updateAvatar(image);
			if (session.user) session.user = { ...session.user, image };
			avatarImgFailed = false;
			closeAvatarEdit();
		} catch (err) {
			errorToast(err);
		} finally {
			avatarBusy = false;
		}
	}

	async function saveAvatar() {
		if (avatarTab === 'url') {
			await commitAvatar(avatarUrl.trim() || null);
		} else {
			if (!uploadedDataUrl) {
				ui.notify(t('profile.photo_no_file', 'Choose a photo first.'), 'error');
				return;
			}
			await commitAvatar(uploadedDataUrl);
		}
	}

	// ── App passwords ──────────────────────────────────────────────────────
	async function loadAppPasswords() {
		try {
			appPasswords = await listAppPasswords();
			appPwLoadFailed = false;
		} catch {
			appPwLoadFailed = true;
		}
	}

	async function createPw() {
		const label = newLabel.trim();
		if (!label) {
			ui.notify(t('profile.error_label_required', 'Enter a label.'), 'error');
			return;
		}
		creatingPw = true;
		try {
			const password = await createAppPassword(label);
			generated = { label, password };
			newLabel = '';
			await loadAppPasswords();
		} catch (err) {
			errorToast(err);
		} finally {
			creatingPw = false;
		}
	}

	async function revokePw(p: AppPassword) {
		const ok = await confirmDialog({
			title: t('profile.app_pw_revoke', 'Revoke app password'),
			message: t('profile.confirm_revoke', { label: p.label }, 'Revoke "{{label}}"?'),
			confirmText: t('profile.app_pw_revoke', 'Revoke'),
			danger: true
		});
		if (!ok) return;
		try {
			await revokeAppPassword(p.id);
			generated = null;
			await loadAppPasswords();
		} catch (err) {
			errorToast(err);
		}
	}

	async function copyGenerated() {
		if (!generated) return;
		try {
			await navigator.clipboard.writeText(generated.password);
			ui.notify(t('profile.copied', 'Copied'), 'success');
		} catch {
			ui.notify(t('profile.copy_failed', 'Could not copy'), 'error');
		}
	}

	onMount(async () => {
		if (!session.loaded) await session.load();
		hydrate();
		void loadAppPasswords();
		try {
			const providers = await getOidcProviders();
			// Only an explicit `false` hides the password card; an absent flag
			// (no OIDC configured) leaves local password login available.
			if (providers.password_login_enabled === false) passwordLoginEnabled = false;
			// Capture OIDC state for the SSO link/unlink card. `provider_name`
			// is the display label the FE renders in the "Connected to X"
			// affordance.
			oidcEnabled = providers.enabled === true;
			if (typeof providers.provider_name === 'string' && providers.provider_name.length > 0) {
				oidcProviderName = providers.provider_name;
			}
		} catch {
			/* leave password login enabled */
		}

		// Toast handling for the OIDC-link callback redirect. The
		// backend redirects here with `?linked=1` on success or
		// `?link_error=<reason>` on refusal (see plan doc for the
		// stable reason keys). Show a translated toast and strip the
		// query params via history.replaceState so a page reload
		// doesn't re-fire the toast.
		const params = page.url.searchParams;
		const linked = params.get('linked');
		const linkError = params.get('link_error');
		if (linked === '1') {
			ui.notify(t('profile.sso_linked_success', 'Single sign-on connected successfully.'), 'info');
			// Session's federation_kind may still be stale from before
			// the round-trip; re-fetch to pick up the fresh columns
			// (federation_kind should now be 'oidc').
			try {
				const me = await fetchMe();
				if (me) session.me = me;
			} catch {
				/* stale session is recoverable — next request refreshes */
			}
		} else if (linkError) {
			// Map the stable reason keys to translated messages. Falls
			// back to a generic message for keys we don't recognise
			// (forward-compatible with new refusal reasons).
			// 10 s dwell (vs the 4 s default) — the copy is long enough
			// that the default vanishes before the user finishes reading.
			const msg = ssoLinkErrorMessage(linkError);
			ui.notify(msg, 'error', 10000);
		}
		if (linked !== null || linkError !== null) {
			const stripped = new URL(page.url);
			stripped.searchParams.delete('linked');
			stripped.searchParams.delete('link_error');
			window.history.replaceState(
				window.history.state,
				'',
				stripped.pathname + stripped.search + stripped.hash
			);
		}
	});

	function ssoLinkErrorMessage(key: string): string {
		// Keys match the `reason` field of `federation.link_refused`
		// audit events — see docs/plan/oidc-account-linking.md.
		switch (key) {
			case 'email_mismatch':
				return t(
					'profile.sso_link_error_email_mismatch',
					"The email from your SSO provider doesn't match your OxiCloud account email."
				);
			case 'email_not_provided':
				return t(
					'profile.sso_link_error_email_not_provided',
					"Your SSO provider didn't return an email address, so we can't verify the link."
				);
			case 'already_linked_elsewhere':
				return t(
					'profile.sso_link_error_already_linked_elsewhere',
					'This SSO identity is already linked to a different OxiCloud account.'
				);
			case 'already_linked':
				return t(
					'profile.sso_link_error_already_linked',
					'Your account is already linked to a different SSO identity. Disconnect first.'
				);
			case 'session_expired':
				return t(
					'profile.sso_link_error_session_expired',
					'Your session expired during the SSO round-trip. Please sign in again.'
				);
			default:
				return t('profile.sso_link_error_generic', 'SSO link failed. Please try again.');
		}
	}

	async function onConnectSso(e: SubmitEvent) {
		e.preventDefault();
		ssoBusy = true;
		try {
			const url = await startOidcLink();
			// Full-page navigation so the browser leaves the SPA and
			// hits the IdP; the callback lands back on /profile via
			// the extended callback dispatch. `goto()` would stay in
			// the SPA and never leave.
			window.location.assign(url);
		} catch (err) {
			ssoBusy = false;
			errorToast(err);
		}
	}

	async function onDisconnectSso(e: SubmitEvent) {
		e.preventDefault();
		const ok = await confirmDialog({
			title: t('profile.sso_disconnect_confirm_title', 'Disconnect Single Sign-On?'),
			message: t(
				'profile.sso_disconnect_confirm_message',
				"You'll only be able to sign in with your password or OPAQUE credential after this."
			),
			confirmText: t('profile.sso_disconnect_confirm_button', 'Disconnect'),
			danger: true
		});
		if (!ok) return;
		ssoBusy = true;
		try {
			await unlinkOidc();
			const me = await fetchMe();
			if (me) session.me = me;
			ui.notify(t('profile.sso_unlinked_success', 'Single sign-on disconnected.'), 'info');
		} catch (err) {
			if (err instanceof ApiError && err.errorType === 'NoAlternativeAuth') {
				ui.notify(
					t(
						'profile.sso_unlink_no_alt_auth',
						'Set a password first — otherwise you would be locked out.'
					),
					'error',
					10000
				);
			} else {
				errorToast(err);
			}
		} finally {
			ssoBusy = false;
		}
	}
</script>

<svelte:head><title>{t('nav.profile', 'Profile')} · OxiCloud</title></svelte:head>

<main class="profile" class:profile--mandatory={mandatoryMode}>
	<h1>{t('nav.profile', 'Profile')}</h1>

	{#if mandatoryMode}
		<!--
			Mandatory-mode banner. Rendered above every other section
			whenever `session.mustChangePassword` is TRUE or the URL
			carries `?forcePasswordChange=1`. Explains WHY the user
			landed here (an admin picked a temporary password) and
			what they need to do (rotate before continuing). Backend
			also refuses every non-allowlisted endpoint with 403
			PasswordChangeRequired — so a user who dismisses the
			banner via URL manipulation still can't reach any file /
			DAV / admin endpoint until the change lands.
		-->
		<div
			class="mandatory-banner"
			role="alert"
			data-testid="profile-mandatory-change-password-banner"
		>
			<Icon name="shield-alt" />
			<div class="mandatory-banner__body">
				<strong>
					{t('profile.mandatory_change_title', 'Please change your password to continue.')}
				</strong>
				<p>
					{t(
						'profile.mandatory_change_body',
						'An administrator has set a temporary password for your account. Choose your own password below before you can access the rest of the application.'
					)}
				</p>
			</div>
		</div>
	{/if}

	{#if session.user}
		<!-- Avatar / identity -->
		<div class="card avatar-card">
			<div class="avatar-section">
				{#if session.user.image && !avatarImgFailed}
					<img
						class="avatar-lg"
						src={session.user.image}
						alt={initials}
						onerror={() => (avatarImgFailed = true)}
					/>
				{:else}
					<span class="avatar-lg avatar-lg--initials">{initials}</span>
				{/if}
				<div class="avatar-info">
					<h2>{session.user.username || session.user.email || '—'}</h2>
					<div class="muted">{session.user.email}</div>
					<span class="role-badge" class:role-badge--admin={isAdmin}>
						<Icon name={isAdmin ? 'shield-alt' : 'user'} />
						{isAdmin ? t('profile.role_admin', 'Administrator') : t('profile.role_user', 'User')}
					</span>
					{#if isOidc && session.user.image}
						<p class="muted">
							{t('profile.photo_managed_by_oidc', 'Photo managed by your identity provider.')}
						</p>
					{/if}
				</div>
				{#if canEditImage}
					<button
						class="btn btn-secondary avatar-edit-btn"
						data-testid="profile-avatar-edit-btn"
						title={t('profile.edit_photo', 'Edit photo')}
						onclick={openAvatarEdit}
					>
						<Icon name="pencil-alt" />
					</button>
				{/if}
			</div>

			{#if canEditImage && avatarEditOpen}
				<div class="avatar-edit" data-testid="profile-avatar-edit-panel">
					<div class="avatar-tabs">
						<button
							class="avatar-tab"
							data-testid="profile-avatar-url-tab"
							class:avatar-tab--active={avatarTab === 'url'}
							onclick={() => (avatarTab = 'url')}
						>
							{t('profile.photo_tab_url', 'URL')}
						</button>
						<button
							class="avatar-tab"
							data-testid="profile-avatar-upload-tab"
							class:avatar-tab--active={avatarTab === 'upload'}
							onclick={() => (avatarTab = 'upload')}
						>
							{t('profile.photo_tab_upload', 'Upload')}
						</button>
					</div>

					{#if avatarTab === 'url'}
						<input
							type="url"
							data-testid="profile-avatar-url-input"
							bind:value={avatarUrl}
							placeholder="https://example.com/photo.jpg"
						/>
						<small class="muted">
							{t('profile.photo_url_hint', 'https://, http://, or data:image/…;base64,… accepted')}
						</small>
					{:else}
						<label class="avatar-file-label btn btn-secondary">
							<Icon name="user-plus" />
							<span>{t('profile.photo_choose_file', 'Choose a photo (PNG, JPEG, WebP)')}</span>
							<input
								type="file"
								data-testid="profile-avatar-file-input"
								accept="image/png,image/jpeg,image/webp"
								hidden
								onchange={onAvatarFile}
							/>
						</label>
						{#if avatarPreview}
							<img class="avatar-preview" src={avatarPreview} alt={t('profile.avatar', 'Avatar')} />
						{/if}
						<small class="muted">
							{t(
								'profile.photo_resize_note',
								'Images larger than 512 × 512 px are automatically resized.'
							)}
						</small>
					{/if}

					<div class="avatar-edit-actions">
						<button
							class="btn btn-primary"
							data-testid="profile-avatar-save-btn"
							disabled={avatarBusy}
							onclick={saveAvatar}
						>
							{t('profile.photo_save', 'Save')}
						</button>
						{#if session.user.image}
							<button
								class="btn link-btn link-btn--danger"
								data-testid="profile-avatar-remove-btn"
								disabled={avatarBusy}
								onclick={() => commitAvatar(null)}
							>
								{t('profile.photo_remove', 'Remove photo')}
							</button>
						{/if}
						<button
							class="btn btn-secondary"
							data-testid="profile-avatar-cancel-btn"
							disabled={avatarBusy}
							onclick={closeAvatarEdit}
						>
							{t('common.cancel', 'Cancel')}
						</button>
					</div>
				</div>
			{/if}
		</div>

		<!-- Account details -->
		<div class="card">
			<h2><Icon name="id-card" /> {t('profile.account_details', 'Account Details')}</h2>
			<div class="info-grid">
				<div class="info-item">
					<div class="info-label"><Icon name="user" /> {t('profile.username', 'Username')}</div>
					<div class="info-value">{session.user.username || '—'}</div>
				</div>
				<div class="info-item">
					<div class="info-label"><Icon name="envelope" /> {t('profile.email', 'Email')}</div>
					<div class="info-value">{session.user.email}</div>
				</div>
				<div class="info-item">
					<div class="info-label"><Icon name="shield-alt" /> {t('profile.role', 'Role')}</div>
					<div class="info-value">
						{isAdmin ? t('profile.role_admin', 'Administrator') : t('profile.role_user', 'User')}
					</div>
				</div>
				<div class="info-item">
					<div class="info-label">
						<Icon name="clock" />
						{t('profile.last_login', 'Last Login')}
					</div>
					<div class="info-value">{timeAgo(session.me?.full.last_login_at)}</div>
				</div>
			</div>
		</div>

		<!-- Storage -->
		<div class="card">
			<h2><Icon name="hdd" /> {t('profile.storage', 'Storage')}</h2>
			<div class="storage-stats">
				<div class="storage-stat">
					<div class="stat-value">{formatBytes(session.me?.full.storage_used_bytes ?? 0)}</div>
					<div class="muted">{t('profile.used', 'Used')}</div>
				</div>
				<div class="storage-stat">
					<div class="stat-value">
						{(session.me?.full.storage_quota_bytes ?? 0) > 0
							? formatBytes(session.me?.full.storage_quota_bytes ?? 0)
							: '∞'}
					</div>
					<div class="muted">{t('profile.quota', 'Quota')}</div>
				</div>
				<div class="storage-stat">
					<div class="stat-value">
						{(session.me?.full.storage_quota_bytes ?? 0) > 0 ? `${storagePct}%` : '—'}
					</div>
					<div class="muted">{t('profile.usage', 'Usage')}</div>
				</div>
			</div>
			<div class="bar">
				<div class={`bar__fill ${storageBarClass}`} style:width="{storagePct}%"></div>
			</div>
		</div>

		<!-- Edit profile (hidden for OIDC users) -->
		<div class="card">
			<h2><Icon name="id-badge" /> {t('profile.edit_profile', 'Edit Profile')}</h2>
			{#if isOidc}
				<div class="alert alert--info">
					<Icon name="info-circle" />
					<span>
						{t(
							'profile.edit_oidc_managed',
							'To change your information (name, profile picture, …), please update it at your identity provider. Your changes will appear on your next sign-in.'
						)}
					</span>
				</div>
			{:else}
				<form data-testid="profile-edit-form" onsubmit={saveProfile}>
					<label>
						<span>{t('profile.username', 'Username')}</span>
						<input
							data-testid="profile-username-input"
							bind:value={username}
							maxlength="64"
							autocomplete="username"
							disabled={usernameClaimed}
						/>
						<small class="muted">
							{usernameClaimed
								? t('profile.username_already_claimed', "Username can't be changed once set.")
								: t(
										'profile.username_claim_hint',
										"2–64 characters. Once chosen, the username can't be changed."
									)}
						</small>
					</label>
					<label>
						<span>{t('profile.given_name', 'First name')}</span>
						<input
							data-testid="profile-given-name-input"
							bind:value={givenName}
							maxlength="128"
							autocomplete="given-name"
						/>
					</label>
					<label>
						<span>{t('profile.family_name', 'Last name')}</span>
						<input
							data-testid="profile-family-name-input"
							bind:value={familyName}
							maxlength="128"
							autocomplete="family-name"
						/>
					</label>
					<label>
						<span>{t('profile.language', 'Language')}</span>
						<select data-testid="profile-language-select" bind:value={preferredLocale}>
							<option value="" data-testid="profile-language-auto-option"
								>{t('profile.language_auto', 'Automatic')}</option
							>
							{#each SUPPORTED_LOCALES as loc (loc)}
								<option value={loc} data-testid={`profile-language-option-${loc}`}>{loc}</option>
							{/each}
						</select>
					</label>
					<label class="checkbox">
						<input
							type="checkbox"
							data-testid="profile-notify-on-share-checkbox"
							bind:checked={notifyOnShare}
						/>
						<span>{t('profile.notify_on_share', 'Email me when someone shares with me')}</span>
					</label>
					<label class="checkbox">
						<input
							type="checkbox"
							data-testid="profile-hide-dotfiles-checkbox"
							bind:checked={hideDotfiles}
						/>
						<span
							>{t(
								'profile.hide_dotfiles',
								'Hide files whose name starts with a dot (.env, .git, …)'
							)}</span
						>
					</label>
					<button type="submit" data-testid="profile-save-btn" disabled={savingProfile}
						>{t('profile.save_profile', 'Save changes')}</button
					>
				</form>
			{/if}
		</div>

		<!-- App passwords -->
		{#if !appPwLoadFailed}
			<div class="card">
				<h2><Icon name="key" /> {t('profile.app_passwords', 'App Passwords')}</h2>
				<p class="muted">
					{t(
						'profile.app_pw_desc',
						'Generate passwords for WebDAV, CalDAV, and CardDAV clients. Each password is shown only once.'
					)}
				</p>

				<div class="app-pw-create">
					<input
						data-testid="profile-app-pw-label-input"
						bind:value={newLabel}
						maxlength="128"
						placeholder={t('profile.app_pw_label_placeholder', 'Label (e.g. Thunderbird, macOS)')}
					/>
					<button
						class="btn btn-primary"
						data-testid="profile-app-pw-generate-btn"
						disabled={creatingPw}
						onclick={createPw}
					>
						<Icon name="user-plus" />
						{t('profile.generate', 'Generate')}
					</button>
				</div>

				{#if generated}
					<div class="generated">
						<div>
							{t('profile.new_password_for', 'New password for')}
							<strong>{generated.label}</strong>:
						</div>
						<div class="generated__value">
							<code>{generated.password}</code>
							<button
								class="btn-action"
								data-testid="profile-app-pw-copy-btn"
								title={t('profile.copy_to_clipboard', 'Copy to clipboard')}
								onclick={copyGenerated}
							>
								<Icon name="copy" />
							</button>
						</div>
						<small class="muted">
							{t(
								'profile.copy_warning',
								"Copy this password now. You won't be able to see it again."
							)}
						</small>
					</div>
				{/if}

				{#if userPasswords.length === 0}
					<p class="muted">{t('profile.no_app_passwords', 'No app passwords yet.')}</p>
				{:else}
					<table class="pw-table">
						<thead>
							<tr>
								<th>{t('profile.col_label', 'Label')}</th>
								<th>{t('profile.col_created', 'Created')}</th>
								<th>{t('profile.col_last_used', 'Last Used')}</th>
								<th>{t('profile.col_status', 'Status')}</th>
								<th></th>
							</tr>
						</thead>
						<tbody>
							{#each userPasswords as p (p.id)}
								<tr>
									<td>{p.label}</td>
									<td>{formatDate(p.created_at)}</td>
									<td>{p.last_used_at ? timeAgo(p.last_used_at) : t('profile.never', 'Never')}</td>
									<td>
										{#if p.active !== false}
											<span class="badge badge--active">{t('profile.active', 'Active')}</span>
										{:else}
											<span class="badge badge--revoked">{t('profile.revoked', 'Revoked')}</span>
										{/if}
									</td>
									<td>
										{#if p.active !== false}
											<button
												class="btn-action btn-action--danger"
												data-testid={`profile-app-pw-revoke-${p.id}`}
												title={t('profile.revoke_title', 'Revoke')}
												onclick={() => revokePw(p)}
											>
												<Icon name="trash-alt" />
											</button>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}

				{#if autoPasswords.length > 0}
					<div class="app-pw-auto">
						<button
							class="app-pw-auto__toggle"
							data-testid="profile-app-pw-auto-toggle-btn"
							onclick={() => (autoExpanded = !autoExpanded)}
						>
							<Icon name={autoExpanded ? 'chevron-down' : 'chevron-right'} />
							<span>{t('profile.client_sessions', 'Client sessions')}</span>
							<span class="badge badge--count">{autoPasswords.length}</span>
						</button>
						{#if autoExpanded}
							<p class="muted">
								{t(
									'profile.client_sessions_desc',
									'Auto-generated when you connect a Nextcloud-compatible client.'
								)}
							</p>
							<table class="pw-table">
								<thead>
									<tr>
										<th>{t('profile.col_client', 'Client')}</th>
										<th>{t('profile.col_created', 'Created')}</th>
										<th>{t('profile.col_last_used', 'Last Used')}</th>
										<th></th>
									</tr>
								</thead>
								<tbody>
									{#each autoPasswords as p (p.id)}
										<tr>
											<td>{p.label}</td>
											<td>{formatDate(p.created_at)}</td>
											<td
												>{p.last_used_at
													? timeAgo(p.last_used_at)
													: t('profile.never', 'Never')}</td
											>
											<td>
												{#if p.active !== false}
													<button
														class="btn-action btn-action--danger"
														data-testid={`profile-app-pw-auto-revoke-${p.id}`}
														title={t('profile.revoke_title', 'Revoke')}
														onclick={() => revokePw(p)}
													>
														<Icon name="trash-alt" />
													</button>
												{/if}
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						{/if}
					</div>
				{/if}
			</div>
		{/if}

		<!-- Change password -->
		{#if showPasswordCard}
			<form class="card password-card" data-testid="profile-password-form" onsubmit={savePassword}>
				<h2><Icon name="key" /> {t('profile.change_password', 'Change Password')}</h2>
				<label>
					<span>{t('profile.current_password', 'Current Password')}</span>
					<input
						type="password"
						data-testid="profile-current-password-input"
						bind:value={currentPw}
						autocomplete="current-password"
					/>
				</label>
				<label>
					<span>{t('profile.new_password', 'New Password')}</span>
					<input
						type="password"
						data-testid="profile-new-password-input"
						bind:value={newPw}
						minlength="8"
						autocomplete="new-password"
					/>
					<small class="muted">{t('profile.min_8_chars', 'At least 8 characters')}</small>
				</label>
				<label>
					<span>{t('profile.confirm_password', 'Confirm New Password')}</span>
					<input
						type="password"
						data-testid="profile-confirm-password-input"
						bind:value={confirmPw}
						minlength="8"
						autocomplete="new-password"
					/>
				</label>
				<button type="submit" data-testid="profile-update-password-btn" disabled={savingPassword}>
					{t('profile.update_password', 'Update Password')}
				</button>
			</form>
		{/if}

		<!--
			OIDC identity link / unlink card. See
			docs/plan/oidc-account-linking.md § UX flow.

			Two mutually-exclusive states: Connect (no federation yet) or
			Disconnect (currently OIDC-linked). The Connect button
			navigates to the IdP; the Disconnect button unlinks and
			refreshes the session. Backend enforces safety checks —
			email-match on link, no-alternative-auth refusal on unlink.
		-->
		{#if canConnectSso}
			<form class="card sso-card" data-testid="profile-sso-connect-card" onsubmit={onConnectSso}>
				<h2><Icon name="link" /> {t('profile.sso_connect_title', 'Connect Single Sign-On')}</h2>
				<p>
					{t(
						'profile.sso_connect_description',
						{ provider: oidcProviderName },
						'Link your account to {{provider}} so you can sign in with SSO instead of your password.'
					)}
				</p>
				<button type="submit" data-testid="profile-sso-connect-btn" disabled={ssoBusy}>
					{t(
						'profile.sso_connect_button',
						{ provider: oidcProviderName },
						'Connect with {{provider}}'
					)}
				</button>
			</form>
		{:else if canDisconnectSso}
			<form
				class="card sso-card"
				data-testid="profile-sso-disconnect-card"
				onsubmit={onDisconnectSso}
			>
				<h2><Icon name="link" /> {t('profile.sso_disconnect_title', 'Single Sign-On')}</h2>
				<p>
					{t(
						'profile.sso_disconnect_description',
						{ provider: oidcProviderName },
						'Your account is connected to {{provider}}. Disconnecting will require you to sign in with your password from now on.'
					)}
				</p>
				<button type="submit" data-testid="profile-sso-disconnect-btn" disabled={ssoBusy}>
					{t('profile.sso_disconnect_button', 'Disconnect Single Sign-On')}
				</button>
			</form>
		{/if}
	{:else}
		<p>{t('common.loading', 'Loading…')}</p>
	{/if}
</main>

<style>
	.profile {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1.5rem 1rem;
		display: flex;
		flex-direction: column;
		gap: 1.5rem;
	}

	.card {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		padding: 1.5rem;
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
	}

	/*
	 * Mandatory-mode: hide every card except the identity header
	 * (`.avatar-card` keeps context — who am I?) and the password
	 * form. Backend blocks non-allowlisted endpoints with 403 anyway;
	 * this is the UX side of that lock so the user sees exactly one
	 * form to fill in. Ergonomically loud banner + a single card.
	 */
	.profile--mandatory :global(.card):not(.avatar-card, .password-card) {
		display: none;
	}

	.mandatory-banner {
		display: flex;
		align-items: flex-start;
		gap: 0.75rem;
		padding: 1rem 1.25rem;
		background: var(--color-bg-warning-subtle, var(--color-bg-surface));
		border: 1px solid var(--color-border-warning, var(--color-border));
		border-left: 4px solid var(--color-accent-warning, var(--color-accent));
		border-radius: var(--radius-md, var(--radius-lg));
		color: var(--color-text);
	}

	.mandatory-banner__body strong {
		display: block;
		margin-bottom: 0.25rem;
	}

	.mandatory-banner__body p {
		margin: 0;
		font-size: 0.9rem;
		color: var(--color-text-muted);
	}

	.card h2 {
		margin: 0 0 0.25rem;
		font-size: 1.125rem;
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.avatar-section {
		display: flex;
		align-items: center;
		gap: 1.25rem;
	}

	.avatar-lg {
		width: 72px;
		height: 72px;
		border-radius: 50%;
		object-fit: cover;
		flex: none;
	}

	.avatar-lg--initials {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		background: var(--color-accent-gradient, var(--color-accent));
		color: var(--color-on-accent);
		font-size: 1.5rem;
		font-weight: var(--weight-bold);
	}

	.avatar-info {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		flex: 1;
		min-width: 0;
	}

	.avatar-info h2 {
		margin: 0;
		font-size: 1.25rem;
	}

	.avatar-edit-btn {
		align-self: flex-start;
		flex: none;
	}

	.role-badge {
		display: inline-flex;
		align-items: center;
		gap: 0.375rem;
		align-self: flex-start;
		padding: 0.1rem 0.55rem;
		border-radius: var(--radius-full);
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold, 600);
		background: var(--color-bg-muted);
		color: var(--color-text);
	}

	.role-badge--admin {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.avatar-edit {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding-top: 1rem;
		border-top: 1px solid var(--color-border);
	}

	.avatar-tabs {
		display: flex;
		gap: 0.5rem;
	}

	.avatar-tab {
		padding: 0.35rem 0.75rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.avatar-tab--active {
		background: var(--color-bg-hover);
		color: var(--color-text);
	}

	.avatar-file-label {
		display: inline-flex;
		align-items: center;
		gap: 0.5rem;
		align-self: flex-start;
		cursor: pointer;
	}

	.avatar-preview {
		width: 96px;
		height: 96px;
		border-radius: 50%;
		object-fit: cover;
	}

	.avatar-edit-actions {
		display: flex;
		gap: 0.5rem;
		flex-wrap: wrap;
		align-items: center;
	}

	.info-grid {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 1rem;
	}

	.info-item {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.info-label {
		display: flex;
		align-items: center;
		gap: 0.375rem;
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.info-value {
		font-weight: var(--weight-medium, 500);
		overflow-wrap: break-word;
	}

	.storage-stats {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 0.5rem;
		text-align: center;
	}

	.storage-stat {
		display: flex;
		flex-direction: column;
		gap: 0.125rem;
		padding: 0.75rem 0.5rem;
		background: var(--color-bg-muted);
		border-radius: var(--radius-md);
	}

	.stat-value {
		font-size: 1.25rem;
		font-weight: var(--weight-bold, 700);
	}

	.bar {
		height: 8px;
		background: var(--color-bg-muted);
		border-radius: var(--radius-full);
		overflow: hidden;
	}

	.bar__fill {
		height: 100%;
	}

	.bar__fill--green {
		background: var(--color-success-text, var(--color-accent));
	}

	.bar__fill--orange {
		background: var(--color-warning-text);
	}

	.bar__fill--red {
		background: var(--color-danger-text);
	}

	.muted {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		margin: 0;
	}

	.alert {
		display: flex;
		align-items: flex-start;
		gap: 0.5rem;
		padding: 0.75rem 1rem;
		border-radius: var(--radius-md);
	}

	.alert--info {
		background: var(--color-info-bg);
		color: var(--color-info-text);
	}

	.app-pw-create {
		display: flex;
		gap: 0.5rem;
		flex-wrap: wrap;
	}

	.app-pw-create input {
		flex: 1;
		min-width: 12rem;
	}

	.generated {
		display: flex;
		flex-direction: column;
		gap: 0.375rem;
		padding: 0.75rem;
		background: var(--color-bg-hover);
		border-radius: var(--radius-md);
	}

	.generated__value {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
	}

	.generated code {
		font-family: var(--font-mono, monospace);
	}

	.pw-table {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--text-sm);
	}

	.pw-table th,
	.pw-table td {
		text-align: left;
		padding: 0.4rem 0.5rem;
		border-bottom: 1px solid var(--color-border);
	}

	.pw-table th {
		color: var(--color-text-muted);
		font-weight: var(--weight-semibold, 600);
	}

	.badge {
		display: inline-block;
		padding: 0.05rem 0.45rem;
		border-radius: var(--radius-sm);
		font-size: var(--text-xs, 0.7rem);
		font-weight: var(--weight-semibold, 600);
	}

	.badge--active {
		background: var(--color-success-bg, var(--color-bg-muted));
		color: var(--color-success-text, var(--color-text));
	}

	.badge--revoked {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.badge--count {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.app-pw-auto {
		padding-top: 0.5rem;
		border-top: 1px solid var(--color-border);
	}

	.app-pw-auto__toggle {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		background: none;
		border: none;
		color: var(--color-text);
		cursor: pointer;
		font-size: 1rem;
		padding: 0.25rem 0;
	}

	form {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	label {
		display: flex;
		flex-direction: column;
		gap: 0.375rem;
		font-size: 0.875rem;
		color: var(--color-text);
	}

	label.checkbox {
		flex-direction: row;
		align-items: center;
		gap: 0.5rem;
	}

	input,
	select {
		padding: 0.5rem 0.625rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: 1rem;
	}

	label.checkbox input {
		width: auto;
	}

	.btn {
		display: inline-flex;
		align-items: center;
		gap: 0.375rem;
		padding: 0.5rem 0.875rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.btn-secondary {
		background: var(--color-bg-hover);
	}

	.btn-primary {
		background: var(--color-primary);
		color: var(--color-text-light);
		border-color: transparent;
	}

	.btn-action {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		padding: 0.3rem;
		border: none;
		border-radius: var(--radius-md);
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.btn-action--danger {
		color: var(--color-danger-alt);
	}

	button[type='submit'] {
		align-self: flex-start;
		padding: 0.5rem 1.25rem;
		border: none;
		border-radius: var(--radius-md);
		background: var(--color-primary);
		color: var(--color-text-light);
		cursor: pointer;
	}

	.link-btn {
		background: none;
		border: none;
		color: var(--color-primary);
		cursor: pointer;
		font-size: 0.8125rem;
	}

	.link-btn--danger {
		color: var(--color-danger-text);
	}

	@media (width <= 32rem) {
		.info-grid {
			grid-template-columns: 1fr;
		}
	}
</style>
