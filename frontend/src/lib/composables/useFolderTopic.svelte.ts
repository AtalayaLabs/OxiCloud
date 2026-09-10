// Folder-view sugar around `useTopic`.
//
// Discriminates the `rt.event` union at the composable boundary so
// each consumer supplies per-verb handlers with correctly-typed
// payloads. Adding a new event kind in Rust regenerates
// `RtEventKind` — the switch below fails to type-check until every
// arm is handled, keeping the FE exhaustive.

import { useTopic } from './useTopic.svelte';
import type RtEventParams from '$lib/generated/message-bus/RtEventParams';
import type RtRevokedParams from '$lib/generated/message-bus/RtRevokedParams';
import type FileCreatedData from '$lib/generated/message-bus/FileCreatedData';
import type FileRenamedData from '$lib/generated/message-bus/FileRenamedData';
import type FileMovedData from '$lib/generated/message-bus/FileMovedData';
import type FileDeletedData from '$lib/generated/message-bus/FileDeletedData';
import type FolderCreatedData from '$lib/generated/message-bus/FolderCreatedData';
import type FolderRenamedData from '$lib/generated/message-bus/FolderRenamedData';
import type FolderMovedData from '$lib/generated/message-bus/FolderMovedData';
import type FolderDeletedData from '$lib/generated/message-bus/FolderDeletedData';

/**
 * Optional per-verb handlers. Any subset is accepted; unhandled verbs
 * are silently ignored. Fires only when the folder view actually cares
 * about that kind — leave a handler undefined to opt out.
 *
 * Callers commonly bind ONE `refresh` function to every handler (see
 * `routes/files/[...path]/+page.svelte`) rather than reason about
 * surgical mutations — that keeps the folder listing consistent
 * with server-side sort/pagination without maintaining a second
 * mutation path.
 */
export interface FolderTopicHandlers {
	onFileCreated?: (data: FileCreatedData) => void;
	onFileRenamed?: (data: FileRenamedData) => void;
	onFileMoved?: (data: FileMovedData) => void;
	onFileDeleted?: (data: FileDeletedData) => void;
	onFolderCreated?: (data: FolderCreatedData) => void;
	onFolderRenamed?: (data: FolderRenamedData) => void;
	onFolderMoved?: (data: FolderMovedData) => void;
	onFolderDeleted?: (data: FolderDeletedData) => void;
	/** Grant revoked or folder deleted — the subscription is gone
	 *  server-side. Reasonable UX: toast + navigate away. */
	onRevoked?: (params: RtRevokedParams) => void;
}

/**
 * Subscribe to `folder:{folderId}` and dispatch each `rt.event`
 * notification to the matching per-verb handler.
 *
 * `folderId` accepts the same shapes as `useTopic`'s `topic` — a
 * plain string, a nullable string (null = don't subscribe yet), or a
 * getter that reads from reactive state (route param) so the
 * subscription follows the current folder.
 */
export function useFolderTopic(
	folderId: string | null | (() => string | null),
	handlers: FolderTopicHandlers
): void {
	const topic = () => {
		const id = typeof folderId === 'function' ? folderId() : folderId;
		return id ? `folder:${id}` : null;
	};
	useTopic(topic, (params) => dispatch(params, handlers), handlers.onRevoked);
}

function dispatch(params: RtEventParams, handlers: FolderTopicHandlers): void {
	// The generated `RtEventKind` string-enum values match the Rust
	// `#[serde(rename_all = "snake_case")]` variants exactly — see
	// `application/ports/message_bus_ports.rs::MessageBusEvent`.
	switch (params.event) {
		case 'file_created':
			handlers.onFileCreated?.(params.data as FileCreatedData);
			return;
		case 'file_renamed':
			handlers.onFileRenamed?.(params.data as FileRenamedData);
			return;
		case 'file_moved':
			handlers.onFileMoved?.(params.data as FileMovedData);
			return;
		case 'file_deleted':
			handlers.onFileDeleted?.(params.data as FileDeletedData);
			return;
		case 'folder_created':
			handlers.onFolderCreated?.(params.data as FolderCreatedData);
			return;
		case 'folder_renamed':
			handlers.onFolderRenamed?.(params.data as FolderRenamedData);
			return;
		case 'folder_moved':
			handlers.onFolderMoved?.(params.data as FolderMovedData);
			return;
		case 'folder_deleted':
			handlers.onFolderDeleted?.(params.data as FolderDeletedData);
			return;
	}
}
