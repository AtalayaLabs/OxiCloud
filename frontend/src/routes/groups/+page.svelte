<script lang="ts">
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { onMount } from 'svelte';
	import {
		addGroupMember,
		addUserMember,
		createGroup,
		deleteGroup,
		INTERNAL_GROUP_ID,
		listGroupsPage,
		listMembers,
		removeGroupMember,
		removeUserMember,
		renameGroup,
		type GroupItem,
		type GroupMember
	} from '$lib/api/endpoints/groups';
	import { groupDescription, groupDisplayName, groupIconName } from '$lib/utils/groupFormat';
	import {
		ensureResolvers,
		resolveRecipient,
		searchRecipients,
		type Recipient
	} from '$lib/api/endpoints/recipients';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { promptDialog } from '$lib/stores/dialogs.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	const PAGE_SIZE = 50;

	let groups = $state<GroupItem[]>([]);
	let total = $state(0);
	let loading = $state(false);
	let loadingMore = $state(false);
	let error = $state<string | null>(null);
	let expandedId = $state<string | null>(null);
	let members = $state<GroupMember[]>([]);
	let resolverReady = $state(false);

	const hasMore = $derived(groups.length < total);

	// Add-member combobox state (scoped to the expanded group)
	let addQuery = $state('');
	let addResults = $state<Recipient[]>([]);
	let addBusy = $state(false);
	let searchTimer: ReturnType<typeof setTimeout> | null = null;

	async function load() {
		loading = true;
		error = null;
		try {
			const page = await listGroupsPage(PAGE_SIZE, 0);
			groups = page.items;
			total = page.total;
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loading = false;
		}
	}

	async function loadMore() {
		loadingMore = true;
		try {
			const page = await listGroupsPage(PAGE_SIZE, groups.length);
			groups = [...groups, ...page.items];
			total = page.total;
		} catch (e) {
			report(e);
		} finally {
			loadingMore = false;
		}
	}

	function report(e: unknown) {
		errorToast(e);
	}

	/** Localised "(N members)" label. Project i18n has no plural rules, so we
	 *  branch on the three named forms ourselves. */
	function memberCountLabel(count: number): string {
		if (count === 0) return t('groups.member_count_zero', 'no members');
		if (count === 1) return t('groups.member_count_one', '1 member');
		return t('groups.member_count_other', { count }, '{{count}} members');
	}

	/** Display info for a member row; falls back to the raw id until caches load. */
	function memberInfo(m: GroupMember): { label: string; sublabel?: string } {
		// resolverReady gates re-resolution once the caches are populated.
		void resolverReady;
		const r = resolveRecipient(m.kind, m.id);
		return { label: r.label, sublabel: r.sublabel };
	}

	async function expand(g: GroupItem) {
		addQuery = '';
		addResults = [];
		if (expandedId === g.id) {
			expandedId = null;
			return;
		}
		expandedId = g.id;
		try {
			members = await listMembers(g.id);
		} catch (e) {
			report(e);
			members = [];
		}
	}

	async function onCreate() {
		const name = await promptDialog({
			title: t('groups.create_dialog_title', 'New group'),
			placeholder: t('groups.name_placeholder', 'engineering'),
			confirmText: t('actions.create', 'Create')
		});
		if (!name || !name.trim()) return;
		try {
			await createGroup(name.trim());
			await load();
		} catch (e) {
			report(e);
		}
	}

	async function onRename(g: GroupItem) {
		const name = await promptDialog({
			title: t('groups.edit_dialog_title', 'Rename group'),
			defaultValue: g.name,
			placeholder: t('groups.name_placeholder', 'engineering'),
			confirmText: t('actions.rename', 'Rename')
		});
		if (!name || !name.trim() || name.trim() === g.name) return;
		try {
			await renameGroup(g.id, name.trim());
			await load();
		} catch (e) {
			report(e);
		}
	}

	async function onDelete(g: GroupItem) {
		// Typed-name confirmation: the user must type the exact group name.
		const typed = await promptDialog({
			title: t('groups.delete_group', 'Delete group'),
			message: t(
				'groups.delete_confirm',
				{ name: g.name },
				'Delete the group "{{name}}"? Type the group name to confirm.'
			),
			placeholder: g.name,
			confirmText: t('actions.delete', 'Delete')
		});
		if (typed === null) return;
		if (typed !== g.name) {
			ui.notify(
				t('groups.delete_confirm_mismatch', 'Type the group name exactly to confirm.'),
				'error'
			);
			return;
		}
		try {
			await deleteGroup(g.id);
			if (expandedId === g.id) expandedId = null;
			await load();
		} catch (e) {
			report(e);
		}
	}

	function onAddQuery() {
		if (searchTimer) clearTimeout(searchTimer);
		const q = addQuery;
		if (!q.trim()) {
			addResults = [];
			return;
		}
		searchTimer = setTimeout(async () => {
			addBusy = true;
			try {
				const all = await searchRecipients(q);
				// Don't offer the group as a member of itself, or current members.
				const existing = new Set(members.map((m) => m.id));
				addResults = all.filter((r) => r.id !== expandedId && !existing.has(r.id));
			} catch (e) {
				report(e);
			} finally {
				addBusy = false;
			}
		}, 200);
	}

	async function pickMember(g: GroupItem, r: Recipient) {
		try {
			if (r.type === 'group') await addGroupMember(g.id, r.id);
			else await addUserMember(g.id, r.id);
			addQuery = '';
			addResults = [];
			members = await listMembers(g.id);
		} catch (e) {
			report(e);
		}
	}

	async function onRemoveMember(groupId: string, m: GroupMember) {
		try {
			if (m.kind === 'user') await removeUserMember(groupId, m.id);
			else await removeGroupMember(groupId, m.id);
			members = await listMembers(groupId);
		} catch (e) {
			report(e);
		}
	}

	onMount(async () => {
		await load();
		try {
			await ensureResolvers();
			resolverReady = true;
		} catch {
			/* names fall back to ids */
		}
	});
