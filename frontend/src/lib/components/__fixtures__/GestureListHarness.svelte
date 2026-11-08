<!--
  Test host for `GestureList`.

  It exists because the component takes a SNIPPET for its rows, which a test
  cannot pass from `render()` — and because the component deliberately owns no
  order: a real consumer applies `onmove` to its own list, so a harness that
  does the same is the only way to pin that a drag reorders anything at all.
  The row renders just its id, which is all any assertion here needs.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import GestureList, { type SwipeAction } from '$lib/components/GestureList.svelte';

	export interface HarnessRow {
		id: string;
	}

	interface Props {
		initial: HarnessRow[];
		reorderCapable?: boolean;
		swipeLeft?: SwipeAction<HarnessRow>;
		swipeRight?: SwipeAction<HarnessRow>;
		ontap?: (item: HarnessRow, index: number) => void;
		onlongpress?: (item: HarnessRow, index: number) => void;
		/** Observed by the test; called after the harness applies the move. */
		onmoved?: (order: string[]) => void;
	}
	let { initial, reorderCapable, swipeLeft, swipeRight, ontap, onlongpress, onmoved }: Props =
		$props();

	// `initial` seeds the list once and is never read again — the harness owns
	// the order from then on, exactly as a real consumer does.
	let rows = $state(untrack(() => [...initial]));

	function move(from: number, to: number) {
		const next = [...rows];
		const [moved] = next.splice(from, 1);
		next.splice(to, 0, moved);
		rows = next;
		onmoved?.(rows.map((r) => r.id));
	}
</script>

<GestureList
	items={rows}
	key={(row) => row.id}
	{reorderCapable}
	onmove={move}
	{swipeLeft}
	{swipeRight}
	{ontap}
	{onlongpress}
>
	{#snippet row(item, i, state)}
		<button data-testid={`row-${item.id}`} class:hovered={state.hovered}>
			{item.id}#{i}
		</button>
	{/snippet}
</GestureList>
