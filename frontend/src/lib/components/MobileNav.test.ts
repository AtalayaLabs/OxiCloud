import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/svelte';

const { pageState } = vi.hoisted(() => ({
	pageState: { url: new URL('http://localhost/files') }
}));
vi.mock('$app/state', () => ({ page: pageState }));

import MobileNav from './MobileNav.svelte';

beforeEach(() => {
	pageState.url = new URL('http://localhost/files');
});

it('renders the four mobile destinations', () => {
	render(MobileNav);
	expect(screen.getByTestId('mobile-nav-files-link').getAttribute('href')).toBe('/files');
	expect(screen.getByTestId('mobile-nav-photos-link').getAttribute('href')).toBe('/photos');
	expect(screen.getByTestId('mobile-nav-notifications-link')).toBeTruthy();
	expect(screen.getByTestId('mobile-nav-account-link').getAttribute('href')).toBe('/account');
});

it('marks the current section active, including nested routes', () => {
	pageState.url = new URL('http://localhost/photos/albums');
	render(MobileNav);
	expect(screen.getByTestId('mobile-nav-photos-link').getAttribute('aria-current')).toBe('page');
	expect(screen.getByTestId('mobile-nav-files-link').getAttribute('aria-current')).toBeNull();
});
