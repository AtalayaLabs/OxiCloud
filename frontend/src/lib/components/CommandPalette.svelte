<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { logout } from '$lib/api/endpoints/auth';
	import { searchResources } from '$lib/api/endpoints/search';
	import { fileInlineUrl } from '$lib/api/endpoints/files';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { session } from '$lib/stores/session.svelte';
	import { theme } from '$lib/stores/theme.svelte';

	interface Command {
		id: string;
		label: string;
		icon: string;
		hint?: string;
		run: () => void;
	}

	// `autoOpen` lets a lazy host (AppShell) mount us already-open on the first
	// Cmd/Ctrl+K, since our own key listener only exists once we're mounted.
	let { autoOpen = false }: { autoOpen?: boolean } = $props();

	let open = $state(false);
	// Drives the enter animation: flipped on after mount so the overlay/panel
	// transition from their initial (faded/offset) state.
	let entered = $state(false);
	let query = $state('');
	let index = $state(0);
	let input = $state<HTMLInputElement | null>(null);
	let listEl = $state<HTMLUListElement | null>(null);
	let fileMatches = $state<Command[]>([]);
	let searchTimer: ReturnType<typeof setTimeout> | null = null;
	// Element focused before the palette opened, restored on close.
	let prevFocus: HTMLElement | null = null;

	// When mounted already-open (autoOpen), run the same enter sequence the
	// keyboard path uses. Guarded so it fires once, not on every reopen.
	let didAutoOpen = false;
	$effect(() => {
		if (didAutoOpen || !autoOpen) return;
		didAutoOpen = true;
		open = true;
		prevFocus = document.activeElement as HTMLElement | null;
		requestAnimationFrame(() => (entered = true));
		queueMicrotask(() => input?.focus());
	});

	function close() {
		open = false;
		entered = false;
		query = '';
		fileMatches = [];
		index = 0;
		prevFocus?.focus?.();
		prevFocus = null;
	}

	// The routes navigated to from the palette. Kept as an explicit literal union
	// so `resolve()` type-checks each path against the real route table.
	type NavPath =
		| '/files'
		| '/shared'
		| '/shared-with-me'
		| '/recent'
		| '/favorites'
		| '/photos'
		| '/music'
		| '/groups'
		| '/trash'
		| '/profile'
		| '/admin'
		| '/login'
		| `/files/${string}`;

	function nav(path: NavPath): Command['run'] {
		return () => {
			close();
			void goto(resolve(path));
		};
	}

	/**
	 * Trigger the file picker in the files view. The input lives in the files
	 * route, so we navigate there first and broadcast an event the page listens
	 * for. (Follow-up: wire `oxicloud:upload-files` in the files route page.)
	 */
	function uploadFiles() {
		close();
		void goto(resolve('/files')).then(() => {
			window.dispatchEvent(new CustomEvent('oxicloud:upload-files'));
		});
	}

	async function showAbout() {
		close();
		await confirmDialog({
			title: t('user_menu.about', 'About OxiCloud'),
			message: t(
				'about.description',
				'OxiCloud — a fast, self-hosted file storage and sync server.'
			),
			confirmText: t('common.ok', 'OK'),
			cancelText: t('common.close', 'Close')
		});
	}

	const baseCommands = $derived.by<Command[]>(() => {
		const cmds: Command[] = [
			{ id: 'files', label: t('nav.files', 'Files'), icon: 'folder', run: nav('/files') },
			{ id: 'shared', label: t('nav.shared', 'Shared'), icon: 'oxiexport', run: nav('/shared') },
			{
				id: 'swm',
				label: t('nav.shared_with_me', 'Shared with me'),
				icon: 'oxiimport',
				run: nav('/shared-with-me')
			},
			{ id: 'recent', label: t('nav.recent', 'Recent'), icon: 'clock', run: nav('/recent') },
			{ id: 'fav', label: t('nav.favorites', 'Favorites'), icon: 'star', run: nav('/favorites') },
			{ id: 'photos', label: t('nav.photos', 'Photos'), icon: 'images', run: nav('/photos') },
			{ id: 'music', label: t('nav.music', 'Music'), icon: 'music', run: nav('/music') },
			{ id: 'groups', label: t('nav.groups', 'Groups'), icon: 'users', run: nav('/groups') },
			{ id: 'trash', label: t('nav.trash', 'Trash'), icon: 'trash', run: nav('/trash') },
			{
				id: 'upload',
				label: t('actions.upload_files', 'Upload files'),
				icon: 'cloud-upload-alt',
				run: uploadFiles
			},
			{
				id: 'profile',
				label: t('user_menu.profile', 'Profile'),
				icon: 'user',
				run: nav('/profile')
			}
		];
		if (session.user?.role === 'admin') {
			cmds.push({
				id: 'admin',
				label: t('user_menu.admin_panel', 'Admin'),
				icon: 'shield-alt',
				run: nav('/admin')
			});
		}
		cmds.push(
			{
				id: 'theme',
				label: t('cmdk.toggle_theme', 'Toggle theme'),
				icon: 'moon',
				run: () => {
					theme.set(theme.current === 'dark' ? 'light' : 'dark');
					close();
				}
			},
			{
				id: 'about',
				label: t('user_menu.about', 'About'),
				icon: 'info-circle',
				run: showAbout
			},
			{
				id: 'logout',
				label: t('actions.logout', 'Log out'),
				icon: 'sign-out-alt',
				run: async () => {
					close();
					let postLogoutUrl: string | undefined;
					try {
						({ postLogoutUrl } = await logout());
					} catch {
						/* clear locally regardless */
					}
					if (postLogoutUrl) {
						// See AppShell::onLogout — `session.reset()` before this
						// races the layout $effect guard and the 401 handler.
						window.location.replace(postLogoutUrl);
						return;
					}
					session.reset();
					await goto(resolve('/login'));
				}
			}
		);
		return cmds;
	});

	const filtered = $derived.by<Command[]>(() => {
		const q = query.trim().toLowerCase();
		const base = q ? baseCommands.filter((c) => c.label.toLowerCase().includes(q)) : baseCommands;
		return [...base, ...fileMatches];
	});

	function runFileSearch() {
		if (searchTimer) clearTimeout(searchTimer);
		const q = query.trim();
		if (q.length < 2) {
			fileMatches = [];
			return;
		}
		searchTimer = setTimeout(async () => {
			try {
				const r = await searchResources(q, { recursive: true, limit: 8 });
				// Wire items are ordered folders-first-then-files, but demux
				// explicitly so the palette keeps the two-section layout even
				// when file hits dominate the result set.
				const folders: Command[] = r.items
					.filter((it) => it.resource_type === 'folder')
					.slice(0, 3)
					.map((it) => ({
						id: `fld-${it.resource.id}`,
						label: it.resource.name,
						icon: 'folder',
						hint: t('files.folder', 'Folder'),
						run: nav(`/files/${it.resource.id}`)
					}));
				const files: Command[] = r.items
					.filter((it) => it.resource_type === 'file')
					.slice(0, 5)
					.map((it) => ({
						id: `fil-${it.resource.id}`,
						label: it.resource.name,
						icon: 'file',
						hint: t('files.file', 'File'),
						run: () => {
							close();
							window.open(fileInlineUrl(it.resource.id), '_blank', 'noopener');
						}
					}));
				fileMatches = [...folders, ...files];
			} catch {
				fileMatches = [];
			}
		}, 250);
	}

	function scrollActiveIntoView() {
		queueMicrotask(() => {
			listEl?.querySelector('.cmdk__item.active')?.scrollIntoView({ block: 'nearest' });
		});
	}

	function onGlobalKey(e: KeyboardEvent) {
		if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
			e.preventDefault();
			if (open) {
				close();
			} else {
				prevFocus = document.activeElement as HTMLElement | null;
				open = true;
				requestAnimationFrame(() => (entered = true));
				queueMicrotask(() => input?.focus());
			}
		} else if (open && e.key === 'Escape') {
			close();
		}
	}

	function onListKey(e: KeyboardEvent) {
		const items = filtered;
		if (e.key === 'ArrowDown') {
			e.preventDefault();
			index = Math.min(index + 1, items.length - 1);
			scrollActiveIntoView();
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			index = Math.max(index - 1, 0);
			scrollActiveIntoView();
		} else if (e.key === 'Enter') {
			e.preventDefault();
			items[index]?.run();
		}
	}

	$effect(() => {
		void query;
		index = 0;
		runFileSearch();
	});
