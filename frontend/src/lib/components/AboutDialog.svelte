<script lang="ts">
	import { apiFetch } from '$lib/api/client';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';

	let { open = $bindable(false) }: { open?: boolean } = $props();

	let appVersion = $state('');

	$effect(() => {
		if (!open || appVersion) return;
		void (async () => {
			try {
				const r = await apiFetch('/api/version', { credentials: 'same-origin' });
				if (r.ok) {
					const data = (await r.json()) as { version?: string };
					if (data.version) appVersion = `v${data.version}`;
				}
			} catch {
				/* version stays blank — non-critical */
			}
		})();
	});
</script>

<svelte:window
	onkeydown={(e) => {
		if (e.key === 'Escape' && open) open = false;
	}}
/>

{#if open}
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div class="about-overlay" onclick={(e) => e.target === e.currentTarget && (open = false)}>
		<div class="about-modal" role="dialog" aria-modal="true" aria-labelledby="about-modal-title">
			<div class="about-modal__logo">
				<svg viewBox="95 67 320 320" aria-hidden="true">
					<path
						d="M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z"
					/>
				</svg>
			</div>
			<h2 id="about-modal-title" class="about-modal__title">OxiCloud</h2>
			<div class="about-modal__version">{appVersion || 'v…'}</div>
			<p class="about-modal__desc">
				{t(
					'user_menu.about_description',
					'Cloud storage platform built with Rust & Clean Architecture. Fast, secure, and private.'
				)}
			</p>
			<div class="about-modal__tech">
				<span class="about-modal__badge">Rust</span>
				<span class="about-modal__badge">Axum</span>
				<span class="about-modal__badge">PostgreSQL</span>
				<span class="about-modal__badge">Clean Architecture</span>
			</div>
			<div class="about-modal__links">
				<a
					class="about-modal__link"
					href="https://github.com/AtalayaLabs/OxiCloud/"
					target="_blank"
					rel="noopener"
					data-testid="appshell-about-github-link"
				>
					<Icon name="github" /> GitHub
				</a>
				<a
					class="about-modal__link"
					href="https://github.com/AtalayaLabs/OxiCloud/blob/main/LICENSE"
					target="_blank"
					rel="noopener"
					data-testid="appshell-about-license-link"
				>
					<Icon name="file-alt" />
					{t('user_menu.mit_license', 'MIT License')}
				</a>
			</div>
			<button
				class="about-modal__close"
				data-testid="appshell-about-close-btn"
				onclick={() => (open = false)}
			>
				{t('actions.close', 'Close')}
			</button>
		</div>
	</div>
{/if}

<style>
	.about-overlay {
		position: fixed;
		inset: 0;
		z-index: var(--z-modal);
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 1rem;
		background: var(--color-overlay);
		animation: about-fade 0.18s ease;
	}

	.about-modal {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-3);
		width: min(92vw, 26rem);
		padding: var(--space-6) var(--space-5);
		background: var(--color-bg-surface);
		color: var(--color-text);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-xl);
		text-align: center;
		animation: about-pop 0.2s ease;
	}

	.about-modal__logo {
		/* 73px (not 64) so the cloud keeps its rendered scale after the viewBox
		   grew 280→320 to stop clipping its left bulge: 73/320 ≈ 64/280. */
		width: 73px;
		height: 73px;
		color: var(--color-accent);
	}

	.about-modal__logo svg {
		width: 100%;
		height: 100%;
		fill: currentColor;
	}

	.about-modal__title {
		margin: 0;
		font-size: var(--text-xl, 1.5rem);
		color: var(--color-text-heading);
	}

	.about-modal__version {
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}

	.about-modal__desc {
		margin: 0;
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		line-height: 1.5;
	}

	.about-modal__tech {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		gap: var(--space-2);
	}

	.about-modal__badge {
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius-pill, 999px);
		background: var(--color-bg-muted);
		color: var(--color-text-secondary, var(--color-text-muted));
		font-size: var(--text-xs, 0.75rem);
	}

	.about-modal__links {
		display: flex;
		gap: var(--space-4);
	}

	.about-modal__link {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		color: var(--color-primary);
		text-decoration: none;
		font-size: var(--text-sm);
	}

	.about-modal__link:hover {
		text-decoration: underline;
	}

	.about-modal__close {
		margin-top: var(--space-2);
		padding: var(--space-2) var(--space-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		cursor: pointer;
		font-size: var(--text-sm);
	}

	.about-modal__close:hover {
		background: var(--color-bg-hover);
	}

	@keyframes about-fade {
		from {
			opacity: 0;
		}

		to {
			opacity: 1;
		}
	}

	@keyframes about-pop {
		from {
			opacity: 0;
			transform: scale(0.96);
		}

		to {
			opacity: 1;
			transform: scale(1);
		}
	}
</style>
