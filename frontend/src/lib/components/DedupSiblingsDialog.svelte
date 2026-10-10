<script lang="ts">
	/**
	 * §9b Layer 2 delete-UX chooser. Opened when the user asks to trash a photo
	 * whose listing row carries `has_blob_siblings: true` — i.e. the §9
	 * within-drive dedup hid at least one other visible file referencing the
	 * same bytes. Lists every LIVE sibling the caller can Read, lets them tick
	 * which copies to trash, and returns the picked ids for a single
	 * `batchTrash` call.
	 *
	 * Already-trashed siblings are filtered out at the FE boundary — the batch
	 * endpoint rejects them (SQL guard `WHERE NOT is_trashed`), so including
	 * them would produce a confusing "N of M moved to trash" warning without
	 * freeing any bytes. A `trashed_note` surfaces the trashed-sibling count
	 * so the user knows bytes still held by trash entries need the Trash
	 * view's permanent-delete to be reclaimed.
	 *
	 * No new bulk-trash endpoint is involved — the dialog is a UX helper on
	 * top of `POST /api/batch/trash`, so there is zero blast-radius risk from
	 * server-side bulk operations on a hash.
	 */
	import { SvelteSet } from 'svelte/reactivity';
	import Modal from '$lib/components/Modal.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import type { HashSibling } from '$lib/api/endpoints/dedup';

	interface Props {
		open: boolean;
		/** All siblings returned by the server, including the originally-clicked tile. */
		siblings: HashSibling[];
		/** True when the server capped the response (`MAX_SIBLINGS = 500`). */
		truncated: boolean;
		/** The tile the user clicked delete on — pre-checked by default. */
		originId: string;
		onclose?: () => void;
		/** Called with the final list of file ids to trash. */
		onconfirm?: (ids: string[]) => void;
	}

	let {
		open = $bindable(false),
		siblings,
		truncated,
		originId,
		onclose,
		onconfirm
	}: Props = $props();

	// Checkbox state keyed by file_id. Pre-checked set defaults to the
	// origin tile only — the user explicitly asked for that one; every
	// other sibling is opt-in, so the no-interaction path trashes exactly
	// what the user asked for and nothing more. `SvelteSet` so individual
	// `.add` / `.delete` calls trigger reactivity (plain `Set` doesn't).
	const picked = new SvelteSet<string>();

	// Reset the picked set every time the dialog opens, so stale state
	// from a prior invocation can't bleed through. `$effect` watches the
	// `open` edge; the `originId` dependency catches the (rare) case where
	// a reopen targets a different tile without the dialog unmounting.
	$effect(() => {
		if (open) {
			picked.clear();
			picked.add(originId);
		}
	});

	// Filter trashed siblings out at the FE boundary: the chooser is a
	// "which copies to trash NOW" picker, and the batch-trash endpoint
	// rejects already-trashed rows (SQL guard `WHERE NOT is_trashed`)
	// as `not_found`. Keeping them in the list produces a confusing
	// "N of M moved to trash" warning without freeing any bytes. The
	// server still returns them in the raw response so audits /
	// future admin views have the full picture. Freeing bytes held by
	// trashed copies is the Trash view's job (`DELETE /api/trash/{id}`).
	const live = $derived(siblings.filter((s) => !s.is_trashed));
	const checkable = $derived(live.filter((s) => s.can_delete));
	const nonCheckable = $derived(live.filter((s) => !s.can_delete));
	const trashedCount = $derived(siblings.length - live.length);

	function toggle(id: string) {
		if (picked.has(id)) picked.delete(id);
		else picked.add(id);
	}

	function selectAllCheckable() {
		picked.clear();
		for (const s of checkable) picked.add(s.file_id);
	}

	function selectNone() {
		picked.clear();
	}

	function confirm() {
		const ids = Array.from(picked);
		open = false;
		onconfirm?.(ids);
	}

	function cancel() {
		open = false;
		onclose?.();
	}
</script>

<Modal
	bind:open
	title={t('photos.dedup_dialog.title', 'This content has copies')}
	onclose={cancel}
	size="lg"
