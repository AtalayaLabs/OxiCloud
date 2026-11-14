<script lang="ts">
	/**
	 * Swimlane header label for a PERSON bucket: the avatar, then the name.
	 *
	 * Grouping a list by owner makes each lane a person, and a lane headed by
	 * a bare name is harder to scan than one anchored by a face — `/shared-
	 * with-me` established that and `/recent`, `/favorites` and anything else
	 * grouping on a user id want the same chrome. One component so they get
	 * the same chrome rather than four near-copies of it.
	 *
	 * Pass `userId` only when the ACTIVE axis is the people one. Every other
	 * axis keys its buckets on a category or a date string, and
	 * `<UserAvatar userId="Today">` would fetch `/api/users/Today` and then
	 * draw a coloured "T" circle as though a person called Today existed —
	 * so the avatar is gated on the axis, not on the key being truthy.
	 */
	import UserAvatar from '$lib/components/UserAvatar.svelte';

	interface Props {
		/** The bucket's user id, or null on any non-person axis. */
		userId?: string | null;
		/** Resolved label from the group-by's `labelOf` — a name, or a bucket name. */
		label: string;
		/** Avatar size; 30 matches the swimlane header's text. */
		size?: number;
	}
	let { userId = null, label, size = 30 }: Props = $props();
</script>

{#if userId}
	<!-- `title=""` suppresses the avatar's own tooltip: the name is already
	     spelled out immediately to its right, so a tooltip repeating it is
	     noise under every lane header. -->
	<UserAvatar {userId} {size} fallbackLabel={label} title="" />
{/if}
<span class="rl-swimlane-header__text">{label}</span>
