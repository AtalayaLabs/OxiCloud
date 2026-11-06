<script lang="ts">
	import Icon from '$lib/icons/Icon.svelte';
	import { dateTimeFormatFor } from '$lib/utils/display';
	import { t } from '$lib/i18n/index.svelte';
	import type { Notification } from '$lib/stores/ui.svelte';

	let { n }: { n: Notification } = $props();

	function formatTime(ms: number): string {
		return dateTimeFormatFor(undefined, { hour: '2-digit', minute: '2-digit' }).format(ms);
	}

	function notifIcon(kind: string): string {
		switch (kind) {
			case 'success':
				return 'check';
			case 'error':
				return 'exclamation-circle';
			case 'warning':
				return 'exclamation-triangle';
			default:
				return 'info-circle';
		}
	}
</script>

<div class="notif-item notif-item--{n.kind}">
	<span class="notif-item-icon"><Icon name={n.icon ?? notifIcon(n.kind)} /></span>
	<div class="notif-item-body">
		<div class="notif-item-text">{n.message}</div>
		{#if n.currentFile}
			<div class="notif-item-current" title={n.currentFile}>{n.currentFile}</div>
		{/if}
		{#if n.progress !== undefined}
			<div
				class="notif-progress"
				role="progressbar"
				aria-valuenow={n.progress}
				aria-valuemin="0"
				aria-valuemax="100"
			>
				<div class="notif-progress__fill" style:width="{n.progress}%"></div>
			</div>
			<div class="notif-progress-detail">
				<span>{n.progress}%</span>
				{#if n.total !== undefined}
					<span>
						{t(
							'upload.files_counter',
							{ done: n.completed ?? 0, total: n.total },
							`${n.completed ?? 0} / ${n.total} files`
						)}
					</span>
				{/if}
				{#if n.onCancel}
					<button
						class="notif-cancel"
						type="button"
						onclick={(e) => {
							e.stopPropagation();
							n.onCancel?.();
						}}
					>
						{t('common.cancel', 'Cancel')}
					</button>
				{/if}
			</div>
		{:else}
			<div class="notif-item-time">{formatTime(n.at)}</div>
		{/if}
	</div>
</div>

<style>
	.notif-progress {
		height: 6px;
		margin-top: 0.35rem;
		border-radius: var(--radius-pill, 999px);
		background: var(--color-bg-muted);
		overflow: hidden;
	}

	.notif-progress__fill {
		height: 100%;
		background: var(--color-accent);
		transition: width 0.2s ease;
	}

	.notif-progress-detail {
		display: flex;
		justify-content: space-between;
		gap: 0.5rem;
		margin-top: 0.25rem;
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
	}

	.notif-cancel {
		padding: 0;
		border: none;
		background: none;
		color: var(--color-danger-text);
		font: inherit;
		cursor: pointer;
	}

	.notif-cancel:hover {
		text-decoration: underline;
	}

	.notif-item-current {
		margin-top: 0.15rem;
		font-size: var(--text-xs, 0.75rem);
		color: var(--color-text-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
