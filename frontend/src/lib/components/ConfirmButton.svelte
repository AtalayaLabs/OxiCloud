<!--
	A button that arms on the first click and runs on the second, swapping its
	label for "Confirm" in between. The icon is passed as a prop and stays put
	across both states; the confirm label inherits the button's font, so the
	button keeps the height of the container it sits in and only widens where
	the label cannot fit. Disarms after three seconds, on Escape, or on blur.
-->
<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { ARMED_CONFIRM_MS } from '$lib/composables/useArmedConfirm.svelte';

	interface Props {
		/** Runs on the confirming click. */
		onconfirm: (e: MouseEvent) => void;
		/** Icon name, rendered before the label in both states. */
		icon?: string;
		/**
		 * `plain` renders whatever the caller's classes give it.
		 * `icon` is the app's delete-icon button: a bare red glyph with no box.
		 * It exists because a class handed to this component cannot be matched
		 * by the caller's scoped stylesheet — only this file's styles reach the
		 * button it renders.
		 */
		variant?: 'plain' | 'icon';
		/** Extra classes from the caller. Only useful for `plain`. */
		class?: string;
		disabled?: boolean;
		/** Accessible name while unarmed; the label replaces it when armed. */
		title?: string;
		/** `menuitem` when the button lives inside a context menu. */
		role?: 'menuitem';
		/** e2e hook forwarded to the underlying `<button>`. */
		'data-testid'?: string;
		children?: Snippet;
		/**
		 * Wording for the ARMED state — the visible label, and the
		 * accessible name while armed. Defaults to "Really?".
		 *
		 * Override it when the second click does something the default
		 * does not describe. The case this exists for: a delete is
		 * reversible on a deployment with trash enabled and irreversible
		 * without it, and "Really?" reads identically for both — the one
		 * distinction a confirm exists to draw. See `PhotoLightbox`.
		 */
		confirmLabel?: string;
	}

	let {
		onconfirm,
		icon,
		variant = 'plain',
		class: cls = '',
		disabled = false,
		title,
		role,
		'data-testid': testid,
		children,
		confirmLabel: confirmLabelProp
	}: Props = $props();

	let armed = $state(false);
	let timer: ReturnType<typeof setTimeout> | undefined;

	function disarm() {
		clearTimeout(timer);
		timer = undefined;
		armed = false;
	}

	function click(e: MouseEvent) {
		if (!armed) {
			clearTimeout(timer);
			armed = true;
			timer = setTimeout(disarm, ARMED_CONFIRM_MS);
			return;
		}
		disarm();
		onconfirm(e);
	}

	const confirmLabel = $derived(confirmLabelProp ?? t('common.really', 'Really?'));
</script>

<svelte:window
	onkeydown={(e) => {
		if (armed && e.key === 'Escape') disarm();
	}}
/>

<button
	type="button"
	class="confirm-btn confirm-btn--{variant} {children ? 'confirm-btn--text' : ''} {armed
		? 'confirm-btn--armed'
		: ''} {cls}"
	{disabled}
	title={armed ? confirmLabel : title}
	aria-label={armed ? confirmLabel : title}
	{role}
	data-testid={testid}
	onclick={click}
	onblur={disarm}
>
	{#if icon}<Icon name={icon} />{/if}
	{#if armed}
		<span class="confirm-btn__label">{confirmLabel}</span>
	{:else}
		{@render children?.()}
	{/if}
</button>

<style>
	/* The admin table's delete button: `.icon-btn` + `.icon-btn--danger` from
	   `admin/[[tab]]`, reproduced here because a class handed to this component
	   cannot be matched by the caller's scoped stylesheet — only this file's
	   styles reach the button it renders. Boxed, 2rem, red glyph. */
	.confirm-btn--icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: var(--space-1);
		flex-shrink: 0;
		width: 2rem;
		height: 2rem;
		padding: 0;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-danger-alt);
		cursor: pointer;
	}

	.confirm-btn--icon:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}

	/* Armed: solid red face, inverse label. Repeated so it outranks the caller's
	   own rules — `.btn-danger.s-hash` (0,2,0), the files list's
	   `.action-cell button:not(.btn-action)` (0,4,1) — whichever stylesheet the
	   browser applied last.

	   `min-width: fit-content` grows a fixed-width cell (28px icon actions, the
	   2rem icon variant) but leaves a caller that already owns its width alone:
	   a `width: 100%` menu row keeps its line width, and the label inherits the
	   button's own font so the line height matches the row it replaces. */
	.confirm-btn.confirm-btn.confirm-btn.confirm-btn--armed,
	.confirm-btn.confirm-btn.confirm-btn.confirm-btn--armed:hover,
	.confirm-btn.confirm-btn.confirm-btn.confirm-btn--armed:disabled {
		background: var(--color-danger-bg);
		border-color: var(--color-danger-bg);
		color: var(--color-danger-text);
		min-width: fit-content;
		filter: none;
		overflow: visible;
	}

	/* A glyph-only button has no padding of its own (`padding: 0` on
	   `.btn-action`/`--icon`), so an armed label would sit flush against the
	   border on both sides. Give the armed face breathing room; unarmed keeps
	   the exact 28px / 2rem cell. */
	.confirm-btn--icon.confirm-btn--armed {
		padding-inline: var(--space-2);
	}

	.confirm-btn--armed .confirm-btn__label {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		padding-inline-start: var(--space-1);
		white-space: nowrap;
		overflow: hidden;
		font-weight: 600;
		letter-spacing: normal;
		animation: confirm-btn-in var(--motion-base) var(--ease-standard);
	}

	/* No caller text (a bare delete glyph), so there is no line box to match:
	   the label's default line height would make the button taller than the
	   icon-only face it replaces. One em keeps it level with the glyph. */
	.confirm-btn:not(.confirm-btn--text) .confirm-btn__label {
		line-height: 1;
	}

	/* The button's width is content-driven, so the label is what grows: capping
	   its `max-width` and widening it frame by frame drags the button's box
	   along with it. A `transition: width` cannot interpolate intrinsic sizes,
	   which is why the width used to jump while the label faded in. */
	@keyframes confirm-btn-in {
		from {
			max-width: 0;
			padding-inline-start: 0;
			opacity: 0;
		}

		to {
			max-width: 12rem;
			opacity: 1;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.confirm-btn__label {
			animation: none;
		}
	}
</style>
