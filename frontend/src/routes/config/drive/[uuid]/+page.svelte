<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { onMount } from 'svelte';

	import { goto } from '$app/navigation';

	import { deleteDrive, listDriveMembers } from '$lib/api/endpoints/drives';
	import { renameFolder } from '$lib/api/endpoints/folders';
	import { errorToast } from '$lib/utils/errors';
	import { ui } from '$lib/stores/ui.svelte';
	import type { Drive, DriveMember, DriveRole, DrivePoliciesPartial } from '$lib/api/types';
	import PolicyList from '$lib/components/PolicyList.svelte';
	import ReadOnlyBanner from '$lib/components/ReadOnlyBanner.svelte';
	import ShareDialog from '$lib/components/ShareDialog.svelte';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import GroupVignette from '$lib/components/GroupVignette.svelte';
	import { ensureResolvers } from '$lib/api/endpoints/recipients';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { drives as drivesStore, driveIcon } from '$lib/stores/drives.svelte';
	import { formatDate } from '$lib/utils/display';
	import { formatBytes } from '$lib/utils/format';
	import { readAllPolicies } from '$lib/utils/drivePolicies';

	const uuid = $derived(page.params.uuid ?? '');
	const drive = $derived<Drive | null>(drivesStore.findById(uuid));

	let members = $state<DriveMember[]>([]);
	let membersLoaded = $state(false);
	let membersError = $state<string | null>(null);

	// Mutation controls are gated by *both* caller_role AND drive kind:
	// even an Owner of a personal drive can't change membership (the
	// backend guard refuses), so the UI hides the controls upfront for
	// honest UX. Shared drives + Owner role → full controls.
	const canManageMembers = $derived(drive?.kind === 'shared' && drive?.caller_role === 'owner');

	// Rename is allowed for any Owner (both shared and personal), since
	// the backend requires `Permission::Manage` on the drive's root
	// folder which only the Owner bundle carries. Personal-drive Owners
	// are the user themselves (seeded by the lifecycle hook).
	const canRename = $derived(drive?.caller_role === 'owner');

	// Delete is allowed for Owners — backend additionally refuses the
	// default Personal drive (405) and non-empty drives (409). We hide
	// the button on the default-personal drive so the affordance only
	// appears when it can actually succeed.
	const canDelete = $derived(drive?.caller_role === 'owner' && !drive?.default_for_user);

	let deleting = $state(false);

	async function confirmAndDelete() {
		if (!drive) return;
		const confirmText = t(
			'drive.delete_confirm',
			{ name: drive.name },
			'Delete drive "{{name}}"? This cannot be undone — the drive ' +
				'must be empty first or the server will refuse.'
		);
		if (typeof window === 'undefined' || !window.confirm(confirmText)) return;
		deleting = true;
		try {
			await deleteDrive(drive.id);
			await drivesStore.refresh();
			ui.notify(t('drive.deleted', 'Drive deleted.'), 'success');
			// Send the user back to /files. The picker's reload above
			// already removed the now-deleted drive from the sidebar.
			await goto(resolve('/files'));
		} catch (e) {
			// 409 (non-empty) and 405 (default personal) come back as
			// thrown errors with the server's detail in the message —
			// surface as a toast rather than a silent failure.
			errorToast(e);
		} finally {
			deleting = false;
		}
	}

	// Inline rename state. `renameDraft` shadows `drive.name` while the
	// input is open; we don't write back to the store until the server
	// accepts the change. `renameBusy` disables the save/cancel buttons
	// during the round-trip.
	let renaming = $state(false);
	let renameDraft = $state('');
	let renameBusy = $state(false);

	function startRename() {
		if (!drive) return;
		renameDraft = drive.name;
		renaming = true;
	}

	function cancelRename() {
		renaming = false;
		renameDraft = '';
	}

	async function saveRename() {
		if (!drive) return;
		const next = renameDraft.trim();
		if (next.length === 0 || next === drive.name) {
			cancelRename();
			return;
		}
		renameBusy = true;
		try {
			// Drive name = root folder name (drive.md §3); rename via the
			// folder endpoint. Backend promotes the perm to Manage for
			// parent_id IS NULL, so a non-Owner caller would 404 here
			// (but the UI also hid this button for non-Owners).
			await renameFolder(drive.root_folder_id, next);
			await drivesStore.refresh();
			renaming = false;
		} catch (e) {
			errorToast(e);
		} finally {
			renameBusy = false;
		}
	}

	function roleLabel(role: DriveRole): string {
		switch (role) {
			case 'owner':
				return t('drive.role.owner', 'Owner');
			case 'editor':
				return t('drive.role.editor', 'Editor');
			case 'viewer':
				return t('drive.role.viewer', 'Viewer');
		}
	}

	// `shareDialogOpen` drives the ShareDialog modal — the same dialog
	// used for file/folder sharing, parameterised with `kind: 'drive'`
	// + `allowLinks: false`. Add/change-role/remove flow through the
	// dialog's existing grants plumbing (server-side those routes
	// dispatch to `DriveManagementService`).
	let shareDialogOpen = $state(false);

	// `dialogItem` is recomputed from the drive so the dialog title
	// reflects renames.
	const dialogItem = $derived(
		drive ? { id: drive.id, name: drive.name, kind: 'drive' as const } : null
	);

	async function loadMembers() {
		if (!uuid) return;
		try {
			members = await listDriveMembers(uuid);
		} catch (e) {
			// 404 here means the caller lacks Read on the drive — which is
			// also what the parent "Drive not found" card already conveys.
			// Keep the listing area empty rather than surfacing a noisy toast.
			membersError = e instanceof Error ? e.message : String(e);
			members = [];
		} finally {
			membersLoaded = true;
		}
	}

	// Refresh the on-page member list on every dialog mutation (add,
	// role change, remove). ShareDialog fires `onchange` for the full
	// set of grant mutations — `onshared` only covers creation, which
	// would leave role-change and removal stale here.
	function onShareDialogChange() {
		void loadMembers();
	}

	const kindLabel = $derived.by(() => {
		if (!drive) return '';
		return drive.kind === 'shared'
			? t('drive.kind_shared', 'Shared drive')
			: t('drive.kind_personal', 'Personal drive');
	});

	const storagePct = $derived.by(() => {
		if (!drive || !drive.quota_bytes || drive.quota_bytes <= 0) return 0;
		return Math.min(100, (drive.used_bytes / drive.quota_bytes) * 100);
	});

	// Drive policies are OxiCloud-admin-only for mutation (§8), but
	// visible read-only here so members understand what rules apply to
	// the drive they're on. The admin's "Manage policies" modal on
	// `/admin` is the only editor. `readAllPolicies` normalises the raw
	// JSONB bag into a `Required<DrivePoliciesPartial>` — unknown keys
	// (or missing ones) resolve to `false`.
	const drivePoliciesView = $derived<Required<DrivePoliciesPartial>>(
		readAllPolicies((drive?.policies ?? {}) as Record<string, unknown>)
	);

	onMount(() => {
		void drivesStore.load();
		// Preload the recipient caches (users + groups) so the members
		// list can render group names + user labels synchronously. Both
		// caches are module-level and shared across surfaces.
		void ensureResolvers();
	});

	// SvelteKit reuses this component when navigating between
	// `/config/drive/<A>` and `/config/drive/<B>` (same route, different
	// dynamic param), so `onMount` only fires once. Re-run the members
	// fetch whenever `uuid` changes — without this, the previous drive's
	// rows linger until a hard refresh. Resetting `members` + the loaded
	// flag first prevents the brief flash of stale data before the new
	// fetch returns.
	$effect(() => {
		const id = uuid;
		members = [];
		membersLoaded = false;
		membersError = null;
		if (id) void loadMembers();
	});