</script>

<svelte:head><title>{t('nav.groups', 'Groups')} · OxiCloud</title></svelte:head>

<main class="groups">
	<header class="groups__head">
		<h1>{t('nav.groups', 'Groups')}</h1>
		<button class="btn btn--primary" data-testid="groups-create-btn" onclick={onCreate}
			>{t('groups.create', 'Create group')}</button
		>
	</header>

	{#if error}
		<p class="status status--error">{error}</p>
	{:else if loading}
		<p class="status">{t('common.loading', 'Loading…')}</p>
	{:else if groups.length === 0}
		<p class="status">{t('groups.empty', 'No groups yet.')}</p>
	{:else}
		<ul class="list">
			{#each groups as g (g.id)}
				{@const description = groupDescription(g)}
				<li class="group">
					<div class="group__row">
						<button
							class="group__name"
							data-testid={`groups-expand-${g.id}`}
							onclick={() => expand(g)}
						>
							<span class="avatar"><Icon name={groupIconName(g)} /></span>
							<span class="group__text">
								<span class="group__title">
									{groupDisplayName(g)}
									{#if g.is_virtual}<span class="badge badge--system"
											>{t('groups.virtual_badge', 'System')}</span
										>{/if}
								</span>
								{#if description}<span class="muted">{description}</span>{/if}
								{#if !g.is_virtual && g.member_count != null}
									<span class="muted">{memberCountLabel(g.member_count)}</span>
								{/if}
							</span>
						</button>
						{#if g.can_manage !== false && !g.is_virtual}
							<div class="group__actions">
								<button
									class="link-btn"
									data-testid={`groups-rename-${g.id}`}
									onclick={() => onRename(g)}>{t('common.rename', 'Rename')}</button
								>
								<button
									class="link-btn link-btn--danger"
									data-testid={`groups-delete-${g.id}`}
									onclick={() => onDelete(g)}
								>
									{t('common.delete', 'Delete')}
								</button>
							</div>
						{/if}
					</div>

					{#if expandedId === g.id}
						<div class="members" data-testid={`groups-members-panel-${g.id}`}>
							<div class="members__head">
								<h2>{t('groups.members', 'Members')}</h2>
							</div>

							{#if g.can_manage !== false && !g.is_virtual}
								<div class="add-member">
									<input
										class="add-member__input"
										data-testid="groups-member-add-input"
										placeholder={t('groups.add_member_search', 'Search users or groups to add…')}
										bind:value={addQuery}
										oninput={onAddQuery}
									/>
									{#if addBusy}
										<p class="muted">{t('common.loading', 'Loading…')}</p>
									{:else if addResults.length > 0}
										<ul class="add-member__results" data-testid="groups-member-add-results">
											{#each addResults as r (r.type + r.id)}
												<li>
													<button
														class="add-member__opt"
														data-testid={`groups-member-add-opt-${r.type}-${r.id}`}
														onclick={() => pickMember(g, r)}
													>
														<span class="avatar avatar--sm">
															<Icon name={r.type === 'group' ? 'user-group' : 'user'} />
														</span>
														<span class="vignette__text">
															<span class="vignette__name">{r.label}</span>
															{#if r.sublabel}<span class="muted">{r.sublabel}</span>{/if}
														</span>
													</button>
												</li>
											{/each}
										</ul>
									{/if}
								</div>
							{:else if g.id === INTERNAL_GROUP_ID}
								<p class="muted">
									{t('groups.virtual_internal_explanation', 'Every internal user on this server.')}
								</p>
							{/if}

							{#if members.length === 0}
								{#if !(g.id === INTERNAL_GROUP_ID)}
									<p class="muted">{t('groups.no_members', 'No members.')}</p>
								{/if}
							{:else}
								<ul class="members__list">
									{#each members as m (m.kind + m.id)}
										{@const info = memberInfo(m)}
										<li class="vignette">
											<span class="avatar avatar--sm">
												<Icon name={m.kind === 'group' ? 'user-group' : 'user'} />
											</span>
											<span class="vignette__text">
												<span class="vignette__name">
													{info.label}
													{#if m.kind === 'group'}<span class="badge badge--nested"
															>{t('groups.nested', 'Group')}</span
														>{/if}
												</span>
												{#if info.sublabel}<span class="muted">{info.sublabel}</span>{/if}
											</span>
											{#if g.can_manage !== false && !g.is_virtual}
												<button
													class="link-btn link-btn--danger"
													data-testid={`groups-member-remove-${m.kind}-${m.id}`}
													onclick={() => onRemoveMember(g.id, m)}
												>
													{t('common.remove', 'Remove')}
												</button>
											{/if}
										</li>
									{/each}
								</ul>
							{/if}
						</div>
					{/if}
				</li>
			{/each}
		</ul>

		{#if hasMore}
			<button
				class="btn load-more"
				data-testid="groups-load-more-btn"
				disabled={loadingMore}
				onclick={loadMore}
			>
				{loadingMore ? t('common.loading', 'Loading…') : t('groups.load_more', 'Load more')}
			</button>
		{/if}
	{/if}
</main>

<style>
	.groups {
		max-width: 48rem;
		margin: 0 auto;
		padding: 1.5rem 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.groups__head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}

	.groups__head h1 {
		margin: 0;
		font-size: 1.5rem;
	}

	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.group {
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
	}

	.group__row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0.75rem;
		gap: 0.5rem;
	}

	.group__name {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		background: none;
		border: none;
		color: var(--color-text);
		cursor: pointer;
		font-size: 1rem;
		text-align: left;
		flex: 1;
		min-width: 0;
	}

	.group__text {
		display: flex;
		flex-direction: column;
		gap: 0.125rem;
		min-width: 0;
	}

	.group__title {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.group__actions {
		display: flex;
		gap: 0.5rem;
		flex-shrink: 0;
	}

	.members {
		border-top: 1px solid var(--color-border);
		padding: 0.75rem;
	}

	.members__head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}

	.members__head h2 {
		margin: 0;
		font-size: 1rem;
	}

	.members__list {
		list-style: none;
		margin: 0.5rem 0 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.members__list li {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}

	.vignette {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1) 0;
	}

	.vignette__text {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-width: 0;
		text-align: left;
	}

	.vignette__name {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.avatar {
		display: grid;
		place-items: center;
		width: 2rem;
		height: 2rem;
		border-radius: 50%;
		background: var(--color-accent-bg, var(--color-bg-muted));
		color: var(--color-accent-text, var(--color-text));
		font-size: var(--text-xs, 0.75rem);
		flex-shrink: 0;
	}

	.avatar--sm {
		width: 1.6rem;
		height: 1.6rem;
	}

	.badge {
		display: inline-block;
		padding: 0.05rem 0.4rem;
		border-radius: var(--radius-sm);
		font-size: var(--text-xs, 0.7rem);
		font-weight: var(--weight-semibold, 600);
	}

	.badge--system {
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
	}

	.badge--nested {
		background: var(--color-info-bg);
		color: var(--color-info-text);
	}

	.add-member {
		position: relative;
		margin: var(--space-2) 0;
	}

	.add-member__input {
		width: 100%;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.add-member__results {
		list-style: none;
		margin: var(--space-1) 0 0;
		padding: 0;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		max-height: 16rem;
		overflow: auto;
	}

	.add-member__opt {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		text-align: left;
	}

	.add-member__opt:hover {
		background: var(--color-bg-hover);
	}

	.muted {
		color: var(--color-text-muted);
		font-size: 0.8125rem;
	}

	.status {
		color: var(--color-text-muted);
		padding: 2rem 0;
		text-align: center;
	}

	.status--error {
		color: var(--color-danger-text);
	}

	.btn {
		padding: 0.5rem 0.875rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.btn--primary {
		background: var(--color-primary);
		color: var(--color-text-light);
		border-color: transparent;
	}

	.load-more {
		align-self: center;
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
</style>
