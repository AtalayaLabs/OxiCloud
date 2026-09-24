# OxiCloud TODO List

This document contains the task list for the development of OxiCloud, a minimalist and efficient cloud storage system similar to NextCloud but optimized for performance.

## Phase 1: Basic File Functionalities

### Folder System
- [x] Implement API for creating folders
- [x] Add support for hierarchical paths in the backend
- [ ] Update UI to show folder structure (tree)
- [x] Implement navigation between folders
- [x] Add functionality to rename folders
- [x] Add option to move files between folders

### File Preview
- [x] Implement integrated image viewer
- [x] Add basic PDF viewer
- [x] Generate thumbnails for images
- [x] Implement specific icons by file type
- [x] Add text/code preview

### Enhanced Search
- [x] Implement search by name
- [x] Add filters by file type
- [x] Implement search by date range
- [x] Add filter by file size
- [x] Add search within specific folders
- [x] Implement cache for search results
- [x] Full-text search over file contents — Tantivy-backed content index, text extraction per blob (BLAKE3-keyed cache), asynchronous drain worker so writes never wait on indexing.

### UI/UX Optimizations
- [ ] Improve responsive design for mobile devices
- [x] Implement drag & drop between folders
- [x] Add support for multiple file selection
- [x] Implement multiple file uploads
- [x] Add progress indicators for long operations
- [x] Implement UI notifications for events
- [x] Photos timeline: virtual scrolling — only visible rows in the DOM, constant memory at any library size.

## Phase 2: Authentication and Multi-User

### User System
- [x] Design data model for users
- [x] Implement user registration
- [x] Create login system
- [x] Add user profile page
- [x] Implement password recovery — admin-initiated reset from the admin panel (user-initiated self-service flow is a follow-up).
- [x] Separate storage by user

### Quotas and Permissions
- [x] Implement storage quota system
- [x] Add basic role system (admin/user)
- [x] Create admin panel
- [x] Implement folder-level permissions
- [x] Add storage usage monitoring

### Basic Security
- [x] Implement secure password hashing with Argon2
- [x] Add session management
- [x] Implement JWT authentication token
- [x] Add CSRF protection
- [x] Implement login attempt limits
- [ ] Create activity logging system

## Phase 3: Collaboration Features

### File Sharing
- [x] Implement shared link generation
- [x] Add permission configuration for links
- [x] Implement password protection for links
- [ ] Add expiration dates for shared links
- [x] Create page to manage all shared resources
- [x] Implement sharing notifications

### Recycle Bin
- [x] Design model for storing deleted files
- [x] Implement soft deletion (move to trash)
- [x] Add functionality to restore files
- [x] Implement automatic purge by time
- [x] Add option to manually empty trash
- [x] Implement storage limits for trash

### Activity Log
- [ ] Create model for activity events
- [ ] Implement logging of CRUD operations
- [ ] Add logging of access and security events
- [ ] Create activity history page
- [ ] Implement filters for activity log
- [ ] Add log export

## Phase 4: API and Synchronization

### Complete REST API
- [x] Design OpenAPI specification
- [x] Implement endpoints for file operations
- [x] Add endpoints for users and authentication
- [x] Implement automatic documentation (Swagger)
- [ ] Create API token system
- [x] Implement rate limiting
- [ ] Add API versioning

### WebDAV Support
- [x] Implement basic WebDAV server
- [x] Add authentication for WebDAV
- [x] Implement PROPFIND operations
- [x] Add support for locking
- [x] Test compatibility with standard clients
- [x] Optimize WebDAV performance
- [x] Implement Range Requests (RFC 7233) for resumable transfers
- [x] Support partial file updates for bandwidth efficiency — small edit in a huge file transmits only the touched chunks (CDC-based delta upload).

### Sync Client
- [ ] Design client architecture in Rust
- [ ] Implement unidirectional synchronization
- [ ] Add bidirectional synchronization
- [ ] Implement conflict detection
- [ ] Add configuration options
- [ ] Create minimal client version for Windows/macOS/Linux
- [ ] Implement bandwidth throttling controls
- [ ] Add delta synchronization for large files
- [ ] Support synchronization pausing and resuming

## Phase 5: Advanced Features

### File Encryption
- [x] Research and select encryption algorithms
- [x] Implement at-rest encryption for files
- [x] Add key management
- [x] Implement encryption for shared files
- [x] Create security documentation

### File Versioning
- [ ] Design version storage system
- [ ] Implement version history
- [ ] Add difference visualization
- [ ] Implement version restoration
- [ ] Add version retention policies

### Basic Applications
- [x] Design plugin/app system
- [x] Implement basic text viewer/editor
- [ ] Add simple notes application
- [x] Implement basic calendar
- [x] Create API for third-party applications

