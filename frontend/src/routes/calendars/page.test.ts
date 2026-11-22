import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/calendars', () => ({
	createCalendar: vi.fn(),
	createEvent: vi.fn(),
	deleteCalendar: vi.fn(),
	deleteEvent: vi.fn(),
	listCalendars: vi.fn(),
	listEvents: vi.fn(async () => []),
	updateCalendar: vi.fn(),
	updateEvent: vi.fn()
}));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog: vi.fn(), promptDialog: vi.fn() }));
// The share dialog's own endpoints; the dialog itself is the real one.
vi.mock('$lib/api/endpoints/grants', () => ({
	createGrant: vi.fn(),
	fetchGrantsForResource: vi.fn(async () => []),
	notifyGrantRecipient: vi.fn(),
	revokeGrant: vi.fn(),
	updateGrantRole: vi.fn()
}));
vi.mock('$lib/api/endpoints/shares', () => ({
	copyShareLink: vi.fn(),
	createShare: vi.fn(),
	deleteShare: vi.fn(),
	listSharesForItem: vi.fn(async () => []),
	updateShare: vi.fn()
}));
vi.mock('$lib/api/endpoints/recipients', () => ({
	ensureResolvers: vi.fn(),
	isDirectoryAvailable: () => true,
	resolveRecipient: (_t: string, id: string) => ({ id, label: id }),
	searchRecipients: vi.fn(async () => [])
}));
vi.mock('$lib/api/endpoints/folders', () => ({ getFolderAncestorsWithGrants: vi.fn() }));
vi.mock('$lib/api/endpoints/files', () => ({ getFile: vi.fn() }));
vi.mock('$lib/stores/drives.svelte', () => ({ drives: { drives: [], load: vi.fn() } }));

import { listCalendars } from '$lib/api/endpoints/calendars';
import { fetchGrantsForResource } from '$lib/api/endpoints/grants';
import { listSharesForItem } from '$lib/api/endpoints/shares';
import Page from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

const calendar = (id: string, name: string, access: string) => ({
	id,
	name,
	owner_id: 'u-owner',
	access,
	is_readonly: access === 'read',
	color: null,
	description: null,
	created_at: '2026-11-01T00:00:00Z',
	updated_at: '2026-11-01T00:00:00Z'
});

beforeEach(() => {
	vi.clearAllMocks();
	m(listCalendars).mockResolvedValue([
		calendar('cal-own', 'Tábor', 'own'),
		calendar('cal-shared', 'Rodina', 'edit')
	]);
});

// Sharing sits next to rename and delete, so only the owner gets it.
it('offers sharing on owned calendars only', async () => {
	render(Page);
	await screen.findByText('Tábor');
	expect(screen.getByTestId('calendar-share-cal-own')).toBeTruthy();
	expect(screen.queryByTestId('calendar-share-cal-shared')).toBeNull();
});

it('opens the share dialog on the calendar, without public links', async () => {
	render(Page);
	await fireEvent.click(await screen.findByTestId('calendar-share-cal-own'));
	await screen.findByTestId('share-dialog');
	await waitFor(() => expect(fetchGrantsForResource).toHaveBeenCalledWith('calendar', 'cal-own'));
	expect(screen.queryByTestId('share-dialog-link-tab')).toBeNull();
	expect(listSharesForItem).not.toHaveBeenCalled();
});
