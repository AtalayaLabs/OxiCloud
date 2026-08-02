# src/AGENTS.md — backend-only notes

Non-obvious rules that trip up new code. Terse on purpose.

## Auth policy

- **OIDC is the master identity provider.** Whenever `AuthApplicationService::oidc_enabled()` returns true, magic-link login MUST be off — `is_magic_link_login_allowed()` returns false regardless of `OXICLOUD_AUTH_METHODS`. Rationale: OIDC may enforce 2FA / step-up; a mailbox-possession bypass would silently sidestep it.
- **Password / magic-link handlers gate via `is_password_login_allowed()` / `is_magic_link_login_allowed()`**, never raw config or `password_login_disabled()` alone. The composed helpers merge the legacy OIDC-only flag, `OXICLOUD_AUTH_METHODS`, SMTP wiring, and the OIDC-master rule in one place.
- **Magic-link redemption** distinguishes login tokens (`resource_kind = None`) from invitation tokens (File / Folder). The login gate only applies to the None case; invitations follow their own admin-mediated trust chain.
- **`OXICLOUD_REQUIRE_VERIFIED_EMAIL`** gates login on `email_verified_at IS NOT NULL`. Admin-created (`admin_create_user`) and setup-admin (`setup_create_admin`) users are stamped verified at creation — admin fiat counts. OIDC-JIT already stamps verified. Admins are EXEMPT from the gate at login regardless of `email_verified_at` — pre-existing admin accounts from before this flag shipped must never be locked out of their own instance. Regular users hit the gate; the frontend detects the `EmailNotVerified` error_type and offers a resend-magic-link CTA.
- **Startup gate in `main.rs`**: magic-link-only allowlist + no SMTP = panic. Never soften to warn.

## New auth surfaces

- Any new endpoint that mints or consumes credentials/tokens must consult one of the `is_*_login_allowed()` helpers, not the raw allowlist.
- Any new "policy-disabled" refusal must emit an `audit`-target line before returning — matches `auth.login_rejected`, `magic_link.redemption_rejected` conventions.

## Storage backend access

- **Read blob content through `Arc<DedupService>`.** It's the ONE canonical read abstraction — CDC-manifest-aware (`file.blob_hash` may reference a chunk manifest, not a blob), backend-agnostic (Local/S3/Azure), wrapper-transparent (encryption/retry/cache). Never take `Arc<dyn BlobStorageBackend>` directly in a service that reads content; you'll silently break on any file ≥ 64 KiB (`CDC_MIN_CHUNK`). Follow `thumbnail_service`, `audio_metadata_service`, `media_metadata_service`, `face_indexing_service`, `search_index::content_index_worker` as reference impls.
- **Reads use `DedupService` methods**: `dedup.read_blob_bytes(hash)` for byte-slice analyzers (ONNX, EXIF, ID3-via-Reader), `dedup.stream_blob_to_tempfile(hash, &temp_dir, ".ext")` for crates that only accept `&Path` (mp3_duration, ffprobe, `nom-exif` video), `dedup.read_blob_stream(hash)` for streaming to a downstream `Stream` consumer.
- **Never hand-craft blob paths.** No `blob_root: PathBuf` fields, no `<storage>/.blobs/<xx>/<hash>.blob` constructions. `BlobStorageBackend::local_blob_path` returns `None` under `EncryptedBlobBackend`; do not rely on it. The three services that did this pre-2026-08 (audio/media/face) are the anti-pattern — see memory `project_services_bypassing_blob_backend`.
- **Persistent state = backend**, not `<storage_path>/*` sidecars. Local sidecars (`.thumbnails/`, `.transcoded/`, `.blob-cache/`, `.search-index/`, `.plugin-logs/`, `.uploads/`) are only for caches (regenerable) or truly-temp scratch (deleted on drop). Anything a user would notice losing → blob backend. Tier-2 migration plan: `docs/plan/derived-blobs.md`.
- **Temp files use `OXICLOUD_TEMP_DIR`** via the shared config path (`AppConfig::temp_dir`) — not raw `std::env::temp_dir()`. Ops point it at real disk on RAM-constrained Linux deployments (default `/tmp` = tmpfs = RAM).
