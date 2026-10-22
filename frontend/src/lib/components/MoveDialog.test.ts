import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// The dialog now sources its starting folder from the drives store
// (D6 drive switcher), not from `session.loadHomeFolder`. We mock a
// single default-personal drive whose `root_folder_id` is 'home' so
// the existing assertions (listFolder('home'), moveFile('f1', 'home'))
// stay valid without test churn.
const { ui, drives, driveIcon } = vi.hoisted(() => {
	const homeDrive = {
		id: 'drive-home',
		root_folder_id: 'home',
		name: 'Personal',
		kind: 'personal' as const,
		default_for_user: 'user-1',
		caller_role: 'owner' as const,
		used_bytes: 0,
		quota_bytes: null
	};
	return {
		ui: { notify: vi.fn() },
		drives: {
			drives: [homeDrive],
			loaded: true,
			load: vi.fn(async () => [homeDrive]),
			findDefault: vi.fn(() => homeDrive),
			findById: vi.fn((id: string) => (id === homeDrive.id ? homeDrive : null)),
			findByRootFolderId: vi.fn((id: string) =>
				id === homeDrive.root_folder_id ? homeDrive : null
			)
		},
		driveIcon: vi.fn(() => 'home')
	};
});
vi.mock('$lib/stores/drives.svelte', () => ({ drives, driveIcon }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/utils/errors', () => ({ errorToast: vi.fn() }));
vi.mock('$lib/api/endpoints/folders', () => ({ listFolder: vi.fn(), moveFolder: vi.fn() }));
vi.mock('$lib/api/endpoints/files', () => ({ moveFile: vi.fn() }));
vi.mock('$lib/api/endpoints/batch', () => ({ copyFiles: vi.fn(), copyFolders: vi.fn() }));

import { listFolder, moveFolder } from '$lib/api/endpoints/folders';
import { moveFile } from '$lib/api/endpoints/files';
import { copyFiles } from '$lib/api/endpoints/batch';
import MoveDialog from './MoveDialog.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;
const item = { id: 'f1', name: 'doc.txt', kind: 'file' as const };

function folder(id: string, name: string) {
	return {
		category: 'Folder',
		created_at: 0,
		icon_class: 'fa-folder',
		id,
		is_root: false,
		modified_at: 0,
		name,
		created_by: 'me',
		updated_by: 'me',
		parent_id: 'home',
		path: '/' + name,
		etag: 'e'
	};
}

beforeEach(() => {
	vi.clearAllMocks();
	m(listFolder).mockResolvedValue({
		folders: [folder('sub1', 'Sub')],
		files: []
	});
});

it('loads the home folder when opened', async () => {
	render(MoveDialog, { props: { open: true, item } });
	await waitFor(() => expect(listFolder).toHaveBeenCalledWith('home'));
	await screen.findByTestId('move-dialog');
});

it('moves the item into the current folder on confirm', async () => {
	m(moveFile).mockResolvedValue(undefined);
	const onmoved = vi.fn();
	render(MoveDialog, { props: { open: true, item, onmoved } });
	await screen.findByTestId('move-dialog-confirm-btn');
	await fireEvent.click(screen.getByTestId('move-dialog-confirm-btn'));
	await waitFor(() => expect(moveFile).toHaveBeenCalledWith('f1', 'home'));
	await waitFor(() => expect(onmoved).toHaveBeenCalled());
});

it('navigates into a subfolder before confirming', async () => {
	m(moveFolder).mockResolvedValue(undefined);
	const folderItem = { id: 'fold9', name: 'Dir', kind: 'folder' as const };
	render(MoveDialog, { props: { open: true, item: folderItem } });
	await fireEvent.click(await screen.findByTestId('move-dialog-folder-sub1'));
	await waitFor(() => expect(listFolder).toHaveBeenCalledWith('sub1'));
	await fireEvent.click(screen.getByTestId('move-dialog-confirm-btn'));
	await waitFor(() => expect(moveFolder).toHaveBeenCalledWith('fold9', 'sub1'));
});

it('copies the item in copy mode', async () => {
	m(copyFiles).mockResolvedValue(undefined);
	render(MoveDialog, { props: { open: true, item, mode: 'copy' } });
	await screen.findByTestId('move-dialog-confirm-btn');
	await fireEvent.click(screen.getByTestId('move-dialog-confirm-btn'));
	await waitFor(() => expect(copyFiles).toHaveBeenCalledWith(['f1'], 'home'));
});
