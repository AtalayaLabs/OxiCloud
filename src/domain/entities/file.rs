use uuid::Uuid;

use crate::domain::services::path_service::{
    StoragePath, normalize_storage_name_owned, validate_storage_name,
};

// Re-export entity errors from the centralized module
pub use super::entity_errors::{FileError, FileResult};

/// Owned parts of a [`File`] entity, produced by [`File::into_parts()`].
///
/// Consuming a `File` into `FileParts` **moves** every field without cloning,
/// eliminating 3-5 heap allocations that previously occurred when converting
/// `File → FileDto` via `.to_string()` on each getter.
pub struct FileParts {
    pub id: String,
    pub name: String,
    pub storage_path: StoragePath,
    pub size: u64,
    pub mime_type: String,
    pub folder_id: Option<String>,
    pub created_at: u64,
    pub modified_at: u64,
    /// BLAKE3 content hash. See [`File::content_hash`] for semantics.
    pub blob_hash: String,
    /// §14 provenance: original creator. See [`File::created_by`].
    pub created_by: Option<Uuid>,
    /// §14 provenance: most recent mutator. See [`File::updated_by`].
    pub updated_by: Option<Uuid>,
}

/**
 * Represents a file in the system's domain model.
 *
 * The File entity is a core domain object that encapsulates all properties and behaviors
 * of a file in the system. It implements an immutable design pattern where modification
 * operations return new instances rather than modifying the existing one.
 *
 * This entity maintains both physical storage information and logical metadata about files,
 * serving as the bridge between the storage system and the application.
 */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Unique identifier for the file - used throughout the system for file operations
    id: String,

    /// Name of the file including extension
    name: String,

    /// Path to the file in the domain model. Owns the canonical joined
    /// string; `path_string()` borrows it (the separate duplicate field
    /// was removed in ROUND11 §20 along with per-segment allocations).
    storage_path: StoragePath,

    /// Size of the file in bytes
    size: u64,

    /// MIME type of the file (e.g., "text/plain", "image/jpeg")
    mime_type: String,

    /// Parent folder ID if the file is within a folder, None if in root
    folder_id: Option<String>,

    /// Creation timestamp (seconds since UNIX epoch)
    created_at: u64,

    /// Last modification timestamp (seconds since UNIX epoch)
    modified_at: u64,

    /// BLAKE3 content hash. Stable across renames/moves, changes only
    /// when the file's content bytes change. Source of truth for both
    /// content-addressable storage and the HTTP ETag (via
    /// [`File::etag`]). Exposed publicly via [`File::content_hash`]
    /// so the REST API can surface it as a distinct concept from the
    /// ETag (the ETag formula may grow to include `modified_at` etc.,
    /// but `content_hash` remains the raw hash).
    blob_hash: String,

    /// User that originally created this file (§14 provenance).
    /// Stamped at INSERT and never updated thereafter. `None` when
    /// the referenced user has been deleted (FK is `ON DELETE SET
    /// NULL`) or for stub/DTO-reconstructed files.
    created_by: Option<Uuid>,

    /// User that performed the most recent mutation that bumped
    /// `updated_at` (rename, move, content overwrite, trash, restore).
    /// Authorship signal — distinct from ownership. `None` when the
    /// referenced user is deleted or for stub/DTO-reconstructed files.
    updated_by: Option<Uuid>,
}

// We no longer need this module, now we use a String directly

impl Default for File {
    fn default() -> Self {
        Self {
            id: "stub-id".to_string(),
            name: "stub-file.txt".to_string(),
            storage_path: StoragePath::from_string("/"),
            size: 0,
            mime_type: "application/octet-stream".to_string(),
            folder_id: None,
            created_at: 0,
            modified_at: 0,
            blob_hash: String::new(),
            created_by: None,
            updated_by: None,
        }
    }
}

