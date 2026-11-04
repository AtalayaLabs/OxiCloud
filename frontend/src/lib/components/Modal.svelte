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

	/**
	 * Is this element a text-entry control that should take initial focus on
	 * open? `<input>` with a text-shaped `type` (so the password-reveal
	 * eyedropper button nested in some auth forms doesn't match on `type=
	 * button`), `<textarea>`, and anything explicitly marked
	 * `contenteditable`. Excludes `<select>` on purpose — it's a dropdown,
	 * not a typing surface, and auto-opening its menu on dialog open
	 * surprises users.
	 */
	function isTextEntry(el: HTMLElement): boolean {
		if (el instanceof HTMLTextAreaElement) return true;
		if (el.isContentEditable) return true;
		if (el instanceof HTMLInputElement) {
			// Empty `type` defaults to text. Everything else must be an
			// opt-in text-entry type — buttons, checkboxes, radios, files,
			// colour pickers, and the like are explicitly NOT what the
			// user wants to type into on open.
			const t = el.type.toLowerCase();
			return (
				t === '' ||
				t === 'text' ||
				t === 'search' ||
				t === 'email' ||
				t === 'url' ||
				t === 'password' ||
				t === 'tel' ||
				t === 'number' ||
				t === 'date' ||
				t === 'datetime-local' ||
				t === 'month' ||
				t === 'time' ||
				t === 'week'
			);
		}
		return false;
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
	//
	// Prefer the first text-entry control (input/textarea/[contenteditable])
	// over any other focusable, because the header's close button (`×`) is
	// almost always the first focusable in reading order — and in a rename
	// or new-folder dialog the user wants to type immediately, not reach
	// for the mouse. Confirmation modals with no text input fall through
	// to the existing first-focusable behaviour (usually the Cancel button,
	// which is also safer than auto-focusing a destructive Delete button).
	$effect(() => {
		if (open) {
			prevFocus = (document.activeElement as HTMLElement | null) ?? null;
			requestAnimationFrame(() => {
				const items = focusables();
				const textEntry = items.find(isTextEntry);
				(textEntry ?? items[0] ?? dialogEl)?.focus();
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
						type="button"
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
		/*
		 * 1050 — sits above full-screen overlays (PhotoLightbox,
		 * FileViewer, notifications-root at z 1000) so a delete /
		 * confirm / chooser triggered while a lightbox is open is
		 * reachable. Stays below CommandPalette (1200) and the
		 * global Toaster (1200) which must remain top-of-stack.
		 */
		z-index: 1050;
		padding: var(--space-4);
		animation: modal-fade var(--motion-base) ease;
	}

	.modal {
		position: relative;
		display: flex;
		flex-direction: column;
		width: min(92vw, 32rem);
		max-height: 90vh;
		max-height: calc(100dvh - var(--space-16));
		background: var(--color-bg-surface);
		color: var(--color-text);
		border-radius: var(--radius-3xl);
		box-shadow: var(--shadow-xl);
		animation: modal-pop var(--motion-moderate) var(--ease-standard);
	}

	/* Keeps the same `92vw` viewport clamp, so this widens a desktop dialog
	   and changes nothing on a phone — where 32rem was already the wider of
	   the two and the clamp is what actually applies. */
	.modal--lg {
		width: min(92vw, 48rem);
	}

	.modal__header {
		display: flex;
		flex: 0 0 auto;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		padding: var(--space-6) var(--space-6) var(--space-2);
	}

	.modal__title {
		margin: 0;
		color: var(--color-text-heading);
		font-size: var(--text-xl);
		font-weight: var(--weight-semibold);
		line-height: var(--leading-tight);
	}

	.modal__close {
		display: grid;
		place-items: center;
		width: 2.5rem;
		height: 2.5rem;
		padding: 0;
		border: 0;
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		color: var(--color-text-secondary);
		font-size: var(--text-xl);
		line-height: var(--leading-none);
		cursor: pointer;
		transition:
			background-color var(--motion-fast) ease,
			color var(--motion-fast) ease;
	}

	.modal__close:hover {
		background: var(--color-bg-hover);
		color: var(--color-text);
	}

	.modal__body {
		flex: 1 1 auto;
		min-height: 0;
		overflow-y: auto;
		overscroll-behavior: contain;
		padding: var(--space-4) var(--space-6) var(--space-6);
	}

	.modal__footer {
		flex: 0 0 auto;
		display: flex;
		justify-content: center;
		flex-wrap: wrap;
		gap: var(--space-3);
		padding: var(--space-4) var(--space-6) var(--space-5);
		border-top: 1px solid var(--color-border);
		border-radius: 0 0 var(--radius-3xl) var(--radius-3xl);
		background: var(--color-bg-subtle);
	}

	.modal__body :global(form) {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}

	.modal__body :global(form label) {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		color: var(--color-text-secondary);
		font-size: var(--text-base);
		font-weight: var(--weight-medium);
	}

	.modal__body :global(form label:has(input[type='checkbox'], input[type='radio'])) {
		flex-direction: row;
		align-items: center;
	}

	.modal__body
		:global(
			form label:has(input:required, select:required, textarea:required) > span:first-child
		)::after {
		content: ' *';
		color: var(--color-accent-text);
	}

	.modal__body
		:global(
			form
				input:not([type='checkbox'], [type='radio'], [type='hidden'], [type='file'], [type='date'])
		),
	.modal__body :global(form select),
	.modal__body :global(form textarea) {
		width: 100%;
		padding: var(--space-2-5) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-xl);
		background: var(--color-bg-input);
		color: var(--color-text);
		font: inherit;
		font-weight: var(--weight-normal);
		transition:
			border-color var(--motion-fast) ease,
			box-shadow var(--motion-fast) ease;
	}

	.modal__body
		:global(
			form
				input:not(
					[type='checkbox'],
					[type='radio'],
					[type='hidden'],
					[type='file'],
					[type='date']
				):focus
		),
	.modal__body :global(form select:focus),
	.modal__body :global(form textarea:focus) {
		outline: none;
		border-color: var(--color-accent);
		box-shadow: 0 0 0 3px var(--color-accent-ring);
	}

	.modal__body :global(form textarea) {
		min-height: 5rem;
		resize: vertical;
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

	@keyframes modal-sheet-up {
		from {
			transform: translateY(100%);
		}

		to {
			transform: translateY(0);
		}
	}

	@media (width <= 768px) {
		.modal__backdrop {
			align-items: flex-end;
			padding: 0 0 calc(var(--mobile-nav-height) + env(safe-area-inset-bottom, 0px));
		}

		.modal {
			width: 100%;
			max-height: calc(100dvh - var(--mobile-nav-height) - var(--space-6));
			border-radius: var(--radius-3xl) var(--radius-3xl) 0 0;
			overflow: hidden;
			animation: modal-sheet-up var(--motion-slow) var(--ease-emphasized);
		}

		.modal--lg {
			width: 100%;
		}

		.modal__header {
			padding: var(--space-5) var(--space-14) var(--space-2) var(--space-5);
		}

		.modal__close {
			position: absolute;
			top: var(--space-3);
			right: var(--space-3);
			z-index: 1;
			width: 2.25rem;
			height: 2.25rem;
			font-size: var(--text-lg);
		}

		.modal__body {
			padding: var(--space-3) var(--space-5) var(--space-5);
		}

		.modal__footer {
			padding: var(--space-3) var(--space-5);
			border-radius: 0;
		}

		.modal__footer :global(button) {
			flex: 1 1 0;
			min-height: 2.75rem;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.modal__backdrop,
		.modal {
			animation: none;
		}
	}
</style>
