
enum RtEventKind {
  FILE_CREATED = "file_created",
  FILE_RENAMED = "file_renamed",
  FILE_MOVED = "file_moved",
  FILE_DELETED = "file_deleted",
  FOLDER_CREATED = "folder_created",
  FOLDER_RENAMED = "folder_renamed",
  FOLDER_MOVED = "folder_moved",
  FOLDER_DELETED = "folder_deleted",
  NOTIFICATION_RECEIVED = "notification_received",
  JOB_RUN_STARTED = "job_run_started",
  JOB_RUN_PROGRESS = "job_run_progress",
  JOB_RUN_ENDED = "job_run_ended",
  SERVER_STATUS_CHANGED = "server_status_changed",
}
export type { RtEventKind as default };