impl File {
    /// Creates a new file with validation
    pub fn new(
        id: String,
        name: String,
        storage_path: StoragePath,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
    ) -> FileResult<Self> {
        let name = normalize_storage_name_owned(name);
        if let Err(reason) = validate_storage_name(&name) {
            return Err(FileError::InvalidFileName(format!("{name}: {reason}")));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(Self {
            id,
            name,
            storage_path,
            size,
            mime_type,
            folder_id,
            created_at: now,
            modified_at: now,
            blob_hash: String::new(),
            created_by: None,
            updated_by: None,
        })
    }

    /// Creates a folder entity
    pub fn new_folder(
        id: String,
        name: String,
        storage_path: StoragePath,
        parent_id: Option<String>,
        created_at: u64,
        modified_at: u64,
    ) -> FileResult<Self> {
        let name = normalize_storage_name_owned(name);
        if let Err(reason) = validate_storage_name(&name) {
            return Err(FileError::InvalidFileName(format!("{name}: {reason}")));
        }

        Ok(Self {
            id,
            name,
            storage_path,
            size: 0,                            // Folders have zero size
            mime_type: "directory".to_string(), // Standard MIME type for directories
            folder_id: parent_id,
            created_at,
            modified_at,
            blob_hash: String::new(),
            created_by: None,
            updated_by: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_timestamps(
        id: String,
        name: String,
        storage_path: StoragePath,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
        created_at: u64,
        modified_at: u64,
    ) -> FileResult<Self> {
        Self::with_timestamps_and_blob_hash(
            id,
            name,
            storage_path,
            size,
            mime_type,
            folder_id,
            created_at,
            modified_at,
            String::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_timestamps_and_blob_hash(
        id: String,
        name: String,
        storage_path: StoragePath,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
        created_at: u64,
        modified_at: u64,
        blob_hash: String,
    ) -> FileResult<Self> {
        Self::with_timestamps_blob_hash_and_provenance(
            id,
            name,
            storage_path,
            size,
            mime_type,
            folder_id,
            created_at,
            modified_at,
            blob_hash,
            None,
            None,
        )
    }

    /// Full constructor including the §14 provenance columns
    /// (`created_by` / `updated_by`). PG-row callers use this to
    /// preserve authorship across reconstruction.
    #[allow(clippy::too_many_arguments)]
    pub fn with_timestamps_blob_hash_and_provenance(
        id: String,
        name: String,
        storage_path: StoragePath,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
        created_at: u64,
        modified_at: u64,
        blob_hash: String,
        created_by: Option<Uuid>,
        updated_by: Option<Uuid>,
    ) -> FileResult<Self> {
        let name = normalize_storage_name_owned(name);
        if let Err(reason) = validate_storage_name(&name) {
            return Err(FileError::InvalidFileName(format!("{name}: {reason}")));
        }

        Ok(Self {
            id,
            name,
            storage_path,
            size,
            mime_type,
            folder_id,
            created_at,
            modified_at,
            blob_hash,
            created_by,
            updated_by,
        })
    }

    /// PG-row constructor: the per-listing-row hot path.
    ///
    /// Builds `storage_path` **and** `path_string` in one pass from the
    /// materialized folder path via
    /// [`StoragePath::from_folder_and_name`], instead of the old chain
    /// (`format!` temp → `from_string` split → `Display` re-join) that
    /// allocated the full path three times per row. The owned `name` is
    /// NFC-normalized without the always-copy of the borrowing variant
    /// (DB rows are NFC by invariant, so this is a zero-alloc check).
    ///
    /// The path is built from the raw incoming name and the name field is
    /// normalized afterwards — the exact observable sequence of the old
    /// `make_file_path` + constructor pair, byte-identical for every
    /// input (for DB rows the two names coincide: stored names are NFC).
    #[allow(clippy::too_many_arguments)]
    pub fn from_materialized_row(
        id: String,
        name: String,
        folder_path: Option<&str>,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
        created_at: u64,
        modified_at: u64,
        blob_hash: String,
        created_by: Option<Uuid>,
        updated_by: Option<Uuid>,
    ) -> FileResult<Self> {
        let storage_path = StoragePath::from_folder_and_name(folder_path, &name);

        let name = normalize_storage_name_owned(name);
        if let Err(reason) = validate_storage_name(&name) {
            return Err(FileError::InvalidFileName(format!("{name}: {reason}")));
        }

        Ok(Self {
            id,
            name,
            storage_path,
            size,
            mime_type,
            folder_id,
            created_at,
            modified_at,
            blob_hash,
            created_by,
            updated_by,
        })
    }

    /// Consume the entity and return all fields by ownership.
    ///
    /// Use this when converting `File` into a DTO to avoid cloning
    /// every `String` field (saves 3-5 heap allocations per file).
    pub fn into_parts(self) -> FileParts {
        FileParts {
            id: self.id,
            name: self.name,
            storage_path: self.storage_path,
            size: self.size,
            mime_type: self.mime_type,
            folder_id: self.folder_id,
            created_at: self.created_at,
            modified_at: self.modified_at,
            blob_hash: self.blob_hash,
            created_by: self.created_by,
            updated_by: self.updated_by,
        }
    }

    /// Raw BLAKE3 content hash — the cryptographic identity of the
    /// file's bytes. Stable across renames, moves, and metadata
    /// updates. Changes only when the underlying content changes.
    ///
    /// This is **distinct from [`File::etag`]**: the ETag is an HTTP
    /// cache token that may incorporate non-content signals (mtime,
    /// permissions, …) in future revisions; `content_hash` is the
    /// raw hash, suitable for content-addressable URLs, dedup
    /// verification, and integrity audits. Keep both accessible —
    /// the API layer can choose to expose `content_hash` even when
    /// `etag` grows additional inputs.
    pub fn content_hash(&self) -> &str {
        &self.blob_hash
    }

    /// Opaque HTTP ETag string (raw, NOT HTTP-quoted). Handlers wrap
    /// in `"…"` themselves at the HTTP boundary.
    ///
    /// This is a thin instance-method wrapper around
    /// [`File::compute_etag`] — see that function for the full
    /// formula, rationale, and the "single source of truth"
    /// guarantee that lets raw-row listings (`/api/folders/{id}/resources`,
    /// favorites, trash, recents, REPORT/SEARCH) compute the same
    /// value without constructing a full `File` entity.
    pub fn etag(&self) -> String {
        Self::compute_etag(&self.blob_hash, self.modified_at)
    }

    /// Pure formula for the file ETag, exposed as a static method so
    /// listing handlers that operate on raw SQL rows (rather than
    /// fully-constructed `File` entities) route through the same
    /// definition.
    ///
    /// **Formula**: `{blob_hash[..16]}-{modified_at}`.
    ///
    /// - The 16-char BLAKE3 prefix is the content identity (64 bits
    ///   ≈ 10⁻⁹ collision probability over 10M files).
    /// - `modified_at` (Unix seconds) catches the `x-oc-mtime`
    ///   case: NextCloud preserves the client-side mtime on upload,
    ///   so a "touch-then-resync" of unchanged content still bumps
    ///   the mtime — without the suffix the ETag wouldn't change
    ///   and clients would serve stale metadata.
    /// - When `blob_hash` is shorter than 16 chars (test fixtures,
    ///   stub entities) the prefix is just the whole value.
    /// - Folder ETags follow a separate formula — see
    ///   [`crate::domain::entities::folder::Folder::compute_etag`].
    ///
    /// Every handler that emits a file ETag header MUST go through
    /// this function (directly or via [`File::etag`] /
    /// `FileDto::etag`) so `GET`, `HEAD`, `PROPFIND`, `PUT`
    /// response, `MOVE`, and every JSON listing return
    /// byte-identical values for the same file. Changing the
    /// formula here changes it everywhere — that is the property
    /// we want.
    pub fn compute_etag(blob_hash: &str, modified_at: u64) -> String {
        use std::fmt::Write as _;

        // Byte index just past the 16th char (whole string when shorter).
        // `blob_hash` is lowercase hex ASCII in practice, so this is
        // effectively `min(len, 16)`, but `char_indices` keeps the slice
        // char-boundary-safe for exotic fixture values — byte-identical
        // to the old `chars().take(16).collect::<String>()` without the
        // intermediate allocation.
        let end = match blob_hash.char_indices().nth(16) {
            Some((i, _)) => i,
            None => blob_hash.len(),
        };

        // Single allocation: prefix + '-' + up to 20 digits (u64::MAX).
        let mut etag = String::with_capacity(end + 1 + 20);
        etag.push_str(&blob_hash[..end]);
        etag.push('-');
        let _ = write!(etag, "{modified_at}");
        etag
    }

    /// Wire-side ETag builder: accepts the `DateTime<Utc>` values that
    /// listing SQL rows carry, routes `modified_at` through the shared
    /// `datetime_to_etag_seconds` rounding (matches
    /// `EXTRACT(EPOCH FROM x)::bigint` on the File-entity path), and
    /// returns the empty string on folder rows (`blob_hash` is `None`
    /// in the UNION ALL).
    ///
    /// Prefer this over inlining the three-line
    /// `let secs = …; File::compute_etag(hash, secs)` dance at every
    /// listing site — one callsite, one rounding rule, no drift between
    /// endpoints. See the Cyberduck regression captured in
    /// `tests/api/webdav_etag_cross_check.hurl` for what inlining
    /// cost us.
    pub fn etag_for_listing(
        blob_hash: Option<&str>,
        modified_at: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let Some(hash) = blob_hash.filter(|h| !h.is_empty()) else {
            return String::new();
        };
        Self::compute_etag(
            hash,
            crate::application::dtos::timestamp_helpers::datetime_to_etag_seconds(modified_at),
        )
    }

    // Getters
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn storage_path(&self) -> &StoragePath {
        &self.storage_path
    }

    pub fn path_string(&self) -> &str {
        self.storage_path.as_str()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn folder_id(&self) -> Option<&str> {
        self.folder_id.as_deref()
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn modified_at(&self) -> u64 {
        self.modified_at
    }

    /// User that originally created this file (§14 provenance).
    /// `None` when the referenced user has been deleted
    /// (FK is `ON DELETE SET NULL`) or for stub/DTO entities.
    pub fn created_by(&self) -> Option<Uuid> {
        self.created_by
    }

    /// User that performed the most recent mutation that bumped
    /// `updated_at`. Authorship signal — distinct from ownership.
    /// `None` when the referenced user is deleted or for
    /// stub/DTO entities.
    pub fn updated_by(&self) -> Option<Uuid> {
        self.updated_by
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_dto(
        id: String,
        name: String,
        path: String,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
        created_at: u64,
        modified_at: u64,
    ) -> Self {
        // Adopt the DTO path (canonical inputs are reused with zero
        // copies; non-canonical ones are normalized like from_string did).
        let storage_path = StoragePath::from_joined(path);

        // Create directly without validation to avoid errors in DTO
        // conversions. Still NFC-normalize so even DTO-reconstructed
        // entities maintain the storage invariant.
        let name = normalize_storage_name_owned(name);

        Self {
            id,
            name,
            storage_path,
            size,
            mime_type,
            folder_id,
            created_at,
            modified_at,
            blob_hash: String::new(),
            // DTO round-trips don't carry provenance; callers needing
            // it must reload from the repository.
            created_by: None,
            updated_by: None,
        }
    }

    // Methods to create new versions of the file (immutable)

    /// Creates a new version of the file with updated name
    pub fn with_name(mut self, new_name: String) -> FileResult<Self> {
        let new_name = normalize_storage_name_owned(new_name);
        if let Err(reason) = validate_storage_name(&new_name) {
            return Err(FileError::InvalidFileName(format!("{new_name}: {reason}")));
        }

        // Recompute the path from the unchanged parent + the new name.
        let new_storage_path = match self.storage_path.parent() {
            Some(parent) => parent.join(&new_name),
            None => StoragePath::from_string(&new_name),
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Consume `self` and mutate in place — only the path, name and mtime
        // change; id / mime_type / folder_id / blob_hash are carried over
        // without the per-field clone the old `&self` builder paid.
        self.storage_path = new_storage_path;
        self.name = new_name;
        self.modified_at = now;
        Ok(self)
    }

    /// Creates a new version of the file with updated folder
    pub fn with_folder(
        mut self,
        folder_id: Option<String>,
        folder_path: Option<StoragePath>,
    ) -> FileResult<Self> {
        // We need a folder path to update the file path
        let new_storage_path = match folder_path {
            Some(path) => path.join(&self.name),
            None => StoragePath::from_string(&self.name), // Root
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Consume `self`: only the path, folder_id and mtime change.
        self.storage_path = new_storage_path;
        self.folder_id = folder_id;
        self.modified_at = now;
        Ok(self)
    }

    /// Creates a new version of the file with updated size
    pub fn with_size(mut self, new_size: u64) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Consume `self`: only size and mtime change — no per-field clone.
        self.size = new_size;
        self.modified_at = now;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_creation_with_valid_name() {
        let storage_path = StoragePath::from_string("/test/file.txt");
        let file = File::new(
            "123".to_string(),
            "file.txt".to_string(),
            storage_path,
            100,
            "text/plain".to_string(),
            None,
        );

        assert!(file.is_ok());
    }

    #[test]
    fn test_file_creation_with_invalid_name() {
        let storage_path = StoragePath::from_string("/test/invalid/file.txt");
        let file = File::new(
            "123".to_string(),
            "file/with/slash.txt".to_string(), // Invalid name
            storage_path,
            100,
            "text/plain".to_string(),
            None,
        );

        assert!(file.is_err());
        match file {
            Err(FileError::InvalidFileName(_)) => (),
            _ => panic!("Expected InvalidFileName error"),
        }
    }

    #[test]
    fn test_file_with_name() {
        let storage_path = StoragePath::from_string("/test/file.txt");
        let file = File::new(
            "123".to_string(),
            "file.txt".to_string(),
            storage_path,
            100,
            "text/plain".to_string(),
            None,
        )
        .unwrap();

        let renamed = file.with_name("newname.txt".to_string());
        assert!(renamed.is_ok());
        let renamed = renamed.unwrap();
        assert_eq!(renamed.name(), "newname.txt");
        assert_eq!(renamed.id(), "123"); // The ID does not change
    }

    /// The ETag formula is `{blob_hash[..16]}-{modified_at}`. Two
    /// fixtures with identical content + mtime must produce
    /// byte-identical ETags — that's the invariant every handler
    /// relies on when comparing a cached client ETag against a
    /// freshly-loaded one.
    #[test]
    fn test_etag_combines_blob_hash_prefix_and_mtime() {
        let file = File::with_timestamps_and_blob_hash(
            "id-1".to_string(),
            "file.txt".to_string(),
            StoragePath::from_string("/file.txt"),
            42,
            "text/plain".to_string(),
            None,
            1_000,
            2_000,
            "abcdef0123456789ZZZZZZZZ".to_string(),
        )
        .unwrap();

        // content_hash stays raw — full blob hash, no truncation.
        assert_eq!(file.content_hash(), "abcdef0123456789ZZZZZZZZ");
        // etag is the 16-char prefix + "-" + mtime.
        assert_eq!(file.etag(), "abcdef0123456789-2000");
    }

    /// When the blob hash is shorter than 16 chars (test fixtures,
    /// stub entities), the prefix degrades to "whatever is there".
    /// Production blob hashes are always full BLAKE3 hex (64 chars).
    #[test]
    fn test_etag_short_blob_hash_uses_full_value() {
        let file = File::with_timestamps_and_blob_hash(
            "id-1".to_string(),
            "file.txt".to_string(),
            StoragePath::from_string("/file.txt"),
            42,
            "text/plain".to_string(),
            None,
            1_000,
            2_000,
            "shorthash".to_string(),
        )
        .unwrap();

        assert_eq!(file.etag(), "shorthash-2000");
    }

    /// `content_hash` is the cryptographic identity of the bytes —
    /// it must NEVER change because of metadata operations like
    /// rename. The ETag is allowed to change (because `with_name`
    /// bumps `modified_at`), but the content hash is not.
    #[test]
    fn test_content_hash_stable_across_rename() {
        let file = File::with_timestamps_and_blob_hash(
            "id-1".to_string(),
            "file.txt".to_string(),
            StoragePath::from_string("/file.txt"),
            42,
            "text/plain".to_string(),
            None,
            1_000,
            2_000,
            "stable-content-hash".to_string(),
        )
        .unwrap();

        let renamed = file.with_name("renamed.txt".to_string()).unwrap();
        assert_eq!(renamed.content_hash(), "stable-content-hash");
    }
}