## Continuous Optimizations

### Backend
- [x] Implement file cache with Rust
- [x] Enable Link Time Optimization (LTO) for better performance
- [x] Optimize large file transmission
- [x] Add adaptive compression by file type
- [x] Implement asynchronous processing for heavy tasks
- [x] Optimize database queries
- [ ] Implement scaling strategies
- [x] Implement transfer acceleration with multipart chunking
- [x] Implement differential sync algorithm (similar to rsync)
- [x] Add strong ETag support for more efficient caching
- [x] Recoverable jobs engine — background job runner pauses at a cursor on transient failures and resumes on the next tick instead of losing progress or falsely reporting success.

### Frontend
- [x] Optimize initial asset loading
- [x] Implement lazy loading for large lists
- [x] Add local cache (localStorage/IndexedDB)
- [ ] Optimize UI rendering
- [ ] Implement intelligent prefetching
- [ ] Add basic offline support
- [x] Implement client-side image resizing before upload
- [x] Add HTTP/2 support for multiplexing requests
- [ ] Implement progressive image loading

### Storage
- [x] Research deduplication options
- [x] Implement block storage
- [x] Add transparent compression by file type
- [ ] Implement log rotation and archiving
- [ ] Create automated backup system
- [x] Add support for distributed storage
- [x] Implement media transcoding for optimized delivery
- [x] Add content-aware compression by file format
- [x] Implement dynamic thumbnail resizing based on viewport
- [x] Online migration between storage backends — hot-switch between local FS / S3 / Azure / encrypted with hash verification, resumable.
- [x] Filesystem consistency checks — background jobs sweep blob ref-counts, cross-backend orphans, thumbnail and extracted-text orphans. Discovery-only; repair is opt-in.
- [x] External mounts — S3 / WebDAV / etc. providers appear as folders in the user's tree; reads and writes route through the mount provider transparently.

### Bandwidth & Transfer Optimization
- [ ] **Sub-file chunked dedup (Restic/Borg style)**
  - [x] Implement Content-Defined Chunking (CDC) with FastCDC/Rabin rolling hash
  - [x] Variable-size chunks (target 1-4 MB) instead of whole-file blobs
  - [x] Per-chunk BLAKE3 hashing and dedup (saves storage + bandwidth on similar files)
  - [ ] Chunk-level Zstd compression (better ratio than whole-file)
  - [ ] Migrate existing whole-file blobs to chunked storage
- [x] **Delta sync / rsync-style transfers**
  - [x] Implement rolling checksum algorithm for block-level diffing
  - [x] Client sends only changed blocks on re-upload (not the full file)
  - [x] Server-side block assembly from delta + existing chunks
  - [x] Huge savings for large files with small edits (VMs, databases, ISOs)
- [ ] **Resumable uploads & downloads (RFC 7233 / tus.io)**
  - [x] Server tracks partial upload state; client resumes from last byte on failure
  - [x] HTTP Range responses for download resume after network drops
  - [ ] tus.io protocol support for cross-client compatibility
- [ ] **Client-side optimization before upload**
  - [x] Resize images to configurable max dimensions before upload (e.g. 4K cap)
  - [ ] Re-encode videos to efficient codec (H.265/AV1) client-side before upload
  - [x] ⚡ **HIGH IMPACT / QUICK WIN** — Pre-compute BLAKE3 hash client-side (WASM); query server before upload; skip transfer entirely if blob already exists (instant dedup, zero bandwidth)
- [ ] **Server-side on-demand transcoding**
  - [ ] Store originals; serve WebP/AVIF for images on request (saves download BW)
  - [ ] Adaptive video streaming (HLS/DASH) from stored originals
  - [ ] Lazy generation + cache of transcoded variants
- [ ] **Smart sync (placeholder/on-demand files)**
  - [ ] Sync client downloads metadata only; fetch file content on first open
  - [ ] Pin/unpin files for offline availability
  - [ ] Automatic eviction of least-recently-used local copies
- [ ] **Transfer-level compression**
  - [ ] Zstd streaming compression for HTTP responses (better than gzip for large files)
  - [ ] Brotli for static assets; Zstd for dynamic/binary content
  - [x] Content-aware: skip compression for already-compressed formats (JPEG, ZIP, etc.)
- [ ] **Batched & multiplexed operations**
  - [ ] Batch small file uploads into single request (tar-stream or multipart bundle)
  - [x] HTTP/2 multiplexing for parallel chunk transfers on single connection
  - [x] Server-side ZIP streaming for multi-file download (already partial)

## Infrastructure and Deployment

- [x] Create Docker configuration
- [x] Implement CI/CD with GitHub Actions
- [x] Add automated tests
- [x] Create installation documentation
- [ ] Implement monitoring and alerts
- [ ] Add automatic update system

