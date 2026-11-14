import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { ui, goto } = vi.hoisted(() => ({ ui: { notify: vi.fn() }, goto: vi.fn() }));
vi.mock('$app/navigation', () => ({ goto }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/utils/errors', () => ({
	errorMessage: (e: Error) => e.message,
	errorToast: vi.fn()
}));
vi.mock('$lib/api/endpoints/grants', () => ({
	displayRole: (r: string) => r,
	expiryToIso: (v: string | null) => v,
	fetchMyShares: vi.fn(),
	notifyGrantRecipient: vi.fn(async () => ({ outcomes: [] })),
	revokeGrant: vi.fn(),
	updateGrantRole: vi.fn()
}));
vi.mock('$lib/api/endpoints/recipients', () => ({
	ensureResolvers: vi.fn(),
	resolveLabel: (_t: string, id: string) => id
}));
vi.mock('$lib/api/endpoints/shares', () => ({
	copyShareLink: vi.fn(),
	deleteShare: vi.fn(),
	getShareById: vi.fn(),
	updateShare: vi.fn()
}));
vi.mock('$lib/api/endpoints/files', () => ({ fileInlineUrl: () => '/in' }));

import { fetchMyShares, updateGrantRole, revokeGrant } from '$lib/api/endpoints/grants';
import { ensureResolvers } from '$lib/api/endpoints/recipients';
import SharedPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function grantItem() {
	return {
		resource_type: 'folder' as const,
		resource: {
			category: 'Folder',
			created_at: 0,
			id: 'r1',
			modified_at: 0,
			name: 'Docs',
			created_by: 'me',
			updated_by: 'me',
			parent_id: null,
			path: '/Docs',
			etag: 'e'
		},
		first_shared_at: '2024-01-01T00:00:00Z',
		grants: [
			{
				grant_id: 'g1',
				subject_type: 'user' as const,
				subject_id: 'u1',
				subject_display: 'Bob',
				role: 'viewer' as const,
				granted_at: '2024-01-01T00:00:00Z',
				expires_at: null,
				has_password: false,
				is_external: false
			}
		]
	};
}

beforeEach(() => {
	vi.clearAllMocks();
	m(ensureResolvers).mockResolvedValue(undefined);
});

it('renders a swimlane for each shared resource', async () => {
	m(fetchMyShares).mockResolvedValue({ items: [grantItem()], next_cursor: null });
	render(SharedPage);
	await waitFor(() => expect(fetchMyShares).toHaveBeenCalled());
	await screen.findByTestId('shared-kebab-g1');
});

it('changes a grant role from the kebab menu', async () => {
	m(fetchMyShares).mockResolvedValue({ items: [grantItem()], next_cursor: null });
	m(updateGrantRole).mockResolvedValue(undefined);
	render(SharedPage);
	await fireEvent.click(await screen.findByTestId('shared-kebab-g1'));
	await fireEvent.click(await screen.findByTestId('shared-role-editor-g1'));
	await waitFor(() =>
		expect(updateGrantRole).toHaveBeenCalledWith(
			{ type: 'user', id: 'u1' },
			{ type: 'folder', id: 'r1' },
			'editor',
			null
		)
	);
});

it('revokes access from the kebab menu', async () => {
	m(fetchMyShares).mockResolvedValue({ items: [grantItem()], next_cursor: null });
	m(revokeGrant).mockResolvedValue(undefined);
	render(SharedPage);
	await fireEvent.click(await screen.findByTestId('shared-kebab-g1'));
	await fireEvent.click(await screen.findByTestId('shared-remove-access-g1'));
	await waitFor(() => expect(revokeGrant).toHaveBeenCalledWith('g1'));
});

// The lane header lost its "Edit sharing" text when it became an icon
// (2026-11-14), and an icon-only control with no accessible name is the
// classic way that goes wrong — invisible to a screen reader, unfindable by
// role. Queried BY NAME on purpose: a testid-only assertion would still pass
// with the aria-label dropped.
it('names the share control even though it shows only an icon', async () => {
	m(fetchMyShares).mockResolvedValue({ items: [grantItem()], next_cursor: null });
	render(SharedPage);
	const share = await screen.findByRole('button', { name: 'Edit sharing' });
	expect(share.getAttribute('data-testid')).toBe('shared-edit-sharing-r1');
});

// The swimlane names its resource the way a list row does, which means the
// glyph carries the type tint — `file-icon--row` supplies the chip geometry
// and `file-icon--folder` the `--fk` colour. Dropping either leaves a bare
// 100×70 `.file-icon` box in the header, which is what this pins against.
it('renders the resource glyph with the list-row icon classes', async () => {
	m(fetchMyShares).mockResolvedValue({ items: [grantItem()], next_cursor: null });
	render(SharedPage);
	const lane = await screen.findByTestId('shared-lane-open-r1');
	const glyph = lane.querySelector('.file-icon');
	expect(glyph).not.toBeNull();
	expect(glyph?.classList.contains('file-icon--row')).toBe(true);
	expect(glyph?.classList.contains('file-icon--folder')).toBe(true);
});

it('surfaces a load error without crashing', async () => {
	m(fetchMyShares).mockRejectedValue(new Error('boom'));
	render(SharedPage);
	await waitFor(() => expect(fetchMyShares).toHaveBeenCalled());
});
