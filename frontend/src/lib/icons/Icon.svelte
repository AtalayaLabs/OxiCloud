<script lang="ts">
	import { OxiIcons, type IconName } from './registry';

	interface Props {
		/** FA5-style icon name (without the `fa-` prefix), e.g. "folder". */
		name: IconName | string;
		/** Accessible label; when omitted the icon is decorative (aria-hidden). */
		title?: string;
		/** Extra classes forwarded to the <svg>. */
		class?: string;
	}

	let { name, title, class: className = '' }: Props = $props();

	const entry = $derived(OxiIcons[name as IconName]);
	const width = $derived(entry?.[0] ?? 512);
	const path = $derived(entry?.[1] ?? '');
</script>

{#if entry}
	<svg
		class={`oxi-icon ${className}`}
		viewBox={`0 0 ${width} 512`}
		fill="currentColor"
		role={title ? 'img' : undefined}
		aria-hidden={title ? undefined : 'true'}
		aria-label={title}
	>
		{#if title}<title>{title}</title>{/if}
		<!--
			`evenodd`, not the SVG default of `nonzero`.

			The registry's paths are authored as a solid outer shape followed by
			inner subpaths that are meant to CUT OUT of it — `ban` is a filled
			disc plus a diagonal bar, `exclamation-circle` a filled disc plus a
			"!". Those subpaths wind the same way as the outer shape, so under
			`nonzero` they fill too and the icon renders as a plain blob with
			its detail invisible. `evenodd` alternates, so they punch through.

			Safe for the rest: an icon whose subpaths do not overlap (arrows,
			bars) renders identically under either rule.
		-->
		<path d={path} fill-rule="evenodd" />
	</svg>
{/if}

<style>
	.oxi-icon {
		display: inline-block;
		width: 1em;
		height: 1em;
		vertical-align: -0.125em;
	}
</style>
