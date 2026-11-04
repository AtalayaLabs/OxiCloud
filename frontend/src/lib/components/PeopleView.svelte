<script lang="ts">
	/**
	 * People (faces): a grid of identity clusters from `GET /api/people`; clicking
	 * a person shows their photos in the shared lightbox. Faces are detected and
	 * clustered server-side, so this view is read-mostly (list, drill-in, rename).
	 * Gated on `OXICLOUD_ENABLE_FACES` — when off the API 404s and we show a hint.
	 */
	import EmptyState from '$lib/components/EmptyState.svelte';
	import PhotoLightbox from '$lib/components/PhotoLightbox.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import {
		fetchPeople,
		fetchPersonPhotos,
		renamePerson,
		type Person
	} from '$lib/api/endpoints/people';
	import { fileThumbnailUrl } from '$lib/api/endpoints/files';
	import type { FileItem } from '$lib/api/types';
	import { promptDialog } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { minimalPhotoItem } from '$lib/utils/media';
	import { onMount } from 'svelte';

	type View = 'list' | 'person';

	let view = $state<View>('list');
	let people = $state<Person[]>([]);
	let loading = $state(true);
	/** Set when the feature is unavailable (faces disabled) or the list errors. */
	let disabled = $state(false);

	// Drill-in state.
	let current = $state<{ id: string; name: string } | null>(null);
	let photos = $state<FileItem[]>([]);
	let lightbox = $state(-1);

	function personName(p: Person): string {
		return p.name || t('people.unnamed', 'Unnamed');
	}

	async function loadList() {
		loading = true;
		disabled = false;
		try {
			people = await fetchPeople();
		} catch {
			people = [];
			disabled = true;
		} finally {
			loading = false;
		}
	}

	async function openPerson(p: Person) {
		current = { id: p.id, name: personName(p) };
		view = 'person';
		photos = [];
		lightbox = -1;
		try {
			const ids = await fetchPersonPhotos(p.id);
			photos = ids.map(minimalPhotoItem);
		} catch {
			photos = [];
		}
	}

	function backToList() {
		view = 'list';
		current = null;
		lightbox = -1;
	}

	async function rename() {
		if (!current) return;
		const placeholder = t('people.unnamed', 'Unnamed');
		const value = current.name === placeholder ? '' : current.name;
		await promptDialog({
			title: t('people.rename_title', 'Name this person'),
			message: t('people.name_label', 'Name'),
			defaultValue: value,
			// Select all — a person's name is treated as one unit (no
			// extension semantic). `selectOnOpen: 'name'` would misfire
			// on names containing dots like "Dr. Smith" (would pre-
			// select only "Dr"); plain `true` selects the full string.
			selectOnOpen: true,
			// Inline-error UX — see `frontend/AGENTS.md § Dialog error UX`.
			action: async (next) => {
				if (next === null) return;
				const trimmed = next.trim();
				await renamePerson(current!.id, trimmed || null);
				current = { id: current!.id, name: trimmed || placeholder };
				// Keep the list in sync so a return trip shows the new name.
				people = people.map((p) =>
					p.id === current?.id ? { ...p, name: trimmed || undefined } : p
				);
			}
		});
	}

	function onDeletePhoto(id: string) {
		photos = photos.filter((p) => p.id !== id);
	}

	onMount(loadList);
</script>

{#if loading}
	<p class="people-status">{t('common.loading', 'Loading…')}</p>
{:else if disabled}
	<EmptyState icon="user-group" title={t('people.disabled', 'Face recognition is disabled')} />
{:else if view === 'list'}
	{#if people.length === 0}
		<EmptyState icon="user-group" title={t('people.empty', 'No people yet')} />
	{:else}
		<ul class="people-grid">
			{#each people as person (person.id)}
				<li>
					<button class="person-tile" type="button" onclick={() => openPerson(person)}>
						<span class="person-avatar">
							{#if person.cover_file_id}
								<img src={fileThumbnailUrl(person.cover_file_id, 'icon')} alt="" loading="lazy" />
							{:else}
								<Icon name="user-group" />
							{/if}
						</span>
						<span class="person-name">{personName(person)}</span>
						<span class="person-count">{person.face_count}</span>
					</button>
				</li>
			{/each}
		</ul>
	{/if}
{:else if current}
	<div class="people-toolbar">
		<button
			class="people-back"
			type="button"
			aria-label={t('people.back', 'Back')}
			onclick={backToList}
		>
			<Icon name="arrow-left" />
		</button>
		<h2 class="people-title">{current.name}</h2>
		<button
			class="people-rename"
			type="button"
			aria-label={t('people.rename_title', 'Name this person')}
			onclick={rename}
		>
			<Icon name="pen" />
		</button>
	</div>

	<ul class="photos">
		{#each photos as photo, i (photo.id)}
			<li class="photos__cell">
				<button class="photos__open" onclick={() => (lightbox = i)}>
					<img src={fileThumbnailUrl(photo.id, 'preview')} alt="" loading="lazy" decoding="async" />
				</button>
			</li>
		{/each}
	</ul>

	<PhotoLightbox items={photos} bind:index={lightbox} onDelete={onDeletePhoto} />
{/if}

<style>
	.people-status {
		text-align: center;
		color: var(--color-text-muted);
		padding: 2rem 0;
	}

	.people-grid {
		list-style: none;
		margin: 0;
		padding: 1rem;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(7rem, 1fr));
		gap: var(--space-4);
	}

	.person-tile {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2);
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		border-radius: var(--radius-md);
	}

	.person-tile:hover {
		background: var(--color-bg-hover);
	}

	.person-avatar {
		display: grid;
		place-items: center;
		width: 5.5rem;
		height: 5.5rem;
		border-radius: 50%;
		overflow: hidden;
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: 1.5rem;
	}

	.person-avatar img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}

	.person-name {
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: var(--text-sm);
	}

	.person-count {
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
	}

	.people-toolbar {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: 1rem;
	}

	.people-back,
	.people-rename {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border: none;
		border-radius: 50%;
		background: var(--color-bg-surface);
		color: var(--color-text);
		cursor: pointer;
	}

	.people-back:hover,
	.people-rename:hover {
		background: var(--color-bg-hover);
	}

	.people-title {
		flex: 1;
		margin: 0;
		font-size: 1.25rem;
		color: var(--color-text-heading);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.photos {
		list-style: none;
		margin: 0;
		padding: 0 1rem 1rem;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(9rem, 1fr));
		gap: 0.25rem;
	}

	.photos__cell {
		position: relative;
		aspect-ratio: 1;
		overflow: hidden;
		border-radius: var(--radius-sm);
		background: var(--color-bg-muted);
	}

	.photos__open {
		display: block;
		width: 100%;
		height: 100%;
		border: none;
		padding: 0;
		cursor: pointer;
		background: none;
	}

	.photos__open img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
	}
</style>
