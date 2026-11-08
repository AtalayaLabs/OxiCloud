import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/svelte';

const { pageState } = vi.hoisted(() => ({
	pageState: { url: new URL('http://localhost/files') }
}));
vi.mock('$app/state', () => ({ page: pageState }));

import MobileNav from './MobileNav.svelte';
import { player } from '$lib/stores/player.svelte';
import { serverConfig } from '$lib/stores/serverConfig.svelte';

beforeEach(() => {
	pageState.url = new URL('http://localhost/files');
	player.queue = [];
	serverConfig.features = { ...serverConfig.features, music: true };
});

it('renders the four mobile destinations', () => {
	render(MobileNav);
	expect(screen.getByTestId('mobile-nav-files-link').getAttribute('href')).toBe('/files');
	expect(screen.getByTestId('mobile-nav-photos-link').getAttribute('href')).toBe('/photos');
	expect(screen.getByTestId('mobile-nav-notifications-link')).toBeTruthy();
	expect(screen.getByTestId('mobile-nav-account-link').getAttribute('href')).toBe('/account');
});

// Order is part of the design: Files · Media · Player · Notifications ·
// Account. Player is a button among links, so it is rendered separately and
// its position is the kind of thing a refactor silently moves.
it('puts Player between Media and Notifications', () => {
	const { container } = render(MobileNav);
	const items = [...container.querySelectorAll('.mobile-nav__item')].map((el) =>
		el.getAttribute('data-testid')
	);
	expect(items).toEqual([
		'mobile-nav-files-link',
		'mobile-nav-photos-link',
		'mobile-nav-player-link',
		'mobile-nav-notifications-link',
		'mobile-nav-account-link'
	]);
});

// Same gate as the sidebar's `/music` link and the top-bar player: a
// deployment with music off must not show a control for it.
it('omits Player when music is disabled', () => {
	serverConfig.features = { ...serverConfig.features, music: false };
	render(MobileNav);
	expect(screen.queryByTestId('mobile-nav-player-link')).toBeNull();
	// The four destinations are untouched by that flag.
	expect(screen.getByTestId('mobile-nav-files-link')).toBeTruthy();
	expect(screen.getByTestId('mobile-nav-account-link')).toBeTruthy();
});

// A LINK to `/player`, like every other entry here, and for a reason worth
// pinning: it was briefly a button opening a full-screen sheet, which has no
// history entry — so Android's back gesture navigated the page behind it
// instead of dismissing it. Notifications and Account are pages; so is this.
it('links Player to its page rather than opening an overlay', () => {
	render(MobileNav);
	const link = screen.getByTestId('mobile-nav-player-link');
	expect(link.tagName).toBe('A');
	expect(link.getAttribute('href')).toBe('/player');
});

it('marks Player active on its own route', () => {
	pageState.url = new URL('http://localhost/player');
	render(MobileNav);
	expect(screen.getByTestId('mobile-nav-player-link').getAttribute('aria-current')).toBe('page');
});

// The badge reports playback state, so it has nothing to say with an empty
// queue — and a badge that is always there is one people stop reading.
it('badges the Player entry only once something is queued', () => {
	const { container, unmount } = render(MobileNav);
	expect(container.querySelector('.mobile-nav__badge--glyph')).toBeNull();
	unmount();

	player.queue = [
		{
			id: 'a',
			title: 'a',
			file_id: 'f',
			duration_secs: null
		} as unknown as (typeof player.queue)[0]
	];
	const second = render(MobileNav);
	expect(second.container.querySelector('.mobile-nav__badge--glyph')).toBeTruthy();
});

it('marks the current section active, including nested routes', () => {
	pageState.url = new URL('http://localhost/photos/albums');
	render(MobileNav);
	expect(screen.getByTestId('mobile-nav-photos-link').getAttribute('aria-current')).toBe('page');
	expect(screen.getByTestId('mobile-nav-files-link').getAttribute('aria-current')).toBeNull();
});
