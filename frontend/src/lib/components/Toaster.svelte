<script lang="ts">
	import { ui } from '$lib/stores/ui.svelte';
	import { t } from '$lib/i18n/index.svelte';
</script>

<div
	class="toaster"
	role="region"
	aria-live="polite"
	aria-label={t('notifications.title', 'Notifications')}
>
	{#each ui.toasts as toast (toast.id)}
		<div class="toast toast--{toast.kind}" role="status" data-testid={`toaster-toast-${toast.id}`}>
			<span class="toast__msg">{toast.message}</span>
			{#if toast.action}
				<button
					class="toast__action"
					data-testid={`toaster-action-btn-${toast.id}`}
					onclick={() => {
						toast.action?.onClick();
						ui.dismiss(toast.id);
					}}
				>
					{toast.action.label}
				</button>
			{/if}
			<button
				class="toast__close"
				data-testid={`toaster-dismiss-btn-${toast.id}`}
				aria-label={t('common.dismiss', 'Dismiss')}
				onclick={() => ui.dismiss(toast.id)}
			>
				×
			</button>
		</div>
	{/each}
</div>

<style>
	.toaster {
		position: fixed;
		/* Offset clears any bottom-right FAB the file view may mount; the
		   --toaster-offset hook lets a page lift the stack further if needed. */
		--toaster-base: 1rem;
		bottom: calc(
			var(--toaster-base) + env(safe-area-inset-bottom, 0px) + var(--toaster-offset, 0px)
		);
		right: 1rem;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		z-index: 1200;
		max-width: min(92vw, 24rem);
		/* Let clicks pass through the gaps; individual toasts re-enable below. */
		pointer-events: none;
	}

	.toast {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.75rem 1rem;
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		box-shadow: var(--shadow-md);
		border-left: 4px solid var(--color-border);
		pointer-events: auto;
	}

	.toast--success {
		border-left-color: var(--color-success-text);
	}

	.toast--error {
		border-left-color: var(--color-danger-text);
	}

	.toast--warning {
		border-left-color: var(--color-warning-text);
	}

	.toast--info {
		border-left-color: var(--color-primary);
	}

	.toast__msg {
		flex: 1;
	}

	.toast__action {
		flex-shrink: 0;
		background: var(--color-accent);
		color: var(--color-accent-contrast);
		border: none;
		border-radius: var(--radius-md);
		padding: var(--space-1-5) var(--space-3);
		font-size: var(--text-sm);
		font-weight: var(--weight-medium);
		cursor: pointer;
	}

	.toast__action:hover {
		filter: brightness(0.95);
	}

	.toast__close {
		background: none;
		border: none;
		cursor: pointer;
		font-size: 1.25rem;
		line-height: 1;
		color: inherit;
		opacity: 0.7;
	}

	.toast__close:hover {
		opacity: 1;
	}

	@media (width <= 768px) {
		.toaster {
			--toaster-base: calc(var(--mobile-nav-height) + var(--space-3));
		}
	}
</style>
