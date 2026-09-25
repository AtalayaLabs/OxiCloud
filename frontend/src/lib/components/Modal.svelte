<script lang="ts">
	import type { Snippet } from 'svelte';
	import { t } from '$lib/i18n/index.svelte';

	interface Props {
		open: boolean;
		title?: string;
		/** Called when the user requests close (backdrop click, Escape, ✕). */
		onclose?: () => void;
		children?: Snippet;
		footer?: Snippet;
		/**
		 * Dialog width.
		 *
		 * `md` (the default, unchanged) suits a handful of fields, which is
		 * what nearly every modal here is. `lg` is for content that is a list
		 * rather than a form — the policy editor puts a title, an inherited
		 * badge and help text on one row, and at `md` all three wrap.
		 */
		size?: 'md' | 'lg';
	}

	let { open = $bindable(false), title, onclose, children, footer, size = 'md' }: Props = $props();

	let dialogEl = $state<HTMLElement | null>(null);
	let prevFocus: HTMLElement | null = null;

	function close() {
		open = false;
		onclose?.();
	}

	const FOCUSABLE =
		'a[href], button:not([disabled]), textarea, input, select, [tabindex]:not([tabindex="-1"])';

	function focusables(): HTMLElement[] {
		if (!dialogEl) return [];
		return Array.from(dialogEl.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
			(el) => el.offsetParent !== null || el === document.activeElement
		);
	}

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			close();
			return;
		}
		// Focus trap: keep Tab cycling inside the dialog.
		if (e.key === 'Tab') {
			const items = focusables();
			if (items.length === 0) return;
			const first = items[0];
			const last = items[items.length - 1];
			const active = document.activeElement as HTMLElement | null;
			if (e.shiftKey && active === first) {
				e.preventDefault();
				last.focus();
			} else if (!e.shiftKey && active === last) {
				e.preventDefault();
				first.focus();
			}
		}
	}

	// On open: remember the previously focused element and move focus into the
	// dialog. On close: restore focus so keyboard users aren't dumped at <body>.
	$effect(() => {
		if (open) {
			prevFocus = (document.activeElement as HTMLElement | null) ?? null;
			requestAnimationFrame(() => {
				const items = focusables();
				(items[0] ?? dialogEl)?.focus();
			});
		} else if (prevFocus) {
			prevFocus.focus();
			prevFocus = null;
		}
	});
</script>

<svelte:window onkeydown={open ? onkeydown : undefined} />

{#if open}
	<!-- backdrop -->
	<div
		class="modal__backdrop"
		role="presentation"
		onclick={(e) => {
			if (e.target === e.currentTarget) close();
		}}
	>
		<div
			class="modal"
			class:modal--lg={size === 'lg'}
			role="dialog"
			aria-modal="true"
			aria-label={title}
			tabindex="-1"
			data-testid="modal"
			bind:this={dialogEl}
		>
			{#if title}
				<header class="modal__header">
					<h2 class="modal__title">{title}</h2>
					<button
						class="modal__close"
						aria-label={t('common.close', 'Close')}
						data-testid="modal-close-btn"
						onclick={close}>×</button
					>
				</header>
			{/if}
			<div class="modal__body">
				{@render children?.()}
			</div>
			{#if footer}
				<footer class="modal__footer">
					{@render footer()}
				</footer>
			{/if}
		</div>
	</div>
{/if}

<style>
	.modal__backdrop {
		position: fixed;
		inset: 0;
		background: var(--color-overlay);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 900;
		padding: 1rem;
		animation: modal-fade 0.16s ease;
	}

	.modal {
		background: var(--color-bg-surface);
		color: var(--color-text);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-xl);
		width: min(92vw, 32rem);
		max-height: 90vh;
		overflow: auto;
		display: flex;
		flex-direction: column;
		animation: modal-pop 0.18s ease;
	}

	/* Keeps the same `92vw` viewport clamp, so this widens a desktop dialog
	   and changes nothing on a phone — where 32rem was already the wider of
	   the two and the clamp is what actually applies. */
	.modal--lg {
		width: min(92vw, 48rem);
	}

	.modal__header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 1rem 1.25rem;
		border-bottom: 1px solid var(--color-border);
	}

	.modal__title {
		margin: 0;
		font-size: 1.125rem;
	}

	.modal__close {
		background: none;
		border: none;
		font-size: 1.5rem;
		line-height: 1;
		cursor: pointer;
		color: var(--color-text-muted);
	}

	.modal__body {
		padding: 1.25rem;
	}

	.modal__footer {
		display: flex;
		justify-content: flex-end;
		gap: 0.5rem;
		padding: 1rem 1.25rem;
		border-top: 1px solid var(--color-border);
	}

	@keyframes modal-fade {
		from {
			opacity: 0;
		}

		to {
			opacity: 1;
		}
	}

	@keyframes modal-pop {
		from {
			opacity: 0;
			transform: translateY(8px) scale(0.98);
		}

		to {
			opacity: 1;
			transform: translateY(0) scale(1);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.modal__backdrop,
		.modal {
			animation: none;
		}
	}
</style>
