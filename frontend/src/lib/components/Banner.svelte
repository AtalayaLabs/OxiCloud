<script lang="ts">
	/**
	 * One banner — the only banner component in the codebase.
	 *
	 * Supersedes the earlier `ReadOnlyBanner` + `OpsBannerStack` split.
	 * This primitive handles both:
	 *
	 * - **Fixed-copy system banners** (drive-frozen, server read-only
	 *   mode, background key rotation). Callers pass the already-
	 *   translated title / body / progress and pick the severity.
	 * - **Operator-authored banners** (markdown content, per-locale,
	 *   dismissable). Callers pre-render the markdown into safe HTML
	 *   (`$lib/utils/opsBannerMarkdown`) and pass it via `html`.
	 *
	 * ## Why one component
	 *
	 * The two use-cases shared ~90% of visual + a11y + layout logic.
	 * Keeping them split meant every tweak (new severity colour,
	 * progress-bar sub-line, accessibility fix) had to land in two
	 * places. This primitive is props-only — no fixed copy, no i18n
	 * lookups inside — so the caller stays in charge of the
	 * message and the component stays in charge of rendering it.
	 *
	 * ## Severity axis
	 *
	 * - `info` — neutral/accent border; "something is happening,
	 *   nothing is wrong" (background rotation, drive-frozen).
	 * - `notification` — same shape as info with the info-tinted
	 *   background (operator-authored informational banners).
	 * - `warning` — warning-tinted accent (server read-only mode,
	 *   operator-authored warnings).
	 * - `critical` — reserved for the future "server degraded"
	 *   class.
	 *
	 * ## Body: `body` vs `html`
	 *
	 * Mutually exclusive. `body` renders as plain text (default
	 * HTML-escape). `html` renders via `{@html ...}` and is the
	 * caller's responsibility — pre-sanitize before passing.
	 * `renderOpsBannerMarkdown` is the designated sanitizer for
	 * operator markdown.
	 *
	 * ## Not dismissable (intentional)
	 *
	 * Earlier versions carried a dismiss (×) button + localStorage
	 * state. Removed: it leaked ids forever (operator-deleted banners
	 * stayed in each user's dismissed set), and the UX cost of
	 * "notification banners nag until operator deletes" is lower than
	 * the correctness cost of invisible stale client state.
	 *
	 * If a future surface needs dismiss semantics (e.g., tour prompts,
	 * one-time onboarding), it should carry its own dismiss state —
	 * do not re-add it to the generic primitive.
	 */
	import Icon from '$lib/icons/Icon.svelte';

	export interface BannerProgress {
		/** Short label — target name, affected entry, etc. */
		target: string;
		migrated: number;
		total: number;
		/** 0–100 integer, for the progress line. */
		percent: number;
	}

	interface Props {
		/** Picks the accent colour and the default icon. */
		severity?: 'info' | 'notification' | 'warning' | 'critical';
		/** Icon override — any `OxiIcons` key. Default: lock (info /
		 *  warning / critical) or bell (notification). */
		icon?: string;
		/** Bold heading line. Omit to render only the body. */
		title?: string;
		/** Plain-text body (HTML-escaped). Mutually exclusive with
		 *  `html`. */
		body?: string;
		/** Pre-sanitized HTML body (`{@html ...}` is used). Caller
		 *  OWNS sanitization. */
		html?: string;
		/** Optional progress sub-line — the migration/rotation case
		 *  shows "25% (120 / 480 blobs)" below the body. */
		progress?: BannerProgress;
		/** Optional secondary line for things like "25% complete"
		 *  without the full migration/total breakdown. */
		progressLine?: string;
		/** data-testid on the wrapping region — used by e2e tests
		 *  and the legacy call sites that pinned to specific IDs. */
		testid?: string;
		/** aria-label on the wrapping region. */
		ariaLabel?: string;
		/** Role on the wrapping region — `region` (default), `alert`
		 *  for high-urgency warnings, `status` for low-urgency
		 *  notifications. */
		role?: 'region' | 'alert' | 'status';
	}

	let {
		severity = 'info',
		icon,
		title,
		body,
		html,
		progress,
		progressLine,
		testid,
		ariaLabel,
		role = 'region'
	}: Props = $props();

	/** Default icon per severity when the caller doesn't override:
	 *  - `info` → lock (drive-frozen / server-readonly semantics)
	 *  - `notification` → bell (operator announcement)
	 *  - `warning` → exclamation-triangle (NOT lock — a warning
	 *    banner isn't a locked-state indicator; the server-readonly
	 *    banner that IS locked passes `icon="lock"` explicitly)
	 *  - `critical` → exclamation-triangle (loud severity reuses the
	 *    same glyph, differentiated by the red palette) */
	const resolvedIcon = $derived(
		icon ??
			(severity === 'notification'
				? 'bell'
				: severity === 'warning' || severity === 'critical'
					? 'exclamation-triangle'
					: 'lock')
	);