## New Comprehensive Roadmap

### Advanced File Management
- [x] Implement optimized file upload/download
  - [x] Implement chunked upload for large files
  - [x] Add file integrity verification
  - [x] Develop adaptive compression by file type
- [x] Implement preview for different file types
  - [x] Create integrated PDF viewer
  - [x] Add office document viewer
  - [x] Develop code viewer with syntax highlighting
- [ ] Add online document editing
  - [x] Integrate collaborative text/markdown editor — CodeMirror + Yjs CRDT, peer cursors, read-only for Viewers, eviction on delete / grant-revoke / external write.
  - [ ] Implement collaborative spreadsheet editor
  - [ ] Develop simple image editor
- [ ] Implement file version control
  - [ ] Create version history system
  - [ ] Add previous version restoration
  - [ ] Develop visual changes comparator

### Multi-device Synchronization
- [ ] Develop synchronization clients
  - [ ] Windows client using Rust
  - [ ] macOS client using Rust
  - [ ] Linux client using Rust
- [ ] Create mobile applications
  - [ ] Android application
  - [ ] iOS application
- [ ] Implement selective synchronization
  - [ ] Allow specific folder selection
  - [ ] Add synchronization profiles
  - [ ] Develop synchronization by file types
- [ ] Develop delta synchronization
  - [ ] Implement incremental change transfer
  - [ ] Add differential compression
  - [ ] Implement intelligent retransmission

### Advanced Sharing
- [x] Improve public links
  - [ ] Add configurable expiration date
  - [x] Implement password protection
  - [ ] Develop download limits
- [x] Implement granular permissions
  - [x] Add per-folder/file permissions
  - [x] Develop customizable roles
  - [x] Implement permission inheritance
- [x] Add real-time collaboration
  - [x] Develop collaborative editing
  - [x] Add presence indicators
  - [ ] Implement per-user change history
- [ ] Integrate with social networks
  - [ ] Add direct sharing to popular platforms
  - [ ] Implement customized preview for networks
  - [ ] Develop sharing statistics

### Robust Security
- [ ] Implement end-to-end encryption
  - [ ] Research and select optimal algorithms
  - [ ] Develop key management system
  - [ ] Add in-transit and at-rest encryption
- [ ] Add multi-factor authentication
  - [ ] Integrate app-based authentication
  - [ ] Add support for U2F/Yubikey
  - [ ] Implement backup codes
- [x] Develop password policies
  - [x] Add customizable requirements
  - [ ] Implement password rotation
  - [ ] Develop compromised password detection
- [x] OPAQUE password login (RFC 9807) — server never sees the plaintext password; DB dump or wire capture yields nothing replayable.
- [x] DPoP session binding (RFC 9449) — every request signs a proof with a browser-held, non-extractable P-256 key; stolen cookies alone are useless.
- [x] Audit logs — structured `target: "audit"` tracing stream for every permission denial, grant change, auth outcome, eviction, admin action; stable event / reason vocabulary so operators can grep + aggregate.
- [ ] Create detailed audit system
  - [x] Log access and actions
  - [ ] Add security alerts
  - [ ] Implement configurable log retention

### Personal Data Management
- [x] Complete CardDAV implementation
  - [x] Finalize contact synchronization
  - [x] Add support for contact groups
  - [x] Implement custom fields
- [x] Complete CalDAV implementation
  - [x] Finalize calendar synchronization
  - [x] Add support for recurring events
  - [ ] Implement notifications/reminders
- [ ] Develop password manager
  - [ ] Create encrypted storage
  - [ ] Add password generator
  - [ ] Implement auto-fill
- [ ] Implement encrypted notes
  - [ ] Develop notes editor
  - [ ] Add tags and organization
  - [ ] Implement full-text search

### Automation and Workflows
- [ ] Create automated rules
  - [ ] Develop automatic file organization
  - [ ] Add scheduled actions
  - [ ] Implement customizable triggers
- [ ] Integrate with productivity tools
  - [ ] Develop connectors for popular services
  - [ ] Add webhooks for integration
  - [x] Implement API for extensions
- [ ] Create customizable workflows
  - [ ] Develop document approval/review
  - [ ] Add configurable states and transitions
  - [ ] Implement automated notifications
- [ ] Implement scheduled actions
  - [ ] Add automated backups
  - [ ] Develop intelligent archiving
  - [ ] Implement periodic analysis

### Intelligence and Analysis
- [ ] Implement OCR for images
  - [ ] Add text recognition in images
  - [ ] Develop indexing of recognized content
  - [ ] Implement search in extracted text
- [ ] Create automatic categorization
  - [ ] Develop content-based classification
  - [ ] Add intelligent grouping
  - [ ] Implement organization suggestions
