<!--
	Test-only host for `FolderBreadcrumb`'s `reloadToken`.

	It exists because `@testing-library/svelte`'s `rerender()` re-runs the
	component's effects whether or not the prop it changed is one they read:
	a refetch test written with `rerender` passes even with the dependency
	deleted (measured — it fires the fetch twice either way), so it pins
	nothing. Owning the token in reactive state here, the way a page does,
	makes the dependency observable: bumping it refetches, and touching
	other state does not.

	Not referenced from any route, so it never reaches the bundle.
-->
<script lang="ts">
	import FolderBreadcrumb from '../FolderBreadcrumb.svelte';

	let { folderId }: { folderId: string } = $props();

	let token = $state(0);
	/** Unrelated reactive state, to prove the refetch isn't just "any render". */
	let unrelated = $state(0);
</script>

<button data-testid="harness-bump" onclick={() => token++}>bump</button>
<button data-testid="harness-unrelated" onclick={() => unrelated++}>unrelated {unrelated}</button>
<FolderBreadcrumb {folderId} reloadToken={token} />