</script>

<svelte:window onkeydown={onGlobalKey} />

{#if open}
	<div
		class="cmdk"
		class:active={entered}
		role="presentation"
		onclick={(e) => e.target === e.currentTarget && close()}
	>
		<div
			class="cmdk__panel"
			role="dialog"
			aria-modal="true"
			aria-label={t('cmdk.title', 'Command palette')}
			data-testid="command-palette-panel"
		>
			<div class="cmdk__search">
				<Icon name="search" />
				<!-- svelte-ignore a11y_autofocus -->
				<input
					bind:this={input}
					bind:value={query}
					onkeydown={onListKey}
					placeholder={t('cmdk.placeholder', 'Type a command or search…')}
					autocomplete="off"
					autofocus
					data-testid="command-palette-input"
				/>
			</div>
			{#if filtered.length === 0}
				<p class="cmdk__empty">{t('cmdk.no_results', 'No matching commands')}</p>
			{:else}
				<ul class="cmdk__list" role="listbox" bind:this={listEl}>
					{#each filtered as cmd, i (cmd.id)}
						<li>
							<button
								class="cmdk__item"
								class:active={i === index}
								role="option"
								aria-selected={i === index}
								data-testid={`command-palette-${cmd.id}-item`}
								onmouseenter={() => (index = i)}
								onclick={cmd.run}
							>
								<Icon name={cmd.icon} />
								<span class="cmdk__label">{cmd.label}</span>
								{#if cmd.hint}<span class="cmdk__hint">{cmd.hint}</span>{/if}
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	</div>
{/if}

<style>
	.cmdk {
		position: fixed;
		inset: 0;
		z-index: 1200;
		background: var(--color-overlay, var(--color-overlay-light));
		backdrop-filter: blur(2px);
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding-top: 12vh;
		opacity: 0;
		transition: opacity var(--motion-base) var(--ease-standard);
	}

	.cmdk.active {
		opacity: 1;
	}

	.cmdk__panel {
		width: min(560px, 92vw);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-3xl);
		box-shadow: var(--shadow-2xl);
		overflow: hidden;
		transform: translateY(-8px) scale(0.98);
		transition: transform var(--motion-base) var(--ease-standard);
	}

	.cmdk.active .cmdk__panel {
		transform: none;
	}

	.cmdk__search {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		padding: 0.75rem 1rem;
		border-bottom: 1px solid var(--color-border);
		color: var(--color-text-muted);
	}

	.cmdk__search input {
		flex: 1;
		border: none;
		background: none;
		color: var(--color-text);
		font-size: 1rem;
		outline: none;
	}

	.cmdk__list {
		list-style: none;
		margin: 0;
		padding: 0.25rem;
		max-height: 50vh;
		overflow: auto;
	}

	.cmdk__item {
		display: flex;
		align-items: center;
		gap: 0.7rem;
		width: 100%;
		padding: 0.55rem 0.7rem;
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		text-align: left;
		border-radius: var(--radius-sm);
	}

	.cmdk__item.active {
		background: var(--color-accent-bg-sm);
	}

	.cmdk__item.active :global(.oxi-icon) {
		color: var(--color-accent);
	}

	.cmdk__label {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.cmdk__hint {
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
	}

	.cmdk__empty {
		padding: 1.5rem;
		text-align: center;
		color: var(--color-text-muted);
	}
</style>