- [ ] Add intelligent tagging
  - [ ] Implement entity recognition
  - [ ] Develop topic analysis
  - [x] Add facial tagging for photos
- [ ] Develop personalized recommendations
  - [ ] Implement usage-based suggestions
  - [ ] Add relevant content discovery
  - [ ] Develop needs prediction
- [ ] Implement intelligent photo gallery
  - [x] Create advanced photo viewer with smooth zoom and navigation
  - [x] Add EXIF metadata extraction and visualization
  - [x] Implement map of photo locations
  - [x] Develop automatic timeline by date/event
  - [x] Add recognition and grouping by identified people
  - [ ] Implement automatic album creation by events, places, and people
  - [ ] Develop photo search using combined filters (person+place+date)
  - [ ] Add scene and object detection in photos (beach, mountain, animals, etc.)
  - [ ] Implement similar or duplicate photo detection
  - [ ] Add non-destructive basic editing features (crop, filters, adjustments)
  - [x] Video support

### Enterprise Collaboration
- [x] Create shared workspaces
  - [x] Develop team structures — Drives own the storage, Groups hold the grant on them; adding a member to the Group grants them access to every Drive that Group is granted on.
  - [ ] Add project templates
  - [ ] Implement customized dashboards
- [x] Implement role-based access control
  - [x] Develop customizable roles
  - [x] Add granular access policies
  - [ ] Implement segregation of duties
- [x] Implement ReBAC (relationship-based access control) — grants are subject→resource relationships (User/Group/Token) × (File/Folder/Drive/Calendar/AddressBook) × (Viewer/Editor/Owner + custom). Cascades from ancestor folders, expands transitively through group membership, anti-enumeration collapses "no access" and "unknown resource" to the same wire shape, decisions cached per-request.
- [ ] Add comments and annotations
  - [ ] Develop document annotations
  - [ ] Add highlighting and marking
  - [ ] Implement comment resolution
- [ ] Integrate videoconference services
  - [ ] Add direct calls from platform
  - [ ] Develop screen sharing
  - [ ] Implement meeting recording

### Advanced Technical Optimizations
- [ ] Implement distributed architecture
  - [ ] Develop high availability
  - [ ] Add load balancing
  - [ ] Implement fault tolerance
- [ ] Create tiered storage
  - [ ] Develop hot/warm/cold stratification
  - [ ] Add automatic migration policies
  - [ ] Implement cost optimization
- [x] Optimize compression and deduplication
  - [x] Develop adaptive compression
  - [x] Add block-level deduplication
  - [ ] Implement similar file detection

### Interoperability and Extensibility
- [x] Improve RESTful API
  - [x] Complete OpenAPI documentation
  - [ ] Add API versioning
  - [x] Implement intelligent rate limiting
- [x] Message bus (pub/sub over WebSocket + AsyncAPI) — one persistent WS per browser, typed topics (`folder:*`, `collab:*`, `user:*:authz`, `notification:*`), server publishes bus events + revocations to interested subscribers; JSON-RPC 2.0 control plane, binary sub-protocol for CRDT / Yjs frames, AsyncAPI 3.0 spec with generated TS DTOs on the FE.
- [x] CLI for low-level operations — `oxicloud` binary carries admin subcommands (`opaque …`, `migrate …`, …) so operators can run maintenance without a running server.
- [ ] Develop webhook system
  - [ ] Add configurable triggers
  - [ ] Implement retries and reliability
  - [ ] Develop delivery verification
- [x] OIDC login (OxiCloud as OIDC client) — sign in via external IdP, OIDC back-channel logout (IdP → OxiCloud) with JTI replay protection, RP-initiated logout propagation (OxiCloud → IdP end_session_endpoint) so the upstream session dies with ours.
- [ ] Implement OAuth for third parties (OxiCloud as authorization server)
  - [ ] Add standard authentication flows
  - [ ] Develop granular permission management
  - [ ] Implement access revocation
- [ ] Create developer SDK
  - [ ] Develop client libraries for popular languages
  - [ ] Add examples and documentation
  - [ ] Implement testing sandbox

### Governance and Compliance
- [x] Implement retention policies
  - [x] Develop configurable retention by type
  - [ ] Add automatic archiving
  - [x] Implement secure deletion
- [ ] Add regulatory compliance
  - [ ] Develop GDPR tools
  - [ ] Add HIPAA compliance where applicable
  - [ ] Implement compliance matrices
- [ ] Create complete data export
  - [ ] Develop standardized formats
  - [ ] Add scheduled export
  - [ ] Implement data portability
- [ ] Implement legal hold
  - [ ] Develop case-based retention
  - [ ] Add evidence preservation
  - [ ] Implement chain of custody


- lightcss for frontend.
