<script lang="ts">
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { player } from '$lib/stores/player.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import PlayerPanel from '$lib/components/PlayerPanel.svelte';

	/**
	 * The app's player: the `<audio>` element, a trigger in the top bar's user
	 * controls, and a popover holding `PlayerPanel`.
	 *
	 * The element lives HERE, outside the `{#if open}`, because this component
	 * is mounted for the whole session while the popover comes and goes. It
	 * used to live in `routes/music/+page.svelte`, so leaving Music unmounted
	 * it and cut playback mid-track — the bug this component exists to fix.
	 * For the same reason nothing about playback may move inside the
	 * conditional, and the `/player` route renders only `PlayerPanel`, never
	 * this.
	 *
	 * The popover mirrors the notification bell's shape (380px, same
	 * anchoring, dismissal and trigger geometry) so it introduces no new
	 * interaction to learn.
	 */
	let open = $state(false);
	let audioEl = $state<HTMLAudioElement | null>(null);
	let wrapper = $state<HTMLElement | null>(null);

	$effect(() => {
		player.attach(audioEl);
		// Pick the previous session back up — the queue, the modes and the
		// position. Must follow `attach`, since restoring sets the element's
		// source and asks it to play; `restore` itself is idempotent.
		if (audioEl) player.restore();
	});

	// A tab can vanish without a `pause`: closed, navigated away from, or sent
	// to the background on a phone where the OS may kill it outright.
	// `pagehide` is the last event guaranteed to run in all three, and
	// `visibilitychange` covers the phone case early enough to matter.
	$effect(() => {
		const save = () => player.persist();
		window.addEventListener('pagehide', save);
		document.addEventListener('visibilitychange', save);
		return () => {
			window.removeEventListener('pagehide', save);
			document.removeEventListener('visibilitychange', save);
		};
	});

	// Keep the element's source on the current track.
	$effect(() => {
		// Touch the dependency explicitly: `syncSource` reads it internally,
		// and an effect only tracks what it reads in its own body.
		void player.current;
		player.syncSource();
	});

	// Media keys / lock screen / headset buttons. Registered once; the
	// handlers stay valid because they close over the store, not over state.
	$effect(() => {
		player.bindMediaKeys();
	});

	// Dismissal, matching the notification panel: outside click or Escape.
	$effect(() => {
		if (!open) return;
		const onPointerDown = (e: MouseEvent) => {
			if (wrapper && !wrapper.contains(e.target as Node)) open = false;
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') open = false;
		};
		window.addEventListener('mousedown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('mousedown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});

	function onLoadedMetadata() {
		const patched = player.onLoadedMetadata();
		// The music page keeps its own copy of the visible rows; tell it a
		// duration was discovered so a row showing 0:00 can fill in.
		if (patched) {
			window.dispatchEvent(new CustomEvent('oxicloud:track-duration', { detail: patched }));
		}
	}

	function onError() {
		player.onError();
		ui.notify(
			t('music.playback_error', { name: player.label() }, 'Playback error: {{name}}'),
			'error'
		);
	}
</script>

<!--
	Nothing at all when the deployment has music switched off: no trigger, and
	no `<audio>` either, since there is nothing to play. Gated HERE rather than
	at the call site so no mount point can forget it. `features.music` is the
	same flag that hides `/music` from the sidebar, so the player disappears
	with the section it belongs to.
-->
{#if serverConfig.features.music}
	<!-- Never inside `{#if open}` — see the component doc. -->
	<audio
		bind:this={audioEl}
		onplay={() => player.onPlay()}
		onpause={() => player.onPause()}
		ontimeupdate={() => player.onTimeUpdate()}
		onloadedmetadata={onLoadedMetadata}
		onended={() => player.onEnded()}
		onerror={onError}
	></audio>

	<div class="player-wrapper" class:open bind:this={wrapper}>
		<button
			class="player-trigger"
			class:active={open}
			aria-haspopup="dialog"
			aria-expanded={open}
			title={t('music.player', 'Player')}
			aria-label={t('music.player', 'Player')}
			data-testid="music-player-toggle-btn"
			onclick={() => (open = !open)}
		>
			<Icon name="music" />
			<!--
				State badge, only once something is queued: an empty player has no
				state worth reporting, and a permanent badge would train people to
				ignore it. Mirrors the notification bell's badge placement, so the
				two controls beside each other behave alike.
			-->
			{#if player.active}
				<span class="player-trigger__badge" aria-hidden="true">
					<Icon name={player.playing ? 'play' : 'pause'} />
				</span>
			{/if}
		</button>

		{#if open}
			<div class="player-panel" role="dialog" aria-label={t('music.player', 'Player')}>
				<PlayerPanel onclose={() => (open = false)} />
			</div>
		{/if}
	</div>
{/if}

<style>
	.player-wrapper {
		position: relative;
		display: flex;
		align-items: center;
	}

	/* Phones reach the player through the footer's Player entry, which goes to
	   `/player`. Showing the trigger here as well would be two controls for
	   one thing, where the footer one is the better of the two — bigger, in
	   reach of a thumb, and it honours the back gesture.

	   Only the TRIGGER goes: the `<audio>` element is a sibling of this
	   wrapper, outside the rule, because hiding the player must never stop
	   the music. Same breakpoint as `.mobile-nav`'s. */
	@media (width <= 768px) {
		.player-wrapper {
			display: none;
		}
	}

	/* Geometry and states lifted from `.notif-bell-btn` (notifications.css):
	   40px round, same resting colour, same tinted circle on hover, same
	   accent ring while its panel is open. They sit next to each other, so
	   anything less than identical reads as a mistake — and the badge's
	   `top/right: 4px` only lands where the bell's does because the button
	   underneath is the same size. */
	.player-trigger {
		position: relative;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 40px;
		height: 40px;
		border: none;
		border-radius: 50%;
		background: none;
		color: var(--color-text-subtle);
		font-size: var(--text-lg);
		cursor: pointer;
		transition: all 0.2s ease;
	}

	.player-trigger:hover {
		background: var(--color-accent-bg-sm);
		color: var(--color-accent);
	}

	.player-trigger.active {
		background: var(--color-accent-ring);
		color: var(--color-accent);
	}

	/* Geometry copied from `.notif-badge` (notifications.css) — same corner,
	   same 16px, same radius and colours — so the two badges in this corner
	   of the bar are plainly the same device reporting different things. The
	   glyph is small inside it because a play triangle at 10px reads as a
	   smudge where a digit does not. */
	.player-trigger__badge {
		position: absolute;
		top: 4px;
		right: 4px;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 16px;
		height: 16px;
		border-radius: var(--radius-lg);
		background: var(--color-notification-badge);
		color: var(--color-notification-bg);
		font-size: 7px;
		line-height: 1;
		pointer-events: none;
	}

	/* Mirrors `.notif-panel` (notifications.css): same width, offset, radius
	   and elevation, so the two panels in this corner read as siblings. */
	.player-panel {
		position: absolute;
		top: calc(100% + 10px);
		right: -40px;
		z-index: 2000;
		display: flex;
		flex-direction: column;
		width: 380px;
		max-height: 480px;
		padding: var(--space-4);
		gap: var(--space-3);
		background: var(--color-notification-bg);
		border-radius: var(--radius-3xl);
		/* Clip to the rounded box, as `.notif-panel` does: the queue scrolls
		   inside, and nothing can paint over the corners. */
		overflow: hidden;
		box-shadow:
			0 12px 40px var(--color-shadow-md),
			0 0 0 1px var(--color-shadow-xs);
	}
</style>
