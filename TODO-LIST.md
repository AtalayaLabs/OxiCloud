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
- [ ] Add basic PDF viewer
- [ ] Generate thumbnails for images
- [x] Implement specific icons by file type
- [ ] Add text/code preview

### Enhanced Search
- [x] Implement search by name
- [x] Add filters by file type
- [x] Implement search by date range
- [x] Add filter by file size
- [x] Add search within specific folders
- [x] Implement cache for search results

### UI/UX Optimizations
- [ ] Improve responsive design for mobile devices
- [ ] Implement drag & drop between folders
- [x] Add support for multiple file selection
- [x] Implement multiple file uploads
- [ ] Add progress indicators for long operations
- [x] Implement UI notifications for events
- [ ] Photos timeline: virtual scrolling
  - Solo mantener en el DOM las filas visibles en el viewport + un margen. Al hacer scroll, reciclar los nodos que salen por arriba para los que entran por abajo.
  - Ventajas: Funciona perfectamente con 50,000 fotos. Uso de memoria constante.
  - Excesiva para ahora — evaluar cuando el volumen de fotos lo justifique.

## Phase 2: Authentication and Multi-User

### User System
- [x] Design data model for users
- [x] Implement user registration
- [x] Create login system
- [ ] Add user profile page
- [ ] Implement password recovery
- [x] Separate storage by user

### Quotas and Permissions
- [x] Implement storage quota system
- [x] Add basic role system (admin/user)
- [ ] Create admin panel
- [x] Implement folder-level permissions
- [x] Add storage usage monitoring

### Basic Security
- [x] Implement secure password hashing with Argon2
- [x] Add session management
- [x] Implement JWT authentication token
- [ ] Add CSRF protection
- [ ] Implement login attempt limits
- [ ] Create activity logging system

## Phase 3: Collaboration Features

### File Sharing
- [x] Implement shared link generation
- [x] Add permission configuration for links
- [x] Implement password protection for links
- [ ] Add expiration dates for shared links
- [x] Create page to manage all shared resources
- [ ] Implement sharing notifications

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
- [ ] Implement automatic documentation (Swagger)
- [ ] Create API token system
- [ ] Implement rate limiting
- [ ] Add API versioning

### WebDAV Support
- [x] Implement basic WebDAV server
- [x] Add authentication for WebDAV
- [x] Implement PROPFIND operations
- [x] Add support for locking
- [x] Test compatibility with standard clients
- [x] Optimize WebDAV performance
- [ ] Implement Range Requests (RFC 7233) for resumable transfers
- [x] Support partial file updates with HTTP PATCH for bandwidth efficiency (RFC 5789, `X-Update-Range` header)
- [x] Implement sync-collection REPORT (RFC 6578) for incremental change discovery — WebDAV files/folders (incl. NextCloud surface), CalDAV events, CardDAV contacts, backed by a durable change-log

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
- [ ] Research and select encryption algorithms
- [ ] Implement at-rest encryption for files
- [ ] Add key management
- [ ] Implement encryption for shared files
- [ ] Create security documentation

### File Versioning
- [ ] Design version storage system
- [ ] Implement version history
- [ ] Add difference visualization
- [ ] Implement version restoration
- [ ] Add version retention policies

### Basic Applications
- [x] Design plugin/app system
- [ ] Implement basic text viewer/editor
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
- [ ] Implement differential sync algorithm (similar to rsync)
- [x] Add strong ETag support for more efficient caching

### Frontend
- [ ] Optimize initial asset loading
- [ ] Implement lazy loading for large lists
- [x] Add local cache (localStorage/IndexedDB)
- [ ] Optimize UI rendering
- [ ] Implement intelligent prefetching
- [ ] Add basic offline support
- [ ] Implement client-side image resizing before upload
- [x] Add HTTP/2 support for multiplexing requests
- [ ] Implement progressive image loading

### Storage
- [ ] Research deduplication options
- [ ] Implement block storage
- [x] Add transparent compression by file type
- [ ] Implement log rotation and archiving
- [ ] Create automated backup system
- [ ] Add support for distributed storage
- [ ] Implement media transcoding for optimized delivery
- [x] Add content-aware compression by file format
- [ ] Implement dynamic thumbnail resizing based on viewport

