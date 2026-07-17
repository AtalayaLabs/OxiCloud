pub mod app_password;
pub mod calendar;
pub mod calendar_event;
pub mod contact;
pub mod device_code;
pub mod drive;
pub mod entity_errors;
pub mod face;
pub mod file;
pub mod folder;
pub mod magic_link_token;
pub mod playlist;
pub mod session;
pub mod share;
pub mod subject_group;
pub mod sync_token;
pub mod trashed_item;
pub mod user;

// Re-exportar errores de entidades para facilitar el uso
pub use entity_errors::{
    CalendarError, CalendarEventError, CalendarEventResult, CalendarResult, FileError, FileResult,
    FolderError, FolderResult, ShareError, ShareResult, SubjectGroupError, SubjectGroupResult,
    UserError, UserResult,
};
