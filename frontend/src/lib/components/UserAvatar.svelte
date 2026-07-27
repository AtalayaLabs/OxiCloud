<script lang="ts">
	/**
	 * Avatar-only chip — no name / email, just the circle. Extracted from
	 * `<UserVignette>` (which pairs avatar + name + email for share
	 * dialogs / owner cells) so surfaces that only have room for the
	 * avatar (grid-card overlay, swimlane header prefix) don't drag the
	 * text half in with them.
	 *
	 * Same visual grammar as UserVignette: uploaded photo when present,
	 * otherwise coloured initials from `userInitials()` + `avatarColorIndex()`.
	 * Same lazy `resolveUser` fetch (cached) so external-user avatars
	 * populate without a per-mount round-trip.
	 */
	import { resolveUser, type ResolvedUser } from '$lib/api/endpoints/users';
	import { userInitials, avatarColorIndex } from '$lib/utils/avatar';

	interface Props {
		userId: string;
		/** Fallback label if `resolveUser` fails or is still resolving. */
		fallbackLabel?: string;
		/** Pixel size — defaults to 24 (badge / swimlane scale). */
		size?: number;
		/**
		 * Optional title text override. Defaults to the resolved display
		 * name so browsers show a tooltip on hover. Pass empty string
		 * to suppress the tooltip.
		 */
		title?: string;
	}
	let { userId, fallbackLabel, size = 24, title }: Props = $props();

	let resolved = $state<ResolvedUser | null>(null);
	$effect(() => {
		let alive = true;
		resolved = null;
		void resolveUser(userId).then((u) => {
			if (alive) resolved = u;
		});
		return () => {
			alive = false;
		};
	});

	const label = $derived(resolved?.name ?? fallbackLabel ?? userId);
	const image = $derived(resolved?.image ?? null);
	const colorIndex = $derived(avatarColorIndex(userId));
	const initials = $derived(userInitials(label));
	const tooltip = $derived(title !== undefined ? title : label);
</script>

<span class="ua" style:width={`${size}px`} style:height={`${size}px`} title={tooltip || undefined}>
	{#if image}
		<img class="ua__photo" src={image} alt="" />
	{:else}
		<span class="ua__initials ua__initials--c{colorIndex}" style:font-size={`${size * 0.42}px`}>
			{initials}
		</span>
	{/if}
</span>

<style>
	.ua {
		/* `inline-block` (not `inline-flex`) so children with
		   `width/height: 100%` reliably fill the box. Ed 2026-07-26:
		   the previous `inline-flex + align-items: center` shape gave
		   the `<img>` a cross-axis intrinsic height (~6 px in a 22 px
		   box); `align-self: stretch` on the img wasn't enough to
		   defeat the parent's alignment rule in some contexts. Block
		   sizing sidesteps flex-alignment quirks entirely.
		   `vertical-align: middle` aligns the chip with adjacent
		   text (swimlane header prefix, etc.). */
		display: inline-block;
		position: relative;
		flex-shrink: 0;
		border-radius: var(--radius-full);
		overflow: hidden;
		vertical-align: middle;
	}

	.ua__photo {
		display: block;
		width: 100%;
		height: 100%;
		object-fit: cover;
	}

	.ua__initials {
		/* `display: flex` (block-level) — NOT `inline-flex`. An inline-
		   flex child inside the `.ua` `inline-block` parent participates
		   in inline layout, gets baseline-aligned, and rendered outside
		   its parent's box for text with descenders (Ed 2026-07-26).
		   Block-flex fills the container unambiguously. */
		display: flex;
		width: 100%;
		height: 100%;
		align-items: center;
		justify-content: center;
		color: var(--color-on-accent);
		font-weight: var(--weight-semibold);
		text-transform: uppercase;
	}

	/* Colour buckets mirror the shared avatar palette used by
	   `AppShell` / `UserVignette` — 5 slots (see `avatarColorIndex()`
	   which does `Math.abs(hash) % 5`). Same badge tokens so a user's
	   avatar renders in the identical colour across surfaces. */
	.ua__initials--c0 {
		background: var(--color-badge-indigo-bg);
		color: var(--color-badge-indigo-text);
	}

	.ua__initials--c1 {
		background: var(--color-badge-green-bg);
		color: var(--color-badge-green-text);
	}

	.ua__initials--c2 {
		background: var(--color-badge-orange-bg);
		color: var(--color-badge-orange-text);
	}

	.ua__initials--c3 {
		background: var(--color-badge-blue-bg);
		color: var(--color-badge-blue-text);
	}

	.ua__initials--c4 {
		background: var(--color-badge-amber-bg);
		color: var(--color-badge-amber-text);
	}
</style>