</script>

<div class="config-drive">
	{#if !drivesStore.loaded}
		<p class="muted">{t('common.loading', 'Loading…')}</p>
	{:else if !drive}
		<div class="card">
			<h2>{t('drive.not_found_title', 'Drive not found')}</h2>
			<p class="muted">
				{t('drive.not_found_body', "This drive doesn't exist or you don't have access to it.")}
			</p>
			<a class="link" href={resolve('/files')}>{t('drive.back_to_files', 'Back to Files')}</a>
		</div>
	{:else}
		<div class="drive-title">
			<Icon name={driveIcon(drive)} />
			{#if renaming}
				<input
					class="drive-title__input"
					type="text"
					data-testid="drive-rename-input"
					bind:value={renameDraft}
					maxlength="200"
					disabled={renameBusy}
					onkeydown={(e) => {
						if (e.key === 'Enter') void saveRename();
						else if (e.key === 'Escape') cancelRename();
					}}
				/>
				<button
					type="button"
					class="icon-btn"
					data-testid="drive-rename-save-btn"
					title={t('common.save', 'Save')}
					aria-label={t('common.save', 'Save')}
					onclick={() => void saveRename()}
					disabled={renameBusy}
				>
					<Icon name="check" />
				</button>
				<button
					type="button"
					class="icon-btn"
					data-testid="drive-rename-cancel-btn"
					title={t('common.cancel', 'Cancel')}
					aria-label={t('common.cancel', 'Cancel')}
					onclick={cancelRename}
					disabled={renameBusy}
				>
					<Icon name="times" />
				</button>
			{:else}
				<h1 class="drive-title__name">{drive.name}</h1>
				{#if canRename}
					<button
						type="button"
						class="icon-btn"
						data-testid="drive-rename-edit-btn"
						title={t('drive.rename', 'Rename drive')}
						aria-label={t('drive.rename', 'Rename drive')}
						onclick={startRename}
					>
						<Icon name="pencil-alt" />
					</button>
				{/if}
			{/if}
		</div>

		{#if drivePoliciesView.read_only}
			<ReadOnlyBanner />
		{/if}

		<div class="card">
			<h2><Icon name="info-circle" /> {t('drive.info', 'Drive info')}</h2>
			<dl class="info-grid">
				<dt>{t('drive.field.kind', 'Kind')}</dt>
				<dd>{kindLabel}</dd>

				{#if drive.default_for_user}
					<dt>{t('drive.field.default', 'Default')}</dt>
					<dd>{t('drive.field.default_yes', 'This is your home drive')}</dd>
				{/if}

				<dt>{t('drive.field.created', 'Created')}</dt>
				<dd>{formatDate(drive.created_at)}</dd>

				<dt>{t('drive.field.updated', 'Last updated')}</dt>
				<dd>{formatDate(drive.updated_at)}</dd>

				<dt>{t('drive.field.id', 'Identifier')}</dt>
				<dd class="mono">{drive.id}</dd>
			</dl>
		</div>

		<div class="card">
			<h2><Icon name="hdd" /> {t('drive.storage', 'Storage')}</h2>
			<div class="storage-row">
				<div class="storage-stat">
					<div class="storage-stat__value">{formatBytes(drive.used_bytes)}</div>
					<div class="storage-stat__label">{t('drive.used', 'Used')}</div>
				</div>
				<div class="storage-stat">
					<div class="storage-stat__value">
						{drive.quota_bytes && drive.quota_bytes > 0 ? formatBytes(drive.quota_bytes) : '∞'}
					</div>
					<div class="storage-stat__label">{t('drive.quota', 'Quota')}</div>
				</div>
				<div class="storage-stat">
					<div class="storage-stat__value">
						{drive.quota_bytes && drive.quota_bytes > 0 ? `${Math.round(storagePct)}%` : '—'}
					</div>
					<div class="storage-stat__label">{t('drive.usage', 'Usage')}</div>
				</div>
			</div>
			{#if drive.quota_bytes && drive.quota_bytes > 0}
				<div
					class="bar"
					role="progressbar"
					aria-valuemin="0"
					aria-valuemax="100"
					aria-valuenow={Math.round(storagePct)}
				>
					<div class="bar__fill" style:width="{storagePct}%"></div>
				</div>
			{/if}
		</div>

		<div class="card">
			<div class="members__header">
				<h2><Icon name="users" /> {t('drive.members', 'Members')}</h2>
				{#if canManageMembers}
					<button
						type="button"
						class="btn btn-primary"
						data-testid="drive-manage-members-btn"
						onclick={() => (shareDialogOpen = true)}
					>
						<Icon name="user-plus" />
						{t('drive.manage_members', 'Manage members')}
					</button>
				{/if}
			</div>

			{#if !membersLoaded}
				<p class="muted">{t('common.loading', 'Loading…')}</p>
			{:else if members.length === 0}
				<p class="muted">
					{membersError ?? t('drive.members_empty', 'No members.')}
				</p>
			{:else}
				<!-- Read-only summary. Add/change/remove happens inside the
				     ShareDialog modal opened by the button above; the inline
				     row controls used to live here have moved into the dialog
				     so the same flow handles file/folder + drive grants. -->
				<ul class="members">
					{#each members as m (m.id)}
						<li class="members__row">
							{#if m.subject.type === 'user'}
								<UserVignette userId={m.subject.id} />
							{:else if m.subject.type === 'group'}
								<GroupVignette groupId={m.subject.id} />
							{:else}
								<span class="members__token">
									<Icon name="link" />
									<span class="mono">{m.subject.id}</span>
								</span>
							{/if}
							<span class="members__role members__role--{m.role}">
								{roleLabel(m.role)}
							</span>
						</li>
					{/each}
				</ul>

				{#if !canManageMembers && drive.kind === 'personal'}
					<p class="muted members__personal-note">
						{t(
							'drive.members_personal_immutable',
							'Personal drives have a fixed single-owner membership.'
						)}
					</p>
				{/if}
			{/if}
		</div>

		<!-- Policies card — read-only summary of the current drive rules.
		     Content is dense (seven toggle rows), so the whole card folds
		     into a native `<details>` disclosure. Closed by default; the
		     admin-only mutation surface still lives on `/admin`. -->
		<details class="card policies-card">
			<summary class="policies-card__summary">
				<h2><Icon name="shield-alt" /> {t('drive.policies', 'Policies')}</h2>
				<span class="policies-card__caret" aria-hidden="true">
					<Icon name="chevron-down" />
				</span>
			</summary>
			<p class="muted">
				{t(
					'drive.policies_help',
					"Rules an OxiCloud admin has set for this drive. Only admins can change them; you're seeing the current state."
				)}
			</p>
			<PolicyList values={drivePoliciesView} readonly testIdPrefix="drive-policy" />
		</details>

		{#if canDelete}
			<!-- Danger zone: drive delete (D3b). Only rendered for Owners on
			     non-default drives. Backend enforces the empty-drive rule —
			     if the drive still has live content the request returns 409
			     with a message that surfaces as a toast. -->
			<div class="card danger-zone">
				<h2><Icon name="exclamation-triangle" /> {t('drive.danger_zone', 'Danger zone')}</h2>
				<p class="muted">
					{t(
						'drive.delete_hint',
						'Deleting a drive removes it permanently. The drive must be empty (no live files or folders) before delete is allowed.'
					)}
				</p>
				<button
					type="button"
					class="btn btn-danger"
					data-testid="drive-delete-btn"
					onclick={confirmAndDelete}
					disabled={deleting}
				>
					<Icon name="trash-alt" />
					{deleting ? t('common.deleting', 'Deleting…') : t('drive.delete', 'Delete drive')}
				</button>
			</div>
		{/if}
	{/if}
</div>

<!-- Drive members modal — reuses the same ShareDialog as file/folder
     sharing. `allowLinks={false}` hides the public-link tab because
     drives don't support shareable URLs (the backend service refuses
     token subjects on drive resources). -->
{#if dialogItem}
	<ShareDialog
		bind:open={shareDialogOpen}
		item={dialogItem}
		allowLinks={false}
		onchange={onShareDialogChange}
	/>
{/if}

<style>
	.config-drive {
		max-width: 800px;
		margin: 0 auto;
		padding: 1.5rem 1rem;
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}

	.config-drive h1 {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin: 0;
		font-size: 1.5rem;
		color: var(--color-text-heading);
	}

	.card {
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border-subtle);
		border-radius: var(--radius-md);
		padding: 1.25rem;
	}

	.card h2 {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin: 0 0 1rem;
		font-size: 1.05rem;
		color: var(--color-text-heading);
	}

	/* Policies card is a `<details>` disclosure — the summary bar carries
	   the h2 title on the left and a chevron on the right that rotates
	   when the section opens. Native `<details>` handles the interaction
	   (click / keyboard / accessible affordance) — no Svelte state
	   needed. */
	.policies-card__summary {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 1rem;
		cursor: pointer;
		list-style: none;
	}

	.policies-card__summary::-webkit-details-marker {
		/* Chrome/Safari: hide the default triangle so our chevron is the
		   only disclosure affordance. Firefox uses `list-style: none`
		   above. */
		display: none;
	}

	.policies-card__summary h2 {
		margin: 0;
	}

	.policies-card__caret {
		color: var(--color-text-muted);
		transition: transform 150ms ease;
	}

	details[open] > .policies-card__summary .policies-card__caret {
		transform: rotate(180deg);
	}

	/* When closed the summary is the entire card content, so we drop the
	   card's default bottom padding. When open the help paragraph +
	   policy list need breathing room from the summary — restore the
	   spacing by nudging the first child. */
	details.policies-card > .muted {
		margin-top: 1rem;
		margin-bottom: 0.75rem;
	}

	.info-grid {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 0.5rem 1.5rem;
		margin: 0;
	}

	.info-grid dt {
		color: var(--color-text-muted);
		font-size: 0.85rem;
	}

	.info-grid dd {
		margin: 0;
		color: var(--color-text);
	}

	.mono {
		font-family: var(--font-mono);
		font-size: 0.85rem;
	}

	.storage-row {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 1rem;
		margin-bottom: 1rem;
	}

	.storage-stat__value {
		font-size: 1.1rem;
		font-weight: var(--weight-semibold);
		color: var(--color-text);
	}

	.storage-stat__label {
		font-size: 0.8rem;
		color: var(--color-text-muted);
	}

	.bar {
		height: 6px;
		background: var(--color-bg-muted);
		border-radius: 3px;
		overflow: hidden;
	}

	.bar__fill {
		height: 100%;
		background: var(--color-accent);
		transition: width 200ms ease;
	}

	.muted {
		color: var(--color-text-muted);
	}

	.link {
		color: var(--color-accent);
		text-decoration: none;
	}

	.link:hover {
		text-decoration: underline;
	}

	/* Drive title row: icon + name (or inline rename input) + edit/save
	   affordances. Mirrors the visual weight of the previous static
	   <h1> so the page layout doesn't shift when entering rename mode. */
	.drive-title {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin-bottom: 1rem;
	}

	.drive-title__name {
		margin: 0;
	}

	.drive-title__input {
		flex: 1;
		min-width: 0;
		max-width: 28rem;
		padding: 0.4rem 0.6rem;
		font-size: 1.5rem;
		font-weight: var(--weight-semibold, 600);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	/* Danger zone card hosts the delete-drive button at the bottom of
	   the page. Border tint makes the destructive context unmissable
	   without hijacking the whole layout — same convention as
	   admin/users delete affordances. */
	.danger-zone {
		border-color: var(--color-error-text);
	}

	.danger-zone h2 {
		color: var(--color-error-text);
	}

	.btn-danger {
		display: inline-flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.5rem 0.875rem;
		border: 1px solid var(--color-error-text);
		border-radius: var(--radius-md);
		background: var(--color-error-text);
		color: var(--color-text-light);
		cursor: pointer;
	}

	.btn-danger:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.icon-btn--danger {
		color: var(--color-error-text);
	}

	/* Compact icon button used in the title row + nowhere else here.
	   The shared `.icon-btn` style isn't promoted to a global yet, so
	   we duplicate the minimum that this page needs. */
	.icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 2rem;
		height: 2rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.icon-btn:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}

	/* Members card header: title on the left, "Manage members" button on
	   the right when the caller can mutate membership. */
	.members__header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3, 0.75rem);
		margin-bottom: var(--space-3, 0.75rem);
	}

	.members__header h2 {
		margin: 0;
	}

	/* Members list */
	.members {
		list-style: none;
		padding: 0;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.members__row {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.5rem 0.75rem;
		border: 1px solid var(--color-border-faint);
		border-radius: var(--radius-sm);
		background: var(--color-bg-page);
	}

	.members__group,
	.members__token {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		flex: 1;
		min-width: 0;
		color: var(--color-text-secondary);
	}

	.members__role {
		display: inline-flex;
		align-items: center;
		padding: 0.2rem 0.65rem;
		border-radius: var(--radius-pill, 999px);
		font-size: 0.8rem;
		background: var(--color-bg-muted);
		color: var(--color-text-secondary);
		flex: none;
	}

	.members__role--owner {
		background: var(--color-accent-tint, var(--color-bg-muted));
		color: var(--color-accent-text, var(--color-text-secondary));
		font-weight: var(--weight-semibold);
	}

	.members__role--editor,
	.members__role--contributor {
		background: var(--color-accent-ring, var(--color-bg-muted));
		color: var(--color-accent-text, var(--color-text-secondary));
	}

	.members__role-select {
		flex: none;
		padding: 0.25rem 0.5rem;
		border-radius: var(--radius-sm);
		border: 1px solid var(--color-border);
		background: var(--color-bg-input);
		color: var(--color-text);
		font: inherit;
		font-size: 0.85rem;
		cursor: pointer;
	}

	.members__remove {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 28px;
		height: 28px;
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--color-text-faint);
		cursor: pointer;
	}

	.members__remove:hover {
		background: var(--color-bg-hover);
		color: var(--color-danger-text, var(--color-text));
	}

	.members__personal-note {
		margin-top: 0.75rem;
		font-size: 0.85rem;
	}
</style>