>
	<div class="dedup-dialog">
		<p class="dedup-dialog__intro">
			{t(
				'photos.dedup_dialog.intro',
				{ n: live.length },
				'{{n}} file(s) in your drive reference the same bytes. Trashing one copy does not free the content — pick every copy you want to send to trash.'
			)}
		</p>

		{#if trashedCount > 0}
			<p class="dedup-dialog__note">
				{t(
					'photos.dedup_dialog.trashed_note',
					{ n: trashedCount },
					'{{n}} other copy / copies already sit in your trash and still hold the content. Purge them from the Trash view to actually free the bytes.'
				)}
			</p>
		{/if}

		{#if truncated}
			<p class="dedup-dialog__warning">
				<Icon name="triangle-exclamation" />
				{t(
					'photos.dedup_dialog.truncated',
					'The server capped this list. More copies exist but are not shown here.'
				)}
			</p>
		{/if}

		<div class="dedup-dialog__toolbar">
			<button type="button" class="dedup-dialog__link" onclick={selectAllCheckable}>
				{t('photos.dedup_dialog.select_all', 'Select all deletable')}
			</button>
			<span class="dedup-dialog__separator">·</span>
			<button type="button" class="dedup-dialog__link" onclick={selectNone}>
				{t('photos.dedup_dialog.select_none', 'Select none')}
			</button>
			<span class="dedup-dialog__count">
				{t(
					'photos.dedup_dialog.picked',
					{ n: picked.size, total: checkable.length },
					'{{n}} of {{total}} selected'
				)}
			</span>
		</div>

		<ul class="dedup-dialog__list" role="list">
			{#each live as sib (sib.file_id)}
				<li class="dedup-dialog__row" class:dedup-dialog__row--readonly={!sib.can_delete}>
					<label class="dedup-dialog__row-label">
						<input
							type="checkbox"
							checked={picked.has(sib.file_id)}
							disabled={!sib.can_delete}
							onchange={() => toggle(sib.file_id)}
						/>
						<div class="dedup-dialog__row-body">
							<div class="dedup-dialog__row-head">
								<span class="dedup-dialog__row-name">{sib.name}</span>
								{#if sib.file_id === originId}
									<span class="dedup-dialog__badge dedup-dialog__badge--origin">
										{t('photos.dedup_dialog.origin_badge', 'the one you clicked')}
									</span>
								{/if}
								{#if !sib.can_delete}
									<span class="dedup-dialog__badge dedup-dialog__badge--readonly">
										{t('photos.dedup_dialog.readonly_badge', 'read-only')}
									</span>
								{/if}
							</div>
							<div class="dedup-dialog__row-location">
								<Icon name="folder" />
								<span>
									{sib.folder_name ?? t('photos.dedup_dialog.drive_root', 'Drive root')}
								</span>
							</div>
						</div>
					</label>
				</li>
			{/each}
		</ul>

		{#if nonCheckable.length > 0}
			<p class="dedup-dialog__note">
				{t(
					'photos.dedup_dialog.readonly_note',
					{ n: nonCheckable.length },
					'{{n}} copy / copies live where you only have read access — they cannot be trashed from here.'
				)}
			</p>
		{/if}
	</div>

	{#snippet footer()}
		<button type="button" class="btn btn--ghost" onclick={cancel}>
			{t('common.cancel', 'Cancel')}
		</button>
		<button type="button" class="btn btn--danger" disabled={picked.size === 0} onclick={confirm}>
			{t('photos.dedup_dialog.confirm', { n: picked.size }, 'Trash {{n}} file(s)')}
		</button>
	{/snippet}
</Modal>

<style>
	.dedup-dialog {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.dedup-dialog__intro {
		margin: 0;
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		line-height: 1.5;
	}

	.dedup-dialog__warning {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin: 0;
		padding: var(--space-2) var(--space-3);
		background: var(--color-warning-bg);
		border: 1px solid var(--color-warning-border);
		color: var(--color-warning-text);
		border-radius: var(--radius-sm);
		font-size: var(--text-sm);
	}

	.dedup-dialog__toolbar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-size: var(--text-sm);
	}

	.dedup-dialog__link {
		background: none;
		border: none;
		padding: 0;
		color: var(--color-accent);
		cursor: pointer;
		font-size: inherit;
	}

	.dedup-dialog__link:hover {
		text-decoration: underline;
	}

	.dedup-dialog__separator {
		color: var(--color-text-muted);
	}

	.dedup-dialog__count {
		margin-left: auto;
		color: var(--color-text-muted);
	}

	.dedup-dialog__list {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 50vh;
		overflow-y: auto;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm);
	}

	.dedup-dialog__row {
		border-bottom: 1px solid var(--color-border-subtle);
	}

	.dedup-dialog__row:last-child {
		border-bottom: none;
	}

	.dedup-dialog__row--readonly {
		opacity: 0.7;
	}

	.dedup-dialog__row-label {
		display: flex;
		align-items: flex-start;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		cursor: pointer;
		font-size: var(--text-sm);
	}

	.dedup-dialog__row-body {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.dedup-dialog__row-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}

	.dedup-dialog__row-location {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.dedup-dialog__row--readonly .dedup-dialog__row-label {
		cursor: not-allowed;
	}

	.dedup-dialog__row-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.dedup-dialog__badge {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: 2px var(--space-2);
		border-radius: var(--radius-full);
		font-size: var(--text-xs);
		font-weight: 500;
	}

	.dedup-dialog__badge--origin {
		background: var(--color-accent-tint);
		color: var(--color-accent);
	}

	.dedup-dialog__badge--readonly {
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
	}

	.dedup-dialog__note {
		margin: 0;
		color: var(--color-text-muted);
		font-size: var(--text-xs);
	}
</style>
