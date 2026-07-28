# Internal Architecture

OxiCloud follows a **hexagonal (ports & adapters) architecture** with four layers:

```
┌───────────────────────────────────────────────────────────────┐
│  Interfaces    │ REST API, WebDAV, CalDAV, CardDAV, WOPI      │
├───────────────────────────────────────────────────────────────┤
│  Application   │ Use cases, DTOs, port definitions            │
├───────────────────────────────────────────────────────────────┤
│  Domain        │ Entities, business rules, repository traits  │
├───────────────────────────────────────────────────────────────┤
│  Infrastructure│ PostgreSQL, filesystem, caching, auth        │
└───────────────────────────────────────────────────────────────┘
```

All cross-layer dependencies point **inward** via trait-based ports. The DI container (`AppServiceFactory`) wires concrete implementations at startup.

## Storage Model: 100% Blob Storage

- **File metadata** (name, folder, size, user, timestamps, trash status) → PostgreSQL (`storage.files`)
- **File content** → content-addressed blobs via DedupService at `.blobs/{prefix}/{hash}.blob`
- **Folder structure** → purely virtual, rows in `storage.folders` (no filesystem directories per user)
- **Trash** → soft-delete flags on files/folders, exposed via `storage.trash_items` VIEW

## Dependency Injection

`AppServiceFactory` in `src/common/di.rs` builds all services in a defined order:

1. **Core services** — paths, content cache, thumbnails, chunked upload, transcode, dedup, compression
2. **Repositories** — `FolderDbRepository`, `FileBlobReadRepository`, `FileBlobWriteRepository`, `TrashDbRepository`
3. **Trash service** (if enabled)
4. **Application services** — folder, file upload/retrieval/management, search, i18n
5. **Share service** (if enabled)
6. **DB services** — favorites, recent, storage usage, auth
7. **CalDAV/CardDAV services**
8. **ZIP service** (last, depends on file & folder services)
9. **Assemble `AppState`**

## AppState Shape

The assembled `AppState` groups the application into a few stable buckets:

- `core` for cross-cutting runtime services such as path resolution, caching, chunked uploads, deduplication, compression, thumbnails, and ZIP handling
- `repositories` for PostgreSQL-backed folder, file, trash, and i18n persistence
- `applications` for the use-case layer exposed to handlers
- optional auth, admin, trash, share, favorites, recent, storage usage, calendar, and contact services when those features are enabled

This lets handlers depend on stable interfaces while the concrete implementation details stay inside the DI container.

## Project Structure

```
src/
├── common/          # Config, DI container, errors
├── domain/          # Entities, repository traits
├── application/     # Use cases, DTOs, port traits
├── infrastructure/  # PostgreSQL repos, filesystem, caching
└── interfaces/      # HTTP handlers, WebDAV, CalDAV, CardDAV
```

## Key Metrics

| Metric | Value |
|--------|-------|
| Rust source files | ~170 |
| Lines of code | ~50 000 |
| Automated tests | 222+ |
| Docker image | ~40 MB |

## Further Reading

- [ReBAC Authorization →](/architecture/rebac-authorization)
- [Caching Architecture →](/architecture/caching)
- [Resource Listing API →](/architecture/resource-listing)
- [Storage Quotas →](/architecture/storage-quotas)
- [Background Jobs →](/architecture/jobs)
