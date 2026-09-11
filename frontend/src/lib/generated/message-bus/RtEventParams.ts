import type FileCreatedData from './FileCreatedData';
import type FileRenamedData from './FileRenamedData';
import type FileMovedData from './FileMovedData';
import type FileDeletedData from './FileDeletedData';
import type FolderCreatedData from './FolderCreatedData';
import type FolderRenamedData from './FolderRenamedData';
import type FolderMovedData from './FolderMovedData';
import type FolderDeletedData from './FolderDeletedData';
import type NotificationReceivedData from './NotificationReceivedData';
import type JobRunStartedData from './JobRunStartedData';
import type JobRunProgressData from './JobRunProgressData';
import type JobRunEndedData from './JobRunEndedData';
import type RtEventKind from './RtEventKind';
// AUTO-GENERATED — do not edit by hand.
// Regenerate with `just asyncapi-ts`.
interface RtEventParams {
	data:
		| FileCreatedData
		| FileRenamedData
		| FileMovedData
		| FileDeletedData
		| FolderCreatedData
		| FolderRenamedData
		| FolderMovedData
		| FolderDeletedData
		| NotificationReceivedData
		| JobRunStartedData
		| JobRunProgressData
		| JobRunEndedData;
	event: RtEventKind;
	topic: string;
}
export type { RtEventParams as default };