</script>

<div class="banner banner--{severity}" {role} aria-label={ariaLabel} data-testid={testid}>
	<div class="banner__icon" aria-hidden="true">
		<Icon name={resolvedIcon} />
	</div>
	<div class="banner__body">
		{#if title}
			<strong>{title}</strong>
		{/if}
		{#if html}
			<!-- Caller-sanitized HTML. The only legitimate use of
			     `{@html}` in the codebase for user-authored content
			     passes through `renderOpsBannerMarkdown`. -->
			<!-- eslint-disable-next-line svelte/no-at-html-tags -->
			<span class="banner__text">{@html html}</span>
		{:else if body}
			<span class="banner__text">{body}</span>
		{/if}
		{#if progress}
			<span class="banner__progress">
				{progress.target} — {progress.percent}% ({progress.migrated} / {progress.total})
			</span>
		{:else if progressLine}
			<span class="banner__progress">{progressLine}</span>
		{/if}
	</div>
</div>

<style>
	/* Shape matches the sibling upgrade-banner in
	   `routes/shared-with-me/+page.svelte` so the family is visually
	   consistent — only the accent colour differs by severity. */
	.banner {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-3) var(--space-4);
		margin-bottom: var(--space-4);
		background: var(--color-surface-raised);
		border: 1px solid var(--color-border);
		border-left-width: 4px;
		border-radius: var(--radius-md);
	}

	/* Severity palette — tinted BACKGROUND + BORDER + ICON do the
	   signalling; title + body stay on `--color-text` so dark mode
	   stays readable rather than screaming saturated yellow at the
	   user. The strong colour lives on the edges and the icon chip,
	   not on the prose. */

	.banner--info {
		border-left-color: var(--color-accent);
	}

	/* Notification — blue family. Pale-blue tint in light mode, deep
	   navy in dark mode (both via `--color-info-bg`'s light-dark()).
	   Distinct from warnings and from the drive-frozen banners so
	   "informational announcement" reads differently from "something
	   is in a locked state". */
	.banner--notification {
		background: var(--color-info-bg);
		border-color: var(--color-info-border);
	}

	/* Warning — amber family. The one axis users should NOT scroll
	   past. Deeper amber background in dark mode (`#2a2410`) keeps
	   the banner distinct from the surface without over-saturating
	   on OLED screens; the orange border + warning icon do the
	   signalling. */
	.banner--warning {
		background: var(--color-warning-bg);
		border-color: var(--color-warning-border);
	}

	/* Critical — red family. Red-tinted background (`--color-danger-light-bg`
	   is pale pink in light mode, deep oxblood in dark mode) plus
	   the solid-red border. Not using the saturated-red TEXT token
	   on purpose — it was readable in light mode but screamed on
	   dark. The border + icon carry the severity. */
	.banner--critical {
		background: var(--color-danger-light-bg);
		border-color: var(--color-danger-bg);
	}

	.banner__icon {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 2rem;
		height: 2rem;
		border-radius: var(--radius-md);
		background: var(--color-surface);
		font-size: var(--text-lg);
	}

	.banner--info .banner__icon {
		color: var(--color-accent);
	}

	.banner--notification .banner__icon {
		color: var(--color-info-border);
	}

	.banner--warning .banner__icon {
		color: var(--color-warning-border);
	}

	.banner--critical .banner__icon {
		color: var(--color-danger-bg);
	}

	.banner__body {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
		flex: 1;
	}

	.banner__body strong {
		font-weight: var(--weight-semibold);
		color: var(--color-text);
	}

	.banner__text {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	/* Severity text colour: NO overrides. Title + body + progress
	   stay on `--color-text` / `--color-text-muted` across every
	   variant — the banner's signal lives in the icon chip + border
	   + tinted background. Earlier revisions inherited the warning
	   TEXT colour (`#fbbf24` bright yellow in dark mode) and the
	   result was unreadable on OLED screens. The quiet-text pattern
	   is also what GitHub, Linear and similar operator-tool banners
	   use. */

	/* Links / inline code inside operator-authored markdown bodies —
	   same tokens as elsewhere in the app. The :global selector reaches
	   into the sanitized HTML the renderer emits. */
	.banner__text :global(a) {
		color: inherit;
		text-decoration: underline;
	}

	.banner__text :global(code) {
		font-family: var(--font-mono, monospace);
		padding: 0 var(--space-1);
		border-radius: var(--radius-sm);
		background: var(--color-surface);
	}

	.banner__progress {
		color: var(--color-text-muted);
		font-size: var(--text-xs);
	}

	@media (width <= 600px) {
		.banner {
			align-items: flex-start;
		}
	}
</style>
