enum RtEventKind {
	FILE_CREATED = 'file_created',
	FILE_RENAMED = 'file_renamed',
	FILE_MOVED = 'file_moved',
	FILE_DELETED = 'file_deleted',
	FOLDER_CREATED = 'folder_created',
	FOLDER_RENAMED = 'folder_renamed',
	FOLDER_MOVED = 'folder_moved',
	FOLDER_DELETED = 'folder_deleted'
}
export type { RtEventKind as default };
