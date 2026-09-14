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

## AuthZ enforcement points

- **The extractors are NOT a choke point.** Four paths authenticate without ever touching `AuthUser` / `CurrentUserId`: `middleware/admin.rs::require_authenticated` (re-parses the JWT from header *or* cookie itself), the three DAV handlers' hand-rolled `extract_user` (`webdav_handler.rs:154`, `caldav_handler.rs:421`, `carddav_handler.rs:173`), `POST /api/auth/refresh` (mounted outside `auth_middleware`, `main.rs:775`), and `GET /api/rt/ws` (self-auths from a raw Bearer and never reads `claims.role`, `rt_ws.rs:240-255`). A rule added to an extractor does not hold until it is added to these too.
- **Never hand-roll principal extraction.** `req.extensions().get::<Arc<CurrentUser>>()` inside a handler is exactly the anti-pattern above — it bypasses every `FromRequestParts` guard. Take the extractor, or call the shared assertion.
- **A method check is not an authorization check.** GETs that mint credentials or write exist: `GET /api/wopi/editor-url` returns a WOPI token usable for `POST /wopi/files/{id}/contents`; `GET /api/s/{token}` writes via `register_shared_link_access`; `GET /api/auth/device/verify` is an oracle on live device codes; `GET /api/batch/download` builds an arbitrary ZIP from a querystring. Never gate on verb alone.
- **An auth helper's `_ =>` arm must DENY.** Two fail-open gates exist and are bugs, not patterns to copy: `require_internal_user` (`middleware/user.rs:64-71`) admits the caller on *any* `get_user_flags` error, and `decide_live_role` (`:169-176`) resurrects the claim role on a transient DB error. The first is the only middleware guarding all three DAV surfaces.
- **Prefer deny-by-default over assert-in-handler.** A restriction enforced inside the extractor covers ~200 call sites with no edits; the same restriction as "handler takes an optional principal and asserts" is one forgotten call away from silently accepting. `OptionalUserId` (`middleware/auth.rs:85-98`) is the cautionary tale — it exists, it is dead code, and nothing ever used it.
- Anonymous-session direction (share links as a principal): `docs/plan/rationalize-publicshare.md`.

## Storage backend access

- **Read blob content through `Arc<DedupService>`.** It's the ONE canonical read abstraction — CDC-manifest-aware (`file.blob_hash` may reference a chunk manifest, not a blob), backend-agnostic (Local/S3/Azure), wrapper-transparent (encryption/retry/cache). Never take `Arc<dyn BlobStorageBackend>` directly in a service that reads content; you'll silently break on any file ≥ 64 KiB (`CDC_MIN_CHUNK`). Follow `thumbnail_service`, `audio_metadata_service`, `media_metadata_service`, `face_indexing_service`, `search_index::content_index_worker` as reference impls.
- **Reads use `DedupService` methods**: `dedup.read_blob_bytes(hash)` for byte-slice analyzers (ONNX, EXIF, ID3-via-Reader), `dedup.stream_blob_to_tempfile(hash, &temp_dir, ".ext")` for crates that only accept `&Path` (mp3_duration, ffprobe, `nom-exif` video), `dedup.read_blob_stream(hash)` for streaming to a downstream `Stream` consumer.
- **Never hand-craft blob paths.** No `blob_root: PathBuf` fields, no `<storage>/.blobs/<xx>/<hash>.blob` constructions. `BlobStorageBackend::local_blob_path` returns `None` under `EncryptedBlobBackend`; do not rely on it. The three services that did this pre-2026-08 (audio/media/face) are the anti-pattern — see memory `project_services_bypassing_blob_backend`.
- **Persistent state = backend**, not `<storage_path>/*` sidecars. Local sidecars (`.thumbnails/`, `.transcoded/`, `.blob-cache/`, `.search-index/`, `.plugin-logs/`, `.uploads/`) are only for caches (regenerable) or truly-temp scratch (deleted on drop). Anything a user would notice losing → blob backend. Tier-2 migration plan: `docs/plan/derived-blobs.md`.
- **Temp files use `OXICLOUD_TEMP_DIR`** via the shared config path (`AppConfig::temp_dir`) — not raw `std::env::temp_dir()`. Ops point it at real disk on RAM-constrained Linux deployments (default `/tmp` = tmpfs = RAM).

## OpenAPI

- **Every new `#[utoipa::path(...)]` handler MUST also be added to the `paths(...)` list in `src/interfaces/api/mod.rs`.** utoipa emits ONLY registered paths; the annotation alone is invisible to `resources/gen/openapi.json`. Historical drift found 21 annotated handlers that never reached the spec (admin drives, admin jobs pause/findings/purge, admin SMTP, admin storage rotate, admin promote-to-internal, admin sessions list/revoke, all OPAQUE endpoints, DPoP bind, magic-link send, profile PATCH, upgrade-to-internal, drive delete/members/policies/quota, grant notify, trash per-drive, user profile, dedup check-batch) — they all had valid `#[utoipa::path]` blocks but nobody registered them.
- **Every DTO the new handler touches** — request body, response body, path/query params, error shapes — MUST also be added to `components(schemas(...))` in the same file, OR be reachable from an already-registered schema. Utoipa only pulls in schemas transitively from registered paths + registered top-level schemas.
- **After adding: `cargo run --bin generate-openapi`** to regenerate `resources/gen/openapi.json`, then `git diff resources/gen/openapi.json` — the new path + its request/response schemas must be present. Zero-diff means you missed the registration.
- Sanity check for the whole surface: `diff <(grep -oE 'path = "/api[^"]+"' src/interfaces/api/handlers/*.rs | grep -oE '/api[^"]+' | sort -u) <(jq -r '.paths | keys | .[]' resources/gen/openapi.json | sort -u)` — should always be empty. Non-empty diff = drift.
- Handlers referenced by the `paths(...)` list MUST be `pub` (module-visible from the paths list). Private `async fn` compiles at the router mount but breaks the paths list with a visibility error — see `get_smtp_info`, `send_smtp_test`, `get_user_profile` for the retrofit.

### Security / scope in the spec

- **`security(("bearerAuth" = []))` — the empty array is the SCOPES list**, not decoration. Every route currently declares the same thing, so the spec claims "a session is required" for `GET /api/version` and `PUT /api/admin/users/{id}/role` alike: true, and useless. If a route's gate differs from the default, declare it there.
- OpenAPI has **no field for a minimum role** — OAuth2 has no role concept, so the spec has nowhere to put one. Use a pseudo-scope (`["role:admin"]`) rather than a vendor extension no tooling renders.
- **Declaring is not enforcing.** utoipa's `security` wires nothing, so it drifts from the real gate silently. Any scope worth declaring is worth a test cross-checking it against the actual mount — otherwise the spec becomes a parallel description of the authorization boundary rather than a picture of it.