### Bandwidth & Transfer Optimization
- [ ] **Sub-file chunked dedup (Restic/Borg style)**
  - [ ] Implement Content-Defined Chunking (CDC) with FastCDC/Rabin rolling hash
  - [ ] Variable-size chunks (target 1-4 MB) instead of whole-file blobs
  - [ ] Per-chunk BLAKE3 hashing and dedup (saves storage + bandwidth on similar files)
  - [ ] Chunk-level Zstd compression (better ratio than whole-file)
  - [ ] Migrate existing whole-file blobs to chunked storage
- [ ] **Delta sync / rsync-style transfers**
  - [ ] Implement rolling checksum algorithm for block-level diffing
  - [ ] Client sends only changed blocks on re-upload (not the full file)
  - [ ] Server-side block assembly from delta + existing chunks
  - [ ] Huge savings for large files with small edits (VMs, databases, ISOs)
- [ ] **Resumable uploads & downloads (RFC 7233 / tus.io)**
  - [ ] Server tracks partial upload state; client resumes from last byte on failure
  - [ ] HTTP Range responses for download resume after network drops
  - [ ] tus.io protocol support for cross-client compatibility
- [ ] **Client-side optimization before upload**
  - [ ] Resize images to configurable max dimensions before upload (e.g. 4K cap)
  - [ ] Re-encode videos to efficient codec (H.265/AV1) client-side before upload
  - [ ] ⚡ **HIGH IMPACT / QUICK WIN** — Pre-compute BLAKE3 hash client-side (WASM); query server before upload; skip transfer entirely if blob already exists (instant dedup, zero bandwidth)
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
  - [ ] Content-aware: skip compression for already-compressed formats (JPEG, ZIP, etc.)
- [ ] **Batched & multiplexed operations**
  - [ ] Batch small file uploads into single request (tar-stream or multipart bundle)
  - [ ] HTTP/2 multiplexing for parallel chunk transfers on single connection
  - [ ] Server-side ZIP streaming for multi-file download (already partial)

## Infrastructure and Deployment

- [x] Create Docker configuration
- [ ] Implement CI/CD with GitHub Actions
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
- [ ] Implement preview for different file types
  - [ ] Create integrated PDF viewer
  - [ ] Add office document viewer
  - [ ] Develop code viewer with syntax highlighting
- [ ] Add online document editing
  - [ ] Integrate text/markdown editor
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
  - [x] Implement incremental change transfer (RFC 6578 sync-collection REPORT — WebDAV/CalDAV/CardDAV)
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
- [ ] Add real-time collaboration
  - [ ] Develop collaborative editing
  - [ ] Add presence indicators
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
- [ ] Create detailed audit system
  - [ ] Log access and actions
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
  - [ ] Add facial tagging for photos
- [ ] Develop personalized recommendations
  - [ ] Implement usage-based suggestions
  - [ ] Add relevant content discovery
  - [ ] Develop needs prediction
- [ ] Implement intelligent photo gallery
  - [ ] Create advanced photo viewer with smooth zoom and navigation
  - [ ] Add EXIF metadata extraction and visualization
  - [ ] Implement map of photo locations
  - [ ] Develop automatic timeline by date/event
  - [ ] Add recognition and grouping by identified people
  - [ ] Implement automatic album creation by events, places, and people
  - [ ] Develop photo search using combined filters (person+place+date)
  - [ ] Add scene and object detection in photos (beach, mountain, animals, etc.)
  - [ ] Implement similar or duplicate photo detection
  - [ ] Add non-destructive basic editing features (crop, filters, adjustments)
  - [ ] Video support

### Enterprise Collaboration
- [ ] Create shared workspaces
  - [ ] Develop team structures
  - [ ] Add project templates
  - [ ] Implement customized dashboards
- [x] Implement role-based access control
  - [x] Develop customizable roles
  - [x] Add granular access policies
  - [ ] Implement segregation of duties
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
  - [ ] Add block-level deduplication
  - [ ] Implement similar file detection

### Interoperability and Extensibility
- [x] Improve RESTful API
  - [x] Complete OpenAPI documentation
  - [ ] Add API versioning
  - [ ] Implement intelligent rate limiting
- [ ] Develop webhook system
  - [ ] Add configurable triggers
  - [ ] Implement retries and reliability
  - [ ] Develop delivery verification
- [ ] Implement OAuth for third parties
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
