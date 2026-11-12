# Vault P0: protection boundary and acceptance contract

**Status: proposed implementation contract; no runtime encryption is enabled.**
Checked against main `48893d1bc3c0c52e05b952895fe835ba5623d8bd` on 2026-11-12.
This narrows contribution 3 of [the Vault plan](vault-e2ee.md) so the first
backend change can be reviewed without also choosing a cryptographic format.

## First patch boundary

Keep `DriveKind::{Personal, Shared}`. Introduce a separate persisted,
exhaustively matched protection discriminator, provisionally `Plain` and
`ClientEncrypted`. Protection describes source content, not a recipient's mount
or an editable policy knob. Personal and shared drives must use the same gate.

P0 must preserve ordinary drive behavior and refuse unsupported protected I/O.
It must not expose a Vault creation/unlock control or accept encrypted uploads.
Test fixtures may seed protected drives to prove refusal. A feature flag is
additional containment, not permission to skip protection checks on stored rows.
Switching the flag off must never reinterpret protected rows as plaintext.

The existing `Drive.policies` bag deliberately preserves unknown JSON keys.
Do not put the discriminator there: a policy update, inheritance operation or
older consumer must not silently change the privacy boundary. Keep the property
out of generic drive PATCH/policy requests. Even an empty drive cannot switch
protection until a separate creation/migration contract is agreed.

The migration backfills existing rows as `Plain`, preserves grants, quota and
root-folder identity, and validates unknown values. Repository decoding of an
unknown discriminator must fail explicitly rather than default to `Plain`.
Do not remove a populated protection column during rollback. Mixed-version
rollout requires old writers to be stopped or otherwise prevented from serving
protected rows before those rows are ever enabled in production.

## Existing implementation entry points

These are current integration points, not a complete list of protected callers:

| Layer | Entry point | P0 responsibility |
|---|---|---|
| Domain | `src/domain/entities/drive.rs` | Separate protection from kind and extensible policies; reject unknown persisted values. |
| Persistence | `src/domain/repositories/drive_repository.rs`, `src/infrastructure/repositories/pg/drive_pg_repository.rs` | Persist/read the discriminator in all projections and create paths; migration round trips for both kinds. |
| Application | `src/application/services/drive_management_service.rs` and content services | Resolve the source drive, enforce grants through `AuthorizationEngine`, then reject unsupported protection before content I/O or enqueueing work. |
| API contract | `src/application/dtos/drive_dto.rs`, `src/interfaces/api/handlers/drive_handler.rs` | Agree any read-only wire addition; update OpenAPI and generated frontend types together. Handlers do not become authorization enforcement points. |
| Storage | `src/infrastructure/services/blob_handler.rs` | Retain the existing content-I/O boundary. P0 introduces no plaintext/ciphertext passthrough or new storage framing. |

Reuse a common application-layer protection check and carry its resolved context
through the operation. Do not add an independent database query for every row in
large listings. Batch/project required source-drive metadata and retain the
existing permission filtering, pagination and query-count behavior.

An authorized caller reaching an unsupported protected operation receives the
agreed typed error. An unauthorized caller retains the existing anti-enumeration
behavior. Permission denials still emit structured audit events; unsupported
protection rejections need an opaque identifier and stable reason, not filenames,
plaintext or key material. A context that cannot be resolved is an error, never
proof that content is plain or absent.

## Acceptance matrix for the implementation PR

Every refusal case needs a corresponding ordinary-drive success case. Run the
matrix for both personal and shared sources, including a permitted second user,
a denied user and group-derived access. The table specifies tests to add; it
does not claim these tests or gates exist today.

| Surface / transition | Required invariant and regression evidence |
|---|---|
| Migration and repository reads | Existing rows remain plain with identical IDs, grants and quota. Seeded protected rows round-trip. Unknown values fail explicitly. |
| Drive creation and policy editing | Existing supported creation works; no public request can create, enable or downgrade protected content. Generic policy changes preserve protection. |
| REST upload/download, preview and thumbnail | Protected fixtures fail before a blob read/write or derivative creation. Spy assertions cover zero I/O, not just an error response. |
| Chunk/delta negotiation, by-hash lookup and finalization | No hash-existence probing, upload session, chunk registration or reference publication bypasses the gate. Rejection preserves active content and accounting. |
| Native WebDAV and Nextcloud DAV | GET/HEAD/PUT, PROPFIND, COPY/MOVE and sync/listing paths cannot expose or mutate protected resources. Specify omission/error behavior consistently and preserve plain-client compatibility. |
| Public links and recipient mounts | Resolve protection from the source drive for every access, including after rename/move. A grant, token or mount never converts protected bytes into a supported plaintext path. |
| WOPI and collaboration | Refuse server-readable content/session setup before fetching bytes or storing collaboration state. |
| Search, OCR, EXIF, faces and transcoding | Exclude protected inputs before reading or enqueueing; a job already queued before a policy change must check again when executed. |
| Copy/move across drives | Do not cross protection contexts through metadata-only operations. Rejection leaves source, destination, quota and references unchanged. Preserve same-context plain behavior. |
| Trash, versions and restore | Preserve source context and retained metadata. Refuse unsupported restore/copy paths before mutation; a missing context does not authorize downgrade. |
| Backup, consistency and GC | Preserve opaque protection metadata through backup/restore. A protection refusal or backend read error cannot be interpreted as missing content eligible for deletion. |
| Flag off, restart and older client | Protected fixtures remain inaccessible after flag changes/restart. An older client cannot silently upload plaintext into protected storage. |

Extend existing Hurl scenarios where they cover the same invariant, including
`drives_foundation.hurl`, `drives_membership.hurl`, `drive_policies.hurl`,
`cross_drive_move.hurl`, `cross_drive_copy.hurl`, `external_mounts_webdav.hurl`,
`webdav_permissions.hurl` and `wopi_shared_drive.hurl` under `tests/api/`.
Add focused service/repository tests for the no-I/O and transaction guarantees.
Regenerate API artifacts normally; do not hand-edit generated contracts.

## Decisions to settle before the implementation patch

1. Confirm the separate protection property rather than an additional drive kind,
   then align the reserved-Vault wording in root guidance in that implementation.
2. Agree the persisted/wire names and whether P0 exposes a read-only property.
   Choose exact unsupported-error codes and DAV listing behavior before pinning
   them in Hurl; this document intentionally does not invent those contracts.
3. Agree how development-only protected fixtures are provisioned without a public
   creation API, and document the rollout barrier for older server binaries.
4. Assign the protocol-surface inventory to the implementation review: all doors
   to a service, worker and bulk operation must be accounted for before enabling
   any protected row outside tests.

Envelope algorithms, membership epochs, manifest signatures, recovery keys and
client unlock remain later reviews under the parent plan. A stored discriminator
must never be described as E2EE. The first usable release still requires the
multi-user key, transfer and revocation gates together.

For the implementation, follow root/src `AGENTS.md`: targeted regression first,
format/Clippy, then `just check`, `just test`, `just test-integration` and
`just api-test` in order before pushing. Frontend changes additionally need
`npm run check`, unit tests and a production build. This documentation change
requires reference/consistency review only and does not claim runtime validation.
