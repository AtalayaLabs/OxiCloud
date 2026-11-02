import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { ui, promptDialog } = vi.hoisted(() => ({ ui: { notify: vi.fn() }, promptDialog: vi.fn() }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ promptDialog }));
vi.mock('$lib/api/endpoints/groups', () => ({
	INTERNAL_GROUP_ID: '00000000-0000-0000-0000-000000000001',
	createGroup: vi.fn(),
	deleteGroup: vi.fn(),
	addGroupMember: vi.fn(),
	addUserMember: vi.fn(),
	groupDescription: (g: { description?: string | null }) => g.description ?? null,
	groupDisplayName: (g: { name: string }) => g.name,
	groupIconName: () => 'fa-users',
	listGroupsPage: vi.fn(),
	listMembers: vi.fn(),
	removeGroupMember: vi.fn(),
	removeUserMember: vi.fn(),
	renameGroup: vi.fn()
}));
vi.mock('$lib/api/endpoints/recipients', () => ({
	ensureResolvers: vi.fn(),
	resolveRecipient: (_t: string, id: string) => ({ id, label: id }),
	searchRecipients: vi.fn(async () => [])
}));

import { listGroupsPage, createGroup, listMembers } from '$lib/api/endpoints/groups';
import GroupsPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

beforeEach(() => {
	vi.clearAllMocks();
	m(listMembers).mockResolvedValue([]);
});

it('renders groups returned by the API', async () => {
	m(listGroupsPage).mockResolvedValue({
		items: [{ id: 'g1', name: 'Engineers', member_count: 2 }],
		total: 1
	});
	render(GroupsPage);
	await waitFor(() => expect(listGroupsPage).toHaveBeenCalled());
	await waitFor(() => expect(screen.getByText('Engineers')).toBeTruthy());
});

it('creates a new group', async () => {
	m(listGroupsPage).mockResolvedValue({ items: [], total: 0 });
	m(createGroup).mockResolvedValue(undefined);
	// `promptDialog` callers migrated to the inline-error UX pattern pass
	// `action: async (name) => createGroup(name.trim())` instead of
	// running the mutation outside the await. Simulate the real
	// `dialogs.resolve` flow by invoking `opts.action` with the
	// submitted value. See `frontend/AGENTS.md § Dialog error UX`.
	promptDialog.mockImplementation(async (opts) => {
		if (opts.action) await opts.action('New Team');
		return 'New Team';
	});
	render(GroupsPage);
	await waitFor(() => expect(listGroupsPage).toHaveBeenCalled());
	await fireEvent.click(screen.getByTestId('groups-create-btn'));
	await waitFor(() => expect(createGroup).toHaveBeenCalledWith('New Team'));
});
