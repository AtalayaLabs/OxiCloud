# Share Mounts P0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Granted folders and shared drives appear as mount rows inside the recipient's personal drive, listable and browsable through the existing folder API and both WebDAV surfaces, with the recipient the only subject who can see them.

**Architecture:** A mount is a `storage.folders` row with `mount_target_id` set. Database triggers enforce the invariants (personal drive only, no children, no mount-of-mount, target in another drive). A new `ShareMountService` owns the lifecycle (idempotent `reconcile(user)` plus event hooks) and the single resolution step "a mount row answers for its target". `FolderService`, `TrashService` and `FileManagementService` call that step at their entry points; the listing SQL filters mount rows nobody but the recipient may see.

**Tech Stack:** Rust 1.93 (edition 2024), Axum, sqlx/PostgreSQL 13+ (ltree), SvelteKit/Svelte 5, Vitest, Hurl.

**Spec:** `docs/plan/share-mounts.md` (read it first; this plan implements § The model, § Lifecycle, § Resolution R0–R3, the P0 rows of § API, and § Phasing P0). URL work (§ URL surface, `resolve` endpoint, ancestors hop, etag propagation) is **P1 and out of scope here**.

## Global Constraints

- AuthZ lives in the service layer, never in handlers (`AGENTS.md` § Authorization). Every new service method that touches a user-scoped resource takes the caller and calls `authz.require(...)` before reading or mutating.
- Every denial emits `tracing::info!(target: "audit", event = "...", reason = "...", ...)` before returning (`AGENTS.md` § Audit logging). New events in this plan: `share_mount.denied`, `share_mount.rejected`, `share_mount.created`, `share_mount.removed`, `share_mount.relocated`.
- Anti-enumeration: a mount the caller may not see is `NotFound`, never `AccessDenied`.
- A mount never widens access: resolution authorises the **caller** against the **target**. No code path may act "as the mount owner".
- No data loss: nothing in this plan deletes or alters rows in the target drive. Unmount, decline and relocation touch only the recipient's personal drive.
- Comments in code: one line, current state only, no history (user rule). Commit messages carry the rationale.
- Pre-commit: `cargo fmt --all` then `cargo clippy --all-features --all-targets -- -D warnings`. Frontend: `cd frontend && npm run check && npm run test:unit`.
- Feature flag `OXICLOUD_ENABLE_SHARE_MOUNTS`, default `false`. Off = service absent, mount rows hidden, hooks no-op. The Hurl run (Task 11) must set it to `true` in `tests/common/server.env`.
- Migrations: new timestamp must be greater than `20261101000000` (current newest). Use `20261102000000_share_mounts.sql` and `20261102000001_copy_folder_tree_skip_mounts.sql`.
- Do not edit `Cargo.lock` or `frontend/package-lock.json` by hand. Regenerate `resources/gen/openapi.json` with `cargo run --bin generate-openapi` after DTO changes.
- Work on branch `vitas`; plain commit messages, no co-author trailers.

## Review Focus

Inputs the spec implies but no task's tests exercised at planning time. Each line's test is added to the owning task below.

1. **A grant whose target is inside the recipient's own personal drive** (Alice grants Alice's own subfolder to Alice via a group). Expected: no mount is created (I4); reconcile must not loop or error. Test in Task 3.
2. **Group grant where the group contains the granter.** Expected: the granter gets no mount (`granted_by <> user`), other members do. Test in Task 3.
3. **Name collision with the recipient's own folder, then rename of that folder.** Expected: mount is created as `Name (2)`; after the user renames their own folder away, the mount keeps `Name (2)` (no silent rename). Test in Task 2.
4. **Trashing the target folder.** Expected: the mount disappears from listings immediately (target unreadable) without any row change; restoring the target brings it back. Test in Task 5.
5. **Share-token caller (public link) listing a folder that contains a mount.** Expected: the mount row is never returned. Test in Task 5.

---

## File Structure

| Path | Responsibility |
|---|---|
| `migrations/20261102000000_share_mounts.sql` | column, indexes, triggers I1–I5, `share_mount_declines`, `auth.users.mounts_reconciled_at` |
| `migrations/20261102000001_copy_folder_tree_skip_mounts.sql` | `storage.copy_folder_tree` skips mount rows (R3) |
| `src/common/errors.rs` + `src/domain/errors.rs` + `src/interfaces/errors.rs` | `ErrorKind::Conflict` → HTTP 409 |
| `src/infrastructure/repositories/pg/share_mount_pg_repository.rs` | all SQL for mounts: lookup, reconcile set, create with suffix, delete, declines, subtree mounts |
| `src/application/services/share_mount_service.rs` | lifecycle + resolution; `ShareMountLoginHook` |
| `src/application/dtos/folder_dto.rs` | `MountDto`, `FolderDto.mount`, `FolderResourceRow.mount_*` |
| `src/infrastructure/repositories/pg/folder_db_repository.rs` | listing SQL: mount columns + R0 filter; `list_subtree_folders` excludes mounts |
| `src/application/services/folder_service.rs` | R1 redirect in list/get/create; R2 guards in move/delete |
| `src/application/services/file_management_service.rs` | copy of a mount refused |
| `src/application/services/trash_service.rs` | mount → unmount; relocate mounts under a trashed ancestor |
| `src/interfaces/api/handlers/folder_handler.rs`, `file_handler.rs`, `grant_handler.rs`, `mount_handler.rs` (new) | handler glue, hooks, `POST /api/mounts` |
| `src/application/services/drive_management_service.rs`, `subject_group_service.rs` | membership hooks |
| `src/common/config.rs`, `src/common/di.rs`, `src/interfaces/api/handlers/config_handler.rs`, `src/interfaces/api/routes.rs` | flag, wiring, route |
| `frontend/src/lib/api/types.ts`, `endpoints/grants.ts`, `endpoints/mounts.ts` (new), `routes/files/[...path]/+page.svelte`, `routes/shared-with-me/+page.svelte`, `static/locales/*.json` | badge, links, remount action |
| `tests/api/share_mounts.hurl`, `tests/api/run.sh` | end-to-end scenario |

---

### Task 1: Schema, invariants, and a 409 error kind

**Files:**
- Create: `migrations/20261102000000_share_mounts.sql`
- Modify: `src/domain/errors.rs:15-40` (enum), `:91` (Display), add constructor near `:164`
- Modify: `src/interfaces/errors.rs:130-136`
- Test: `src/infrastructure/repositories/pg/share_mount_pg_repository.rs` (created here with the test module only; SQL methods come in Task 2)

**Interfaces:**
- Produces: `storage.folders.mount_target_id UUID NULL`, `storage.share_mount_declines(recipient_id, target_folder_id, declined_at)`, `auth.users.mounts_reconciled_at TIMESTAMPTZ NULL`, `DomainError::conflict(entity: &'static str, msg) -> DomainError` mapping to HTTP 409.

- [ ] **Step 1: Add `ErrorKind::Conflict`**

In `src/domain/errors.rs`, inside `pub enum ErrorKind` add after `AlreadyExists`:

```rust
    /// Request conflicts with current state (not a duplicate).
    Conflict,
```

In the `Display` match near line 91 add `ErrorKind::Conflict => "Conflict",`. Next to `operation_not_supported` (line 164) add:

```rust
    pub fn conflict<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self::new(ErrorKind::Conflict, entity_type, message)
    }
```

In `src/interfaces/errors.rs` next to line 130 add `ErrorKind::Conflict => StatusCode::CONFLICT,`. Run `cargo build` and fix any other exhaustive `match` on `ErrorKind` the compiler reports (`src/interfaces/api/handlers/contacts_handler.rs:145` is one).

- [ ] **Step 2: Write the migration**

Create `migrations/20261102000000_share_mounts.sql`:

```sql
-- ════════════════════════════════════════════════════════════════════════════
-- Share mount points (docs/plan/share-mounts.md § The model)
-- ════════════════════════════════════════════════════════════════════════════
-- A folder row with mount_target_id set is a mount: it lives in the
-- recipient's personal drive and answers for the target folder, which stays
-- in its own drive. Invariants I1–I5 are enforced here, not in Rust.

ALTER TABLE storage.folders
    ADD COLUMN IF NOT EXISTS mount_target_id UUID
        REFERENCES storage.folders(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_folders_mount_target
    ON storage.folders(mount_target_id)
    WHERE mount_target_id IS NOT NULL;

-- I5: one mount per (recipient drive, target).
CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_one_mount_per_target
    ON storage.folders(drive_id, mount_target_id)
    WHERE mount_target_id IS NOT NULL AND NOT is_trashed;

-- I1, I3, I4 on the mount row itself.
CREATE OR REPLACE FUNCTION storage.check_share_mount_row()
RETURNS TRIGGER AS $$
DECLARE
    v_drive_kind   TEXT;
    v_target       storage.folders%ROWTYPE;
BEGIN
    IF NEW.mount_target_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT kind INTO v_drive_kind FROM storage.drives WHERE id = NEW.drive_id;
    IF v_drive_kind IS DISTINCT FROM 'personal' THEN
        RAISE EXCEPTION 'share mount must live in a personal drive'
            USING ERRCODE = 'check_violation';
    END IF;
    SELECT * INTO v_target FROM storage.folders WHERE id = NEW.mount_target_id;
    IF v_target.id IS NULL THEN
        RAISE EXCEPTION 'share mount target does not exist'
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF v_target.mount_target_id IS NOT NULL THEN
        RAISE EXCEPTION 'share mount target must not itself be a mount'
            USING ERRCODE = 'check_violation';
    END IF;
    IF v_target.drive_id = NEW.drive_id THEN
        RAISE EXCEPTION 'share mount target must be in another drive'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_share_mount_row ON storage.folders;
CREATE TRIGGER trg_share_mount_row
    BEFORE INSERT OR UPDATE OF mount_target_id, drive_id, parent_id ON storage.folders
    FOR EACH ROW
    EXECUTE FUNCTION storage.check_share_mount_row();

-- I2: a mount has no children. TG_ARGV[0] names the parent column.
CREATE OR REPLACE FUNCTION storage.forbid_children_of_share_mount()
RETURNS TRIGGER AS $$
DECLARE
    v_parent UUID;
BEGIN
    v_parent := (to_jsonb(NEW) ->> TG_ARGV[0])::uuid;
    IF v_parent IS NOT NULL AND EXISTS (
        SELECT 1 FROM storage.folders p
         WHERE p.id = v_parent AND p.mount_target_id IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'share mount cannot have children'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_no_children_under_mount_folders ON storage.folders;
CREATE TRIGGER trg_no_children_under_mount_folders
    BEFORE INSERT OR UPDATE OF parent_id ON storage.folders
    FOR EACH ROW
    EXECUTE FUNCTION storage.forbid_children_of_share_mount('parent_id');

DROP TRIGGER IF EXISTS trg_no_children_under_mount_files ON storage.files;
CREATE TRIGGER trg_no_children_under_mount_files
    BEFORE INSERT OR UPDATE OF folder_id ON storage.files
    FOR EACH ROW
    EXECUTE FUNCTION storage.forbid_children_of_share_mount('folder_id');

-- Recipient declined a target: reconcile must not recreate the mount.
CREATE TABLE IF NOT EXISTS storage.share_mount_declines (
    recipient_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    target_folder_id UUID NOT NULL REFERENCES storage.folders(id) ON DELETE CASCADE,
    declined_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recipient_id, target_folder_id)
);

-- Driver for the periodic reconcile job (P2). NULL = never reconciled.
ALTER TABLE auth.users
    ADD COLUMN IF NOT EXISTS mounts_reconciled_at TIMESTAMPTZ;

COMMENT ON COLUMN storage.folders.mount_target_id IS
    'Non-NULL marks a share mount: the row answers for the target folder (docs/plan/share-mounts.md).';
```

- [ ] **Step 3: Write failing trigger tests**

Create `src/infrastructure/repositories/pg/share_mount_pg_repository.rs` with only the test module for now. Register it in `src/infrastructure/repositories/pg/mod.rs` with `pub mod share_mount_pg_repository;` (follow the existing `pub mod` list). Tests use the testcontainers harness `crate::mount_it_support` (compiled under `--cfg integration_tests`).

```rust
//! PostgreSQL access for share mounts (docs/plan/share-mounts.md).

#[cfg(all(test, integration_tests))]
mod trigger_tests {
    use crate::mount_it_support::{fresh_db, make_user, provision_folder};
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Creates a shared drive with a root folder and an owner grant. Returns (drive_id, root_folder_id).
    pub(crate) async fn make_shared_drive(pool: &PgPool, name: &str, owner: Uuid) -> (Uuid, Uuid) {
        let drive_id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let root: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by)
             VALUES ($1, NULL, $2, $3, $3) RETURNING id",
        )
        .bind(name)
        .bind(drive_id)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
            .bind(root)
            .bind(drive_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'drive', $2, 'owner', $1)",
        )
        .bind(owner)
        .bind(drive_id)
        .execute(pool)
        .await
        .unwrap();
        (drive_id, root)
    }

    pub(crate) async fn personal_root(pool: &PgPool, user: Uuid) -> (Uuid, Uuid) {
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT id, root_folder_id FROM storage.drives WHERE default_for_user = $1",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn insert_mount(pool: &PgPool, parent: Uuid, drive: Uuid, target: Uuid, by: Uuid, name: &str) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by, mount_target_id)
             VALUES ($1, $2, $3, $4, $4, $5) RETURNING id",
        )
        .bind(name).bind(parent).bind(drive).bind(by).bind(target)
        .fetch_one(pool)
        .await
    }

    #[tokio::test]
    async fn mount_row_accepted_in_personal_drive() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let id = insert_mount(&pool, bob_root, bob_drive, alice.mount_folder_id, bob, "Docs").await;
        assert!(id.is_ok(), "{id:?}");
    }

    #[tokio::test]
    async fn i1_mount_in_shared_drive_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", bob).await;
        let err = insert_mount(&pool, sd_root, sd, alice.mount_folder_id, bob, "Docs").await.unwrap_err();
        assert!(err.to_string().contains("personal drive"), "{err}");
    }

    #[tokio::test]
    async fn i2_child_under_mount_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let mount = insert_mount(&pool, bob_root, bob_drive, alice.mount_folder_id, bob, "Docs").await.unwrap();
        let err = sqlx::query(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('x', $1, $2, $3, $3)",
        )
        .bind(mount).bind(bob_drive).bind(bob)
        .execute(&*pool).await.unwrap_err();
        assert!(err.to_string().contains("cannot have children"), "{err}");
    }

    #[tokio::test]
    async fn i3_mount_of_mount_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let carol = make_user(&pool, "carol").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let (carol_drive, carol_root) = personal_root(&pool, carol).await;
        let bob_mount = insert_mount(&pool, bob_root, bob_drive, alice.mount_folder_id, bob, "Docs").await.unwrap();
        let err = insert_mount(&pool, carol_root, carol_drive, bob_mount, carol, "Docs").await.unwrap_err();
        assert!(err.to_string().contains("itself be a mount"), "{err}");
    }

    #[tokio::test]
    async fn i4_target_in_same_drive_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let (alice_drive, alice_root) = personal_root(&pool, alice.owner_id).await;
        let err = insert_mount(&pool, alice_root, alice_drive, alice.mount_folder_id, alice.owner_id, "Docs2").await.unwrap_err();
        assert!(err.to_string().contains("another drive"), "{err}");
    }

    #[tokio::test]
    async fn i5_second_mount_of_same_target_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        insert_mount(&pool, bob_root, bob_drive, alice.mount_folder_id, bob, "Docs").await.unwrap();
        let err = insert_mount(&pool, bob_root, bob_drive, alice.mount_folder_id, bob, "Docs (2)").await.unwrap_err();
        assert!(err.to_string().contains("idx_folders_one_mount_per_target"), "{err}");
    }
}
```

Note: `provision_folder(&pool, "alice", "Docs")` returns `Provisioned { owner_id, drive_id, mount_folder_id }` where `mount_folder_id` is the created folder "Docs" (the name comes from the external-mount harness; here it is just an ordinary folder). `make_user` provisions no drive, so call `DrivePgRepository::create_personal_drive_atomic(bob, None)` is needed before `personal_root` works: check `src/mount_it_support.rs`; if `make_user` does not create a drive, add a helper `make_user_with_drive(pool, name) -> Uuid` in the test module that calls `crate::infrastructure::repositories::pg::DrivePgRepository::new(pool.clone()).create_personal_drive_atomic(uid, None).await.unwrap()` and use it in place of `make_user` throughout this plan.

- [ ] **Step 4: Run the tests to see them fail**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount_pg_repository -- --nocapture`
Expected: compile OK, every test FAILS (`column "mount_target_id" does not exist`) because `fresh_db` applies migrations and the migration is not yet picked up only if the file is missing; if you wrote Step 2 first, instead temporarily move the migration away to observe the failure, then restore it.

- [ ] **Step 5: Run the tests to see them pass**

Run the same command. Expected: 6 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add migrations/20261102000000_share_mounts.sql src/domain/errors.rs src/interfaces/errors.rs src/infrastructure/repositories/pg/mod.rs src/infrastructure/repositories/pg/share_mount_pg_repository.rs
git commit -m "feat(share-mounts): schema, invariant triggers, Conflict error kind"
```

---

### Task 2: `ShareMountPgRepository`

**Files:**
- Modify: `src/infrastructure/repositories/pg/share_mount_pg_repository.rs`
- Modify: `src/infrastructure/repositories/pg/mod.rs` (re-export `ShareMountPgRepository`)

**Interfaces:**
- Produces:

```rust
pub struct MountRow {
    pub mount_id: Uuid,
    pub mount_drive_id: Uuid,        // recipient's personal drive
    pub recipient_id: Uuid,          // drives.default_for_user
    pub target_id: Uuid,
    pub target_drive_id: Uuid,
    pub kind: MountKind,             // SharedDrive if target is a drive root, else SharedFolder
    pub name: String,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountKind { SharedFolder, SharedDrive }
impl MountKind { pub fn as_str(self) -> &'static str }  // "shared_folder" | "shared_drive"

pub struct ShareMountPgRepository { pool: Arc<PgPool> }
impl ShareMountPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self;
    pub async fn mount_info(&self, folder_id: Uuid) -> Result<Option<MountRow>, DomainError>;
    pub async fn mount_worthy_targets(&self, user_id: Uuid, personal_drive_id: Uuid) -> Result<Vec<Uuid>, DomainError>;
    pub async fn existing_mounts(&self, personal_drive_id: Uuid) -> Result<Vec<MountRow>, DomainError>;
    pub async fn create_mount(&self, personal_drive_id: Uuid, parent_id: Uuid, target_id: Uuid, user_id: Uuid) -> Result<MountRow, DomainError>;
    pub async fn delete_mount(&self, mount_id: Uuid) -> Result<(), DomainError>;
    pub async fn mounts_in_subtree(&self, folder_id: Uuid) -> Result<Vec<MountRow>, DomainError>;
    pub async fn mounts_for_targets(&self, personal_drive_id: Uuid, target_ids: &[Uuid]) -> Result<HashMap<Uuid, Uuid>, DomainError>; // target -> mount
    pub async fn insert_decline(&self, recipient_id: Uuid, target_id: Uuid) -> Result<(), DomainError>;
    pub async fn delete_decline(&self, recipient_id: Uuid, target_id: Uuid) -> Result<(), DomainError>;
    pub async fn declined_targets(&self, recipient_id: Uuid, target_ids: &[Uuid]) -> Result<HashSet<Uuid>, DomainError>;
    pub async fn caller_can_read_target(&self, user_id: Uuid, target_id: Uuid) -> Result<bool, DomainError>;
    pub async fn touch_reconciled(&self, user_id: Uuid) -> Result<(), DomainError>;
}
```

- [ ] **Step 1: Write failing repository tests**

Append to the file (same `#[cfg(all(test, integration_tests))]` gating, new module `repo_tests`, reuse `make_shared_drive`, `personal_root` by making them `pub(crate)` in `trigger_tests` and importing `super::trigger_tests::*`):

```rust
#[cfg(all(test, integration_tests))]
mod repo_tests {
    use super::trigger_tests::{make_shared_drive, personal_root};
    use super::*;
    use crate::mount_it_support::{fresh_db, make_user, provision_folder};

    async fn grant_folder(pool: &sqlx::PgPool, subject_type: &str, subject: Uuid, folder: Uuid, by: Uuid) {
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ($1, $2, 'folder', $3, 'viewer', $4)",
        ).bind(subject_type).bind(subject).bind(folder).bind(by).execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn worthy_targets_direct_grant_and_shared_drive() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, _) = personal_root(&pool, bob).await;
        grant_folder(&pool, "user", bob, alice.mount_folder_id, alice.owner_id).await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        sqlx::query("INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
                     VALUES ('user', $1, 'drive', $2, 'editor', $3)")
            .bind(bob).bind(sd).bind(alice.owner_id).execute(&*pool).await.unwrap();
        let repo = ShareMountPgRepository::new(pool.clone());
        let mut got = repo.mount_worthy_targets(bob, bob_drive).await.unwrap();
        got.sort();
        let mut want = vec![alice.mount_folder_id, sd_root];
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn worthy_targets_skip_nested_grant_and_own_drive_and_granter() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        // nested: grant on Docs and on Docs/Sub -> only Docs
        let sub: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Sub', $1, $2, $3, $3) RETURNING id")
            .bind(alice.mount_folder_id).bind(alice.drive_id).bind(alice.owner_id).fetch_one(&*pool).await.unwrap();
        grant_folder(&pool, "user", bob, alice.mount_folder_id, alice.owner_id).await;
        grant_folder(&pool, "user", bob, sub, alice.owner_id).await;
        // own drive: a grant on bob's own root to bob (I4) -> skipped
        grant_folder(&pool, "user", bob, bob_root, alice.owner_id).await;
        // granter: alice granted alice's folder to alice -> skipped for alice
        grant_folder(&pool, "user", alice.owner_id, alice.mount_folder_id, alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert_eq!(repo.mount_worthy_targets(bob, bob_drive).await.unwrap(), vec![alice.mount_folder_id]);
        let (alice_drive, _) = personal_root(&pool, alice.owner_id).await;
        assert!(repo.mount_worthy_targets(alice.owner_id, alice_drive).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn worthy_targets_skip_folder_grant_inside_member_drive_and_declined() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let inside: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Inside', $1, $2, $3, $3) RETURNING id")
            .bind(sd_root).bind(sd).bind(alice.owner_id).fetch_one(&*pool).await.unwrap();
        sqlx::query("INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
                     VALUES ('user', $1, 'drive', $2, 'viewer', $3)")
            .bind(bob).bind(sd).bind(alice.owner_id).execute(&*pool).await.unwrap();
        grant_folder(&pool, "user", bob, inside, alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert_eq!(repo.mount_worthy_targets(bob, bob_drive).await.unwrap(), vec![sd_root]);
        repo.insert_decline(bob, sd_root).await.unwrap();
        assert!(repo.mount_worthy_targets(bob, bob_drive).await.unwrap().is_empty());
        repo.delete_decline(bob, sd_root).await.unwrap();
        assert_eq!(repo.mount_worthy_targets(bob, bob_drive).await.unwrap(), vec![sd_root]);
    }

    #[tokio::test]
    async fn create_mount_suffixes_on_collision_and_keeps_name_after_rename() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let own: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Docs', $1, $2, $3, $3) RETURNING id")
            .bind(bob_root).bind(bob_drive).bind(bob).fetch_one(&*pool).await.unwrap();
        let repo = ShareMountPgRepository::new(pool.clone());
        let m = repo.create_mount(bob_drive, bob_root, alice.mount_folder_id, bob).await.unwrap();
        assert_eq!(m.name, "Docs (2)");
        assert_eq!(m.kind, MountKind::SharedFolder);
        assert_eq!(m.recipient_id, bob);
        sqlx::query("UPDATE storage.folders SET name = 'Other' WHERE id = $1").bind(own).execute(&*pool).await.unwrap();
        let again = repo.mount_info(m.mount_id).await.unwrap().unwrap();
        assert_eq!(again.name, "Docs (2)");
    }

    #[tokio::test]
    async fn create_mount_of_shared_drive_root_has_drive_kind() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let (_sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        let m = repo.create_mount(bob_drive, bob_root, sd_root, bob).await.unwrap();
        assert_eq!(m.kind, MountKind::SharedDrive);
        assert_eq!(m.name, "Team");
    }

    #[tokio::test]
    async fn mounts_in_subtree_and_delete() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let parent: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Projects', $1, $2, $3, $3) RETURNING id")
            .bind(bob_root).bind(bob_drive).bind(bob).fetch_one(&*pool).await.unwrap();
        let repo = ShareMountPgRepository::new(pool.clone());
        let m = repo.create_mount(bob_drive, parent, alice.mount_folder_id, bob).await.unwrap();
        let found = repo.mounts_in_subtree(bob_root).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mount_id, m.mount_id);
        repo.delete_mount(m.mount_id).await.unwrap();
        assert!(repo.mount_info(m.mount_id).await.unwrap().is_none());
        assert!(repo.mounts_in_subtree(bob_root).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn caller_can_read_target_follows_grants() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert!(!repo.caller_can_read_target(bob, alice.mount_folder_id).await.unwrap());
        grant_folder(&pool, "user", bob, alice.mount_folder_id, alice.owner_id).await;
        assert!(repo.caller_can_read_target(bob, alice.mount_folder_id).await.unwrap());
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1").bind(alice.mount_folder_id).execute(&*pool).await.unwrap();
        assert!(!repo.caller_can_read_target(bob, alice.mount_folder_id).await.unwrap());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount_pg_repository::repo_tests`
Expected: compile error, `ShareMountPgRepository` not found.

- [ ] **Step 3: Implement the repository**

Above the test modules:

```rust
use crate::common::errors::DomainError;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountKind {
    SharedFolder,
    SharedDrive,
}

impl MountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MountKind::SharedFolder => "shared_folder",
            MountKind::SharedDrive => "shared_drive",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MountRow {
    pub mount_id: Uuid,
    pub mount_drive_id: Uuid,
    pub recipient_id: Uuid,
    pub target_id: Uuid,
    pub target_drive_id: Uuid,
    pub kind: MountKind,
    pub name: String,
}

type MountTuple = (Uuid, Uuid, Uuid, Uuid, Uuid, bool, String);

fn tuple_to_row(t: MountTuple) -> MountRow {
    MountRow {
        mount_id: t.0,
        mount_drive_id: t.1,
        recipient_id: t.2,
        target_id: t.3,
        target_drive_id: t.4,
        kind: if t.5 { MountKind::SharedDrive } else { MountKind::SharedFolder },
        name: t.6,
    }
}

/// Columns of one mount row; `m` = mount row, `t` = target, `d` = mount's drive.
const MOUNT_SELECT: &str = "SELECT m.id, m.drive_id, d.default_for_user, t.id, t.drive_id, \
            (t.parent_id IS NULL) AS is_drive_root, m.name \
       FROM storage.folders m \
       JOIN storage.drives  d ON d.id = m.drive_id \
       JOIN storage.folders t ON t.id = m.mount_target_id ";

/// Caller `$1` may Read folder `t` through a folder grant on an ancestor or drive membership.
const CALLER_READS_T: &str = "( \
        EXISTS (SELECT 1 FROM storage.role_grants g \
                  JOIN storage.folders a ON a.id = g.resource_id \
                 WHERE g.resource_type = 'folder' AND a.lpath @> t.lpath \
                   AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1) \
                     OR (g.subject_type = 'group' AND g.subject_id IN (SELECT storage.caller_group_ids($1))))) \
     OR EXISTS (SELECT 1 FROM storage.role_grants g \
                 WHERE g.resource_type = 'drive' AND g.resource_id = t.drive_id \
                   AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1) \
                     OR (g.subject_type = 'group' AND g.subject_id IN (SELECT storage.caller_group_ids($1))))) \
    )";

pub struct ShareMountPgRepository {
    pool: Arc<PgPool>,
}

impl ShareMountPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn err(op: &str, e: sqlx::Error) -> DomainError {
        DomainError::internal_error("ShareMountDb", format!("{op}: {e}"))
    }

    pub async fn mount_info(&self, folder_id: Uuid) -> Result<Option<MountRow>, DomainError> {
        let sql = format!("{MOUNT_SELECT} WHERE m.id = $1 AND NOT m.is_trashed");
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(folder_id)
            .fetch_optional(&*self.pool)
            .await
            .map(|o| o.map(tuple_to_row))
            .map_err(|e| Self::err("mount_info", e))
    }

    pub async fn existing_mounts(&self, personal_drive_id: Uuid) -> Result<Vec<MountRow>, DomainError> {
        let sql = format!("{MOUNT_SELECT} WHERE m.drive_id = $1 AND NOT m.is_trashed");
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(personal_drive_id)
            .fetch_all(&*self.pool)
            .await
            .map(|v| v.into_iter().map(tuple_to_row).collect())
            .map_err(|e| Self::err("existing_mounts", e))
    }

    pub async fn mounts_in_subtree(&self, folder_id: Uuid) -> Result<Vec<MountRow>, DomainError> {
        let sql = format!(
            "{MOUNT_SELECT} WHERE NOT m.is_trashed \
               AND m.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1)"
        );
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(folder_id)
            .fetch_all(&*self.pool)
            .await
            .map(|v| v.into_iter().map(tuple_to_row).collect())
            .map_err(|e| Self::err("mounts_in_subtree", e))
    }

    pub async fn mounts_for_targets(
        &self,
        personal_drive_id: Uuid,
        target_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Uuid>, DomainError> {
        if target_ids.is_empty() {
            return Ok(HashMap::new());
        }
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT mount_target_id, id FROM storage.folders \
              WHERE drive_id = $1 AND mount_target_id = ANY($2) AND NOT is_trashed",
        )
        .bind(personal_drive_id)
        .bind(target_ids)
        .fetch_all(&*self.pool)
        .await
        .map(|v| v.into_iter().collect())
        .map_err(|e| Self::err("mounts_for_targets", e))
    }

    /// Targets the user should have a mount for: shared drives they hold a role on,
    /// plus top-level incoming folder grants outside their own drive and outside any
    /// drive they are a member of, minus declined targets.
    pub async fn mount_worthy_targets(
        &self,
        user_id: Uuid,
        personal_drive_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            WITH groups AS (
                SELECT ARRAY(SELECT storage.caller_group_ids($1)) AS ids
            ),
            my_drives AS (
                SELECT DISTINCT g.resource_id AS drive_id
                  FROM storage.role_grants g, groups
                 WHERE g.resource_type = 'drive'
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            ),
            drive_targets AS (
                SELECT d.root_folder_id AS target_id
                  FROM storage.drives d
                  JOIN my_drives md ON md.drive_id = d.id
                 WHERE d.kind = 'shared' AND d.root_folder_id IS NOT NULL
            ),
            folder_grants AS (
                SELECT DISTINCT f.id AS target_id, f.lpath, f.drive_id
                  FROM storage.role_grants g
                  CROSS JOIN groups
                  JOIN storage.folders f ON f.id = g.resource_id
                 WHERE g.resource_type = 'folder'
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND g.granted_by <> $1
                   AND NOT f.is_trashed
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            ),
            folder_targets AS (
                SELECT fg.target_id
                  FROM folder_grants fg
                 WHERE fg.drive_id <> $2
                   AND fg.drive_id NOT IN (SELECT drive_id FROM my_drives)
                   AND NOT EXISTS (
                       SELECT 1 FROM folder_grants anc
                        WHERE anc.target_id <> fg.target_id AND anc.lpath @> fg.lpath
                   )
            )
            SELECT target_id FROM drive_targets
            UNION
            SELECT target_id FROM folder_targets
            EXCEPT
            SELECT target_folder_id FROM storage.share_mount_declines WHERE recipient_id = $1
            ORDER BY 1
            "#,
        )
        .bind(user_id)
        .bind(personal_drive_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::err("mount_worthy_targets", e))
    }

    /// Creates the mount row named after the target, retrying with " (2)", " (3)", … on a
    /// sibling-name collision. Returns `already_exists` when a mount of this target exists.
    pub async fn create_mount(
        &self,
        personal_drive_id: Uuid,
        parent_id: Uuid,
        target_id: Uuid,
        user_id: Uuid,
    ) -> Result<MountRow, DomainError> {
        let base: String = sqlx::query_scalar("SELECT name FROM storage.folders WHERE id = $1")
            .bind(target_id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(|e| Self::err("create_mount/name", e))?
            .ok_or_else(|| DomainError::not_found("Folder", target_id.to_string()))?;

        for attempt in 1..=100u32 {
            let name = if attempt == 1 { base.clone() } else { format!("{base} ({attempt})") };
            let res = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by, mount_target_id) \
                 VALUES ($1, $2, $3, $4, $4, $5) RETURNING id",
            )
            .bind(&name)
            .bind(parent_id)
            .bind(personal_drive_id)
            .bind(user_id)
            .bind(target_id)
            .fetch_one(&*self.pool)
            .await;
            match res {
                Ok(id) => {
                    return self
                        .mount_info(id)
                        .await?
                        .ok_or_else(|| DomainError::internal_error("ShareMountDb", "mount vanished after insert"));
                }
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
                    if db.constraint() == Some("idx_folders_one_mount_per_target") {
                        return Err(DomainError::already_exists("ShareMount", target_id.to_string()));
                    }
                    continue;
                }
                Err(e) => return Err(Self::err("create_mount", e)),
            }
        }
        Err(DomainError::conflict("ShareMount", "no free name after 100 attempts"))
    }

    pub async fn delete_mount(&self, mount_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.folders WHERE id = $1 AND mount_target_id IS NOT NULL")
            .bind(mount_id)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("delete_mount", e))
    }

    pub async fn insert_decline(&self, recipient_id: Uuid, target_id: Uuid) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO storage.share_mount_declines (recipient_id, target_folder_id) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(recipient_id)
        .bind(target_id)
        .execute(&*self.pool)
        .await
        .map(|_| ())
        .map_err(|e| Self::err("insert_decline", e))
    }

    pub async fn delete_decline(&self, recipient_id: Uuid, target_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.share_mount_declines WHERE recipient_id = $1 AND target_folder_id = $2")
            .bind(recipient_id)
            .bind(target_id)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("delete_decline", e))
    }

    pub async fn declined_targets(&self, recipient_id: Uuid, target_ids: &[Uuid]) -> Result<HashSet<Uuid>, DomainError> {
        if target_ids.is_empty() {
            return Ok(HashSet::new());
        }
        sqlx::query_scalar::<_, Uuid>(
            "SELECT target_folder_id FROM storage.share_mount_declines \
              WHERE recipient_id = $1 AND target_folder_id = ANY($2)",
        )
        .bind(recipient_id)
        .bind(target_ids)
        .fetch_all(&*self.pool)
        .await
        .map(|v| v.into_iter().collect())
        .map_err(|e| Self::err("declined_targets", e))
    }

    pub async fn caller_can_read_target(&self, user_id: Uuid, target_id: Uuid) -> Result<bool, DomainError> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM storage.folders t WHERE t.id = $2 AND NOT t.is_trashed AND {CALLER_READS_T})"
        );
        sqlx::query_scalar::<_, bool>(&sql)
            .bind(user_id)
            .bind(target_id)
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| Self::err("caller_can_read_target", e))
    }

    pub async fn touch_reconciled(&self, user_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("UPDATE auth.users SET mounts_reconciled_at = NOW() WHERE id = $1")
            .bind(user_id)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("touch_reconciled", e))
    }
}
```

Add `pub use share_mount_pg_repository::{MountKind, MountRow, ShareMountPgRepository};` to `src/infrastructure/repositories/pg/mod.rs` next to the other re-exports.

- [ ] **Step 4: Run the tests**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount_pg_repository`
Expected: all trigger and repo tests pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add src/infrastructure/repositories/pg/
git commit -m "feat(share-mounts): ShareMountPgRepository with reconcile set and suffixed create"
```

---

### Task 3: `ShareMountService` (lifecycle + resolution) and login hook

**Files:**
- Create: `src/application/services/share_mount_service.rs`
- Modify: `src/application/services/mod.rs` (add `pub mod share_mount_service;`)

**Interfaces:**
- Consumes: `ShareMountPgRepository` (Task 2), `DrivePgRepository::find_default_for_user`, `PgAclEngine` (`check`, `require`), `FolderDbRepository::move_folder`, `SubjectGroupService::list_transitive_users`.
- Produces:

```rust
pub struct ResolvedMount { pub mount_id: Uuid, pub target_id: Uuid, pub target_drive_id: Uuid, pub kind: MountKind, pub name: String }

pub struct ShareMountService { .. }
impl ShareMountService {
    pub fn new(repo: Arc<ShareMountPgRepository>, drive_repo: Arc<DrivePgRepository>, authz: Arc<PgAclEngine>, folder_repo: Arc<FolderDbRepository>, share_folder: Option<String>) -> Self;
    pub fn set_group_service(&self, groups: Arc<SubjectGroupService>);   // OnceLock; call once from DI
    pub async fn reconcile(&self, user_id: Uuid) -> Result<(), DomainError>;
    pub async fn on_folder_granted(&self, subject: Subject, folder_id: Uuid) -> Result<(), DomainError>;
    pub async fn on_drive_member_set(&self, subject: Subject, drive_id: Uuid) -> Result<(), DomainError>;
    pub async fn on_subject_revoked(&self, subject: Subject) -> Result<(), DomainError>;   // reconcile affected users
    pub async fn resolve(&self, caller: Subject, folder_id: &str) -> Result<Option<ResolvedMount>, DomainError>;
    pub async fn is_mount(&self, folder_id: &str) -> Result<bool, DomainError>;
    pub async fn unmount(&self, caller_id: Uuid, mount_id: Uuid) -> Result<(), DomainError>;
    pub async fn remount(&self, caller_id: Uuid, target_id: Uuid) -> Result<ResolvedMount, DomainError>;
    pub async fn relocate_mounts_under(&self, caller_id: Uuid, folder_id: Uuid) -> Result<Vec<ResolvedMount>, DomainError>;
    pub async fn mount_ids_for_targets(&self, caller_id: Uuid, targets: &[Uuid]) -> Result<HashMap<Uuid, Uuid>, DomainError>;
    pub async fn declined_for(&self, caller_id: Uuid, targets: &[Uuid]) -> Result<HashSet<Uuid>, DomainError>;
}
pub struct ShareMountLoginHook(pub Arc<ShareMountService>);   // UserLifecycleHook: on_user_login → reconcile
```

- [ ] **Step 1: Write failing service tests**

In the new file, under `#[cfg(all(test, integration_tests))] mod it`:

```rust
#[cfg(all(test, integration_tests))]
mod it {
    use super::*;
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{make_shared_drive, personal_root};
    use crate::infrastructure::repositories::pg::{DrivePgRepository, FileBlobReadRepository, FolderDbRepository, ShareMountPgRepository, SubjectGroupPgRepository};
    use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
    use crate::mount_it_support::{fresh_db, make_user, provision_folder};
    use crate::domain::services::authorization::{Permission, Resource, Role, Subject};
    use std::sync::atomic::AtomicBool;

    fn engine(pool: &Arc<sqlx::PgPool>) -> Arc<PgAclEngine> {
        Arc::new(PgAclEngine::new(
            pool.clone(),
            Arc::new(FolderDbRepository::new(pool.clone())),
            Arc::new(FileBlobReadRepository::new_stub()),
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    fn service(pool: &Arc<sqlx::PgPool>, share_folder: Option<&str>) -> ShareMountService {
        ShareMountService::new(
            Arc::new(ShareMountPgRepository::new(pool.clone())),
            Arc::new(DrivePgRepository::new(pool.clone())),
            engine(pool),
            Arc::new(FolderDbRepository::new(pool.clone())),
            share_folder.map(str::to_owned),
        )
    }

    #[tokio::test]
    async fn reconcile_creates_and_removes_mounts() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let svc = service(&pool, None);
        let authz = engine(&pool);
        authz.set_role(alice.owner_id, Subject::User(bob), Role::Viewer, Resource::Folder(alice.mount_folder_id), None).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let mounts = svc.repo.existing_mounts(bob_drive).await.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target_id, alice.mount_folder_id);
        let parent: Option<Uuid> = sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE id = $1").bind(mounts[0].mount_id).fetch_one(&*pool).await.unwrap();
        assert_eq!(parent, Some(bob_root));
        // revoke -> reconcile removes
        authz.clear_role(Subject::User(bob), Resource::Folder(alice.mount_folder_id)).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        assert!(svc.repo.existing_mounts(bob_drive).await.unwrap().is_empty());
        // idempotent on empty
        svc.reconcile(bob).await.unwrap();
    }

    #[tokio::test]
    async fn reconcile_uses_share_folder_and_creates_it_once() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let svc = service(&pool, Some("Shared"));
        engine(&pool).set_role(alice.owner_id, Subject::User(bob), Role::Viewer, Resource::Folder(alice.mount_folder_id), None).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let shared_dirs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage.folders WHERE parent_id = $1 AND name = 'Shared' AND mount_target_id IS NULL")
            .bind(bob_root).fetch_one(&*pool).await.unwrap();
        assert_eq!(shared_dirs, 1);
        let m = &svc.repo.existing_mounts(bob_drive).await.unwrap()[0];
        let parent_name: String = sqlx::query_scalar("SELECT p.name FROM storage.folders m JOIN storage.folders p ON p.id = m.parent_id WHERE m.id = $1")
            .bind(m.mount_id).fetch_one(&*pool).await.unwrap();
        assert_eq!(parent_name, "Shared");
    }

    #[tokio::test]
    async fn group_grant_mounts_for_members_but_not_granter() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let group: Uuid = sqlx::query_scalar("INSERT INTO auth.subject_groups (name, created_by) VALUES ('team', $1) RETURNING id")
            .bind(alice.owner_id).fetch_one(&*pool).await.unwrap();
        for u in [alice.owner_id, bob] {
            sqlx::query("INSERT INTO auth.subject_group_members (group_id, member_user_id) VALUES ($1, $2)")
                .bind(group).bind(u).execute(&*pool).await.unwrap();
        }
        let svc = service(&pool, None);
        engine(&pool).set_role(alice.owner_id, Subject::Group(group), Role::Viewer, Resource::Folder(alice.mount_folder_id), None).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        svc.reconcile(alice.owner_id).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let (alice_drive, _) = personal_root(&pool, alice.owner_id).await;
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 1);
        assert!(svc.repo.existing_mounts(alice_drive).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_only_for_recipient_with_read_on_target() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let carol = make_user(&pool, "carol").await;
        let svc = service(&pool, None);
        let authz = engine(&pool);
        authz.set_role(alice.owner_id, Subject::User(bob), Role::Viewer, Resource::Folder(alice.mount_folder_id), None).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let m = svc.repo.existing_mounts(bob_drive).await.unwrap().remove(0);
        // plain folder -> None
        assert!(svc.resolve(Subject::User(bob), &bob_root.to_string()).await.unwrap().is_none());
        // recipient -> Some(target)
        let r = svc.resolve(Subject::User(bob), &m.mount_id.to_string()).await.unwrap().unwrap();
        assert_eq!(r.target_id, alice.mount_folder_id);
        // bob shares his root with carol; carol still cannot resolve the mount (R0)
        authz.set_role(bob, Subject::User(carol), Role::Viewer, Resource::Folder(bob_root), None).await.unwrap();
        let err = svc.resolve(Subject::User(carol), &m.mount_id.to_string()).await.unwrap_err();
        assert_eq!(err.kind(), crate::common::errors::ErrorKind::NotFound);
        // grant revoked but row still present -> NotFound for bob too
        authz.clear_role(Subject::User(bob), Resource::Folder(alice.mount_folder_id)).await.unwrap();
        authz.invalidate_cascade_grant_cache_all().await;
        let err = svc.resolve(Subject::User(bob), &m.mount_id.to_string()).await.unwrap_err();
        assert_eq!(err.kind(), crate::common::errors::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn unmount_declines_and_remount_restores_drive_mount_not_declinable() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let svc = service(&pool, None);
        let authz = engine(&pool);
        authz.set_role(alice.owner_id, Subject::User(bob), Role::Viewer, Resource::Folder(alice.mount_folder_id), None).await.unwrap();
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        authz.set_role(alice.owner_id, Subject::User(bob), Role::Viewer, Resource::Drive(sd), None).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let mounts = svc.repo.existing_mounts(bob_drive).await.unwrap();
        let folder_mount = mounts.iter().find(|m| m.target_id == alice.mount_folder_id).unwrap().clone();
        let drive_mount = mounts.iter().find(|m| m.target_id == sd_root).unwrap().clone();
        svc.unmount(bob, folder_mount.mount_id).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 1, "declined target not recreated");
        let err = svc.unmount(bob, drive_mount.mount_id).await.unwrap_err();
        assert_eq!(err.kind(), crate::common::errors::ErrorKind::Conflict);
        let r = svc.remount(bob, alice.mount_folder_id).await.unwrap();
        assert_eq!(r.target_id, alice.mount_folder_id);
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 2);
        // carol cannot unmount bob's mount
        let carol = make_user(&pool, "carol").await;
        let err = svc.unmount(carol, r.mount_id).await.unwrap_err();
        assert_eq!(err.kind(), crate::common::errors::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn relocate_moves_mounts_to_root_with_suffix() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let svc = service(&pool, None);
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let projects: Uuid = sqlx::query_scalar("INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Projects', $1, $2, $3, $3) RETURNING id")
            .bind(bob_root).bind(bob_drive).bind(bob).fetch_one(&*pool).await.unwrap();
        sqlx::query("INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Docs', $1, $2, $3, $3)")
            .bind(bob_root).bind(bob_drive).bind(bob).execute(&*pool).await.unwrap();
        let m = svc.repo.create_mount(bob_drive, projects, alice.mount_folder_id, bob).await.unwrap();
        assert_eq!(m.name, "Docs");
        let moved = svc.relocate_mounts_under(bob, projects).await.unwrap();
        assert_eq!(moved.len(), 1);
        let (parent, name): (Option<Uuid>, String) = sqlx::query_as("SELECT parent_id, name FROM storage.folders WHERE id = $1")
            .bind(m.mount_id).fetch_one(&*pool).await.unwrap();
        assert_eq!(parent, Some(bob_root));
        assert_eq!(name, "Docs (2)");
    }
}
```

Make `trigger_tests` in the repository file `pub(crate) mod trigger_tests` (and its two helpers `pub(crate)`) so this module can import them.

- [ ] **Step 2: Run to verify failure**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount_service`
Expected: compile error, module missing.

- [ ] **Step 3: Implement the service**

```rust
//! Share mount lifecycle and resolution (docs/plan/share-mounts.md § Lifecycle, § Resolution).

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::application::services::subject_group_service::SubjectGroupService;
use crate::common::errors::DomainError;
use crate::domain::entities::user::User;
use crate::domain::repositories::drive_repository::{DriveRepository, DriveRepositoryError};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::domain::services::path_service::validate_storage_name;
use crate::infrastructure::repositories::pg::share_mount_pg_repository::{MountKind, MountRow};
use crate::infrastructure::repositories::pg::{DrivePgRepository, FolderDbRepository, ShareMountPgRepository};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ResolvedMount {
    pub mount_id: Uuid,
    pub target_id: Uuid,
    pub target_drive_id: Uuid,
    pub kind: MountKind,
    pub name: String,
}

impl From<MountRow> for ResolvedMount {
    fn from(m: MountRow) -> Self {
        Self { mount_id: m.mount_id, target_id: m.target_id, target_drive_id: m.target_drive_id, kind: m.kind, name: m.name }
    }
}

pub struct ShareMountService {
    pub(crate) repo: Arc<ShareMountPgRepository>,
    drive_repo: Arc<DrivePgRepository>,
    authz: Arc<PgAclEngine>,
    folder_repo: Arc<FolderDbRepository>,
    groups: std::sync::OnceLock<Arc<SubjectGroupService>>,
    /// Relative path under the personal root where new mounts land; `None` = root.
    share_folder: Option<String>,
}

impl ShareMountService {
    pub fn new(
        repo: Arc<ShareMountPgRepository>,
        drive_repo: Arc<DrivePgRepository>,
        authz: Arc<PgAclEngine>,
        folder_repo: Arc<FolderDbRepository>,
        share_folder: Option<String>,
    ) -> Self {
        let share_folder = share_folder
            .map(|s| s.trim_matches('/').to_owned())
            .filter(|s| !s.is_empty() && validate_storage_name(s).is_ok());
        Self { repo, drive_repo, authz, folder_repo, groups: std::sync::OnceLock::new(), share_folder }
    }

    /// Group expansion for group-subject grants. Set once by DI after the group service exists.
    pub fn set_group_service(&self, groups: Arc<SubjectGroupService>) {
        let _ = self.groups.set(groups);
    }

    /// `(personal_drive_id, root_folder_id)` or `None` for users without a personal drive.
    async fn personal_drive(&self, user_id: Uuid) -> Result<Option<(Uuid, Uuid)>, DomainError> {
        match self.drive_repo.find_default_for_user(user_id).await {
            Ok(d) => Ok(Some((d.drive.id, d.drive.root_folder_id))),
            Err(DriveRepositoryError::NotFound(_)) => Ok(None),
            Err(e) => Err(DomainError::internal_error("ShareMount", format!("default drive: {e}"))),
        }
    }

    /// Folder new mounts are created in: the share folder (created on demand) or the root.
    async fn target_parent(&self, user_id: Uuid, root: Uuid) -> Result<Uuid, DomainError> {
        let Some(name) = &self.share_folder else { return Ok(root) };
        let existing = self.folder_repo.list_folders(Some(&root.to_string())).await?;
        if let Some(f) = existing.iter().find(|f| f.name() == name) {
            return Uuid::parse_str(f.id()).map_err(|_| DomainError::internal_error("ShareMount", "bad folder id"));
        }
        let created = self.folder_repo.create_folder(name.clone(), Some(root.to_string()), user_id).await?;
        Uuid::parse_str(created.id()).map_err(|_| DomainError::internal_error("ShareMount", "bad folder id"))
    }

    pub async fn reconcile(&self, user_id: Uuid) -> Result<(), DomainError> {
        let Some((drive_id, root)) = self.personal_drive(user_id).await? else { return Ok(()) };
        let wanted: HashSet<Uuid> = self.repo.mount_worthy_targets(user_id, drive_id).await?.into_iter().collect();
        let existing = self.repo.existing_mounts(drive_id).await?;
        let have: HashSet<Uuid> = existing.iter().map(|m| m.target_id).collect();

        for m in existing.iter().filter(|m| !wanted.contains(&m.target_id)) {
            self.repo.delete_mount(m.mount_id).await?;
            tracing::info!(target: "audit", event = "share_mount.removed", reason = "no_longer_granted",
                recipient_id = %user_id, mount_id = %m.mount_id, target_id = %m.target_id, "🔌 share mount removed");
        }
        let missing: Vec<Uuid> = wanted.difference(&have).copied().collect();
        if !missing.is_empty() {
            let parent = self.target_parent(user_id, root).await?;
            for target in missing {
                match self.repo.create_mount(drive_id, parent, target, user_id).await {
                    Ok(m) => tracing::info!(target: "audit", event = "share_mount.created", reason = "reconcile",
                        recipient_id = %user_id, mount_id = %m.mount_id, target_id = %target, kind = m.kind.as_str(), "🔌 share mount created"),
                    Err(e) if e.kind() == crate::common::errors::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
        }
        self.repo.touch_reconciled(user_id).await
    }

    async fn users_of(&self, subject: Subject) -> Result<Vec<Uuid>, DomainError> {
        match subject {
            Subject::User(u) => Ok(vec![u]),
            Subject::Group(g) => match self.groups.get() {
                Some(gs) => gs.list_transitive_users(g).await,
                None => Ok(Vec::new()),
            },
            Subject::Token(_) => Ok(Vec::new()),
        }
    }

    pub async fn on_folder_granted(&self, subject: Subject, _folder_id: Uuid) -> Result<(), DomainError> {
        for u in self.users_of(subject).await? {
            self.reconcile(u).await?;
        }
        Ok(())
    }

    pub async fn on_drive_member_set(&self, subject: Subject, _drive_id: Uuid) -> Result<(), DomainError> {
        self.on_folder_granted(subject, Uuid::nil()).await
    }

    pub async fn on_subject_revoked(&self, subject: Subject) -> Result<(), DomainError> {
        self.on_folder_granted(subject, Uuid::nil()).await
    }

    pub async fn is_mount(&self, folder_id: &str) -> Result<bool, DomainError> {
        let Ok(id) = Uuid::parse_str(folder_id) else { return Ok(false) };
        Ok(self.repo.mount_info(id).await?.is_some())
    }

    /// R0 + R1: `Ok(None)` for a plain folder; `Ok(Some)` when the caller is the recipient and
    /// may Read the target; `NotFound` otherwise.
    pub async fn resolve(&self, caller: Subject, folder_id: &str) -> Result<Option<ResolvedMount>, DomainError> {
        let Ok(id) = Uuid::parse_str(folder_id) else { return Ok(None) };
        let Some(m) = self.repo.mount_info(id).await? else { return Ok(None) };
        let denied = |reason: &'static str| {
            tracing::info!(target: "audit", event = "share_mount.denied", reason,
                caller = ?caller, mount_id = %m.mount_id, target_id = %m.target_id, "👮🏻‍♂️ share mount access denied");
            DomainError::not_found("Folder", folder_id)
        };
        if caller.user_id() != Some(m.recipient_id) {
            return Err(denied("not_recipient"));
        }
        if !self.authz.check(caller, Permission::Read, Resource::Folder(m.target_id)).await? {
            return Err(denied("target_unreadable"));
        }
        Ok(Some(m.into()))
    }

    pub async fn unmount(&self, caller_id: Uuid, mount_id: Uuid) -> Result<(), DomainError> {
        let Some(m) = self.repo.mount_info(mount_id).await? else {
            return Err(DomainError::not_found("Folder", mount_id.to_string()));
        };
        if m.recipient_id != caller_id {
            tracing::info!(target: "audit", event = "share_mount.denied", reason = "not_recipient",
                caller_id = %caller_id, mount_id = %mount_id, "👮🏻‍♂️ unmount denied");
            return Err(DomainError::not_found("Folder", mount_id.to_string()));
        }
        if m.kind == MountKind::SharedDrive {
            tracing::info!(target: "audit", event = "share_mount.rejected", reason = "drive_mount_not_declinable",
                caller_id = %caller_id, mount_id = %mount_id, "👮🏻‍♂️ unmount rejected");
            return Err(DomainError::conflict("ShareMount", "a shared drive mount cannot be removed"));
        }
        self.repo.insert_decline(caller_id, m.target_id).await?;
        self.repo.delete_mount(mount_id).await?;
        tracing::info!(target: "audit", event = "share_mount.removed", reason = "declined",
            recipient_id = %caller_id, mount_id = %mount_id, target_id = %m.target_id, "🔌 share mount declined");
        Ok(())
    }

    pub async fn remount(&self, caller_id: Uuid, target_id: Uuid) -> Result<ResolvedMount, DomainError> {
        let Some((drive_id, root)) = self.personal_drive(caller_id).await? else {
            return Err(DomainError::not_found("Folder", target_id.to_string()));
        };
        self.repo.delete_decline(caller_id, target_id).await?;
        let wanted = self.repo.mount_worthy_targets(caller_id, drive_id).await?;
        if !wanted.contains(&target_id) {
            tracing::info!(target: "audit", event = "share_mount.rejected", reason = "not_mount_worthy",
                caller_id = %caller_id, target_id = %target_id, "👮🏻‍♂️ remount rejected");
            return Err(DomainError::not_found("Folder", target_id.to_string()));
        }
        if let Some(m) = self.repo.existing_mounts(drive_id).await?.into_iter().find(|m| m.target_id == target_id) {
            return Ok(m.into());
        }
        let parent = self.target_parent(caller_id, root).await?;
        let m = self.repo.create_mount(drive_id, parent, target_id, caller_id).await?;
        tracing::info!(target: "audit", event = "share_mount.created", reason = "remount",
            recipient_id = %caller_id, mount_id = %m.mount_id, target_id = %target_id, "🔌 share mount created");
        Ok(m.into())
    }

    /// R2: move every mount under `folder_id` to the share folder / root before the
    /// folder is trashed. Only the recipient's own mounts are touched.
    pub async fn relocate_mounts_under(&self, caller_id: Uuid, folder_id: Uuid) -> Result<Vec<ResolvedMount>, DomainError> {
        let Some((_drive_id, root)) = self.personal_drive(caller_id).await? else { return Ok(Vec::new()) };
        let mounts = self.repo.mounts_in_subtree(folder_id).await?;
        let mut moved = Vec::new();
        if mounts.is_empty() {
            return Ok(moved);
        }
        let parent = self.target_parent(caller_id, root).await?;
        for m in mounts.into_iter().filter(|m| m.recipient_id == caller_id && m.mount_id != folder_id) {
            let mut name = m.name.clone();
            let mut attempt = 1u32;
            loop {
                if name != m.name {
                    self.folder_repo.rename_folder(&m.mount_id.to_string(), name.clone(), caller_id).await?;
                }
                match self.folder_repo.move_folder(&m.mount_id.to_string(), Some(&parent.to_string()), caller_id).await {
                    Ok(_) => break,
                    Err(e) if e.kind() == crate::common::errors::ErrorKind::AlreadyExists && attempt < 100 => {
                        attempt += 1;
                        name = format!("{} ({attempt})", m.name);
                    }
                    Err(e) => return Err(e),
                }
            }
            tracing::info!(target: "audit", event = "share_mount.relocated", reason = "ancestor_trashed",
                recipient_id = %caller_id, mount_id = %m.mount_id, from = %folder_id, to = %parent, "🔌 share mount relocated");
            moved.push(ResolvedMount { mount_id: m.mount_id, target_id: m.target_id, target_drive_id: m.target_drive_id, kind: m.kind, name });
        }
        Ok(moved)
    }

    pub async fn mount_ids_for_targets(&self, caller_id: Uuid, targets: &[Uuid]) -> Result<HashMap<Uuid, Uuid>, DomainError> {
        let Some((drive_id, _)) = self.personal_drive(caller_id).await? else { return Ok(HashMap::new()) };
        self.repo.mounts_for_targets(drive_id, targets).await
    }

    pub async fn declined_for(&self, caller_id: Uuid, targets: &[Uuid]) -> Result<HashSet<Uuid>, DomainError> {
        self.repo.declined_targets(caller_id, targets).await
    }
}

/// Reconciles the user's mounts on every login (safety net for missed events).
pub struct ShareMountLoginHook(pub Arc<ShareMountService>);

#[async_trait]
impl UserLifecycleHook for ShareMountLoginHook {
    async fn on_user_login(&self, user: &User) -> Result<(), DomainError> {
        self.0.reconcile(user.id()).await
    }
    async fn on_user_logout(&self, _user: &User, _reason: LogoutReason) -> Result<(), DomainError> { Ok(()) }
    async fn on_user_deleted(&self, _user: &User, _mode: DeletionMode, _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), DomainError> { Ok(()) }
}
```

Check `UserLifecycleHook` for other required methods (`on_user_created`, `on_user_upgraded`, …) in `src/application/ports/user_lifecycle.rs` and implement each as a no-op `Ok(())`. Check `User::id()` return type (`Uuid` vs `&str`) and adapt. `move_folder` on a name collision: the repository maps 23505 to `already_exists` in `create_folder`; verify `move_folder` does the same (grep `23505` around line 789-880); if it does not, add the same mapping there.

- [ ] **Step 4: Run the tests**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add src/application/services/share_mount_service.rs src/application/services/mod.rs src/infrastructure/repositories/pg/
git commit -m "feat(share-mounts): ShareMountService reconcile, resolve, unmount, remount, relocate"
```

---

### Task 4: Feature flag, config, DI wiring

**Files:**
- Modify: `src/common/config.rs` (`FeaturesConfig` near `:2322`, default near `:2903`, env parse near `:4149`)
- Modify: `src/interfaces/api/handlers/config_handler.rs:102-170`
- Modify: `src/common/di.rs` (`ApplicationServices` `:3696`, `AppState` `:3740`, assembly `:2495`, lifecycle hooks `:2195`, service builders `:752`, `:1075`)
- Modify: `frontend/src/lib/api/types.ts:1013-1034`, `frontend/src/lib/stores/serverConfig.svelte.ts:34-45`

**Interfaces:**
- Produces: `FeaturesConfig.enable_share_mounts: bool`, `FeaturesConfig.share_mount_folder: Option<String>`, `FeaturesDto.share_mounts`, `ServerFeatures.share_mounts`, `AppState.share_mount_service: Option<Arc<ShareMountService>>`, `FolderService::with_share_mounts`, `TrashService::with_share_mounts`, `FileManagementService::with_share_mounts`.

- [ ] **Step 1: Config**

In `FeaturesConfig` add:

```rust
    /// Mount granted folders and shared drives into the recipient's personal drive.
    /// Env: `OXICLOUD_ENABLE_SHARE_MOUNTS` (default true).
    pub enable_share_mounts: bool,
    /// Folder under the personal root where new mounts land; empty = root.
    /// Env: `OXICLOUD_SHARE_MOUNT_FOLDER`.
    pub share_mount_folder: Option<String>,
```

Defaults: `enable_share_mounts: false, share_mount_folder: None,` (review A5: opt-in for the first release). Env parsing, next to `OXICLOUD_ENABLE_EXTERNAL_MOUNTS`:

```rust
        if let Ok(v) = env::var("OXICLOUD_ENABLE_SHARE_MOUNTS").map(|v| v.parse::<bool>())
            && let Ok(val) = v
        {
            config.features.enable_share_mounts = val;
        }
        if let Ok(v) = env::var("OXICLOUD_SHARE_MOUNT_FOLDER") {
            let v = v.trim().trim_matches('/').to_owned();
            config.features.share_mount_folder = (!v.is_empty()).then_some(v);
        }
```

Add a unit test next to the existing `FeaturesConfig` tests (grep `enable_external_mounts` in the `#[cfg(test)]` module of `config.rs`) asserting the default is `true` and that `OXICLOUD_SHARE_MOUNT_FOLDER="/Shared/"` parses to `Some("Shared")`. If config tests use a helper that sets env vars, follow it; otherwise construct via `AppConfig::default()` and mutate.

Also document both variables in `example.env` next to `OXICLOUD_ENABLE_EXTERNAL_MOUNTS`.

- [ ] **Step 2: `FeaturesDto` and `/api/config`**

Add `pub share_mounts: bool,` to `FeaturesDto` with a doc line, and `share_mounts: f.enable_share_mounts,` in `get_config`. Frontend: add `share_mounts: boolean;` to `ServerFeatures` and `share_mounts: true` to `DEFAULT_FEATURES`.

- [ ] **Step 3: Builder methods on the three services**

`FolderService`: add field `share_mounts: Option<Arc<ShareMountService>>` (init `None` in `new`) and

```rust
    pub fn with_share_mounts(mut self, svc: Arc<ShareMountService>) -> Self {
        self.share_mounts = Some(svc);
        self
    }
```

Same for `TrashService` (`src/application/services/trash_service.rs`, next to `with_message_bus`) and `FileManagementService` (`src/application/services/file_management_service.rs`, next to `with_mount_router`).

- [ ] **Step 4: DI**

In `di.rs`, before `create_application_services` is called (around line 1943 where `trash_service` is built), construct:

```rust
        let share_mount_service: Option<Arc<crate::application::services::share_mount_service::ShareMountService>> =
            if self.config.features.enable_share_mounts {
                Some(Arc::new(
                    crate::application::services::share_mount_service::ShareMountService::new(
                        Arc::new(crate::infrastructure::repositories::pg::ShareMountPgRepository::new(pool.clone())),
                        drive_repo.clone(),
                        authorization.clone(),
                        repos.folder_repository.clone(),
                        self.config.features.share_mount_folder.clone(),
                    ),
                ))
            } else {
                tracing::info!("Share mounts are disabled in configuration");
                None
            };
```

Pass it into `create_trash_service` and `create_application_services` (add a parameter `share_mount_service: Option<Arc<ShareMountService>>` to both) and chain `.with_share_mounts(svc.clone())` on `FolderService`, `TrashService`, `FileManagementService` when `Some`. The group service is built later (`:2568`); right after it exists call `svc.set_group_service(subject_group_service.clone())` (Task 3's `OnceLock` setter), so construction order does not matter.

Add `pub share_mount_service: Option<Arc<ShareMountService>>` to `AppState` and set it in the assembly at `:2495`. Register the login hook in the `UserLifecycleService` builder chain (`:2195`):

```rust
            if let Some(svc) = &share_mount_service {
                user_lifecycle_builder = user_lifecycle_builder.with_hook(Arc::new(
                    crate::application::services::share_mount_service::ShareMountLoginHook(svc.clone()),
                ));
            }
```

(Adapt to how the builder variable is actually threaded; it is `let mut user_lifecycle_builder = ...` at `:2194`.)

- [ ] **Step 5: Build and run existing tests**

Run: `cargo build && cargo test --workspace`
Expected: green; `GET /api/config` now carries `share_mounts`.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add -A src/common/config.rs src/common/di.rs src/interfaces/api/handlers/config_handler.rs src/application/services/ example.env frontend/src/lib/api/types.ts frontend/src/lib/stores/serverConfig.svelte.ts
git commit -m "feat(share-mounts): feature flag, share folder config, DI wiring, login hook"
```

---

### Task 5: `MountDto`, listing columns, and the R0 listing filter

**Files:**
- Modify: `src/application/dtos/folder_dto.rs:39-150` (DTO), `:245-300` (row)
- Modify: `src/infrastructure/repositories/pg/folder_db_repository.rs:1707-2060` (`list_resources_paged`), `:1120-1135` (`list_subtree_folders`)
- Modify: `src/application/services/folder_service.rs:1419-1428`
- Modify: `src/interfaces/api/handlers/folder_handler.rs:641-700`
- Modify: `frontend/src/lib/api/types.ts:14-71`
- Test: `src/infrastructure/repositories/pg/folder_db_repository.rs` (new `#[cfg(all(test, integration_tests))] mod share_mount_listing_tests`)

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountDto {
    pub kind: String,                 // "shared_folder" | "shared_drive" | "external"
    #[serde(skip_serializing_if = "Option::is_none")] pub target_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")] pub target_drive_id: Option<Uuid>,
}
// FolderDto gains: #[serde(default, skip_serializing_if = "Option::is_none")] pub mount: Option<MountDto>,
// FolderResourceRow gains: pub mount_target_id: Option<Uuid>, pub mount_target_drive_id: Option<Uuid>, pub mount_kind: Option<String>,
// FolderDbRepository::list_resources_paged gains a trailing `show_mounts: bool` parameter.
```

- [ ] **Step 1: Write the failing listing tests**

In `folder_db_repository.rs` add:

```rust
#[cfg(all(test, integration_tests))]
mod share_mount_listing_tests {
    use super::*;
    use crate::application::dtos::folder_dto::ListResourcesOptions;
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::personal_root;
    use crate::infrastructure::repositories::pg::ShareMountPgRepository;
    use crate::mount_it_support::{fresh_db, make_user, provision_folder};
    use crate::domain::services::authorization::Subject;

    async fn list_ids(repo: &FolderDbRepository, parent: Uuid, caller: Subject, show_mounts: bool) -> Vec<Uuid> {
        repo.list_resources_paged(parent, caller, 100, None, "name", None, false, show_mounts)
            .await.unwrap().into_iter().map(|r| r.id).collect()
    }

    #[tokio::test]
    async fn r0_mount_visible_only_to_recipient_with_readable_target() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let carol = make_user(&pool, "carol").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        sqlx::query("INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by) VALUES ('user', $1, 'folder', $2, 'viewer', $3)")
            .bind(bob).bind(alice.mount_folder_id).bind(alice.owner_id).execute(&*pool).await.unwrap();
        let mounts = ShareMountPgRepository::new(pool.clone());
        let m = mounts.create_mount(bob_drive, bob_root, alice.mount_folder_id, bob).await.unwrap();
        let repo = FolderDbRepository::new(pool.clone());
        assert_eq!(list_ids(&repo, bob_root, Subject::User(bob), true).await, vec![m.mount_id]);
        assert!(list_ids(&repo, bob_root, Subject::User(bob), false).await.is_empty(), "flag off hides mounts");
        // carol granted read on bob's root still does not see the mount
        sqlx::query("INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by) VALUES ('user', $1, 'folder', $2, 'viewer', $3)")
            .bind(carol).bind(bob_root).bind(bob).execute(&*pool).await.unwrap();
        assert!(list_ids(&repo, bob_root, Subject::User(carol), true).await.is_empty());
        // token caller never sees it
        assert!(list_ids(&repo, bob_root, Subject::Token(Uuid::new_v4()), true).await.is_empty());
        // target trashed -> hidden; restored -> back
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1").bind(alice.mount_folder_id).execute(&*pool).await.unwrap();
        assert!(list_ids(&repo, bob_root, Subject::User(bob), true).await.is_empty());
        sqlx::query("UPDATE storage.folders SET is_trashed = FALSE WHERE id = $1").bind(alice.mount_folder_id).execute(&*pool).await.unwrap();
        assert_eq!(list_ids(&repo, bob_root, Subject::User(bob), true).await, vec![m.mount_id]);
        // grant revoked -> hidden
        sqlx::query("DELETE FROM storage.role_grants WHERE subject_id = $1 AND resource_id = $2").bind(bob).bind(alice.mount_folder_id).execute(&*pool).await.unwrap();
        assert!(list_ids(&repo, bob_root, Subject::User(bob), true).await.is_empty());
    }

    #[tokio::test]
    async fn listing_row_carries_mount_columns() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        sqlx::query("INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by) VALUES ('user', $1, 'folder', $2, 'viewer', $3)")
            .bind(bob).bind(alice.mount_folder_id).bind(alice.owner_id).execute(&*pool).await.unwrap();
        ShareMountPgRepository::new(pool.clone()).create_mount(bob_drive, bob_root, alice.mount_folder_id, bob).await.unwrap();
        let repo = FolderDbRepository::new(pool.clone());
        let rows = repo.list_resources_paged(bob_root, Subject::User(bob), 10, None, "name", None, false, true).await.unwrap();
        assert_eq!(rows[0].mount_target_id, Some(alice.mount_folder_id));
        assert_eq!(rows[0].mount_target_drive_id, Some(alice.drive_id));
        assert_eq!(rows[0].mount_kind.as_deref(), Some("shared_folder"));
        let _ = ListResourcesOptions { limit: 1, cursor: None, order_by: "name", kinds: None, reverse: false };
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount_listing_tests`
Expected: compile error (extra argument / missing fields).

- [ ] **Step 3: DTO changes**

In `folder_dto.rs` add `MountDto` (above `FolderDto`), add the `mount` field to `FolderDto` (set `mount: None` in `From<Folder>`, in `redacted_for_token` keep `None`, and in every other struct-literal constructor the compiler reports), and the three fields on `FolderResourceRow`. Grep `FolderDto {` across `src/` and add `mount: None,` to each literal (handler listing mapping at `folder_handler.rs:651`, `mount_dto.rs`, tests).

- [ ] **Step 4: Listing SQL**

In `list_resources_paged`: add parameter `show_mounts: bool`. Folder branch SELECT: after `0::int AS folder_first` add

```sql
                , f.mount_target_id,
                (SELECT t.drive_id FROM storage.folders t WHERE t.id = f.mount_target_id) AS mount_target_drive_id,
                (SELECT CASE WHEN t.parent_id IS NULL THEN 'shared_drive' ELSE 'shared_folder' END
                   FROM storage.folders t WHERE t.id = f.mount_target_id)                 AS mount_kind
```

File branch: `, NULL::uuid AS mount_target_id, NULL::uuid AS mount_target_drive_id, NULL::text AS mount_kind`. Folder branch WHERE: append

```sql
              AND (
                  f.mount_target_id IS NULL
                  OR (
                      $9::bool
                      AND EXISTS (SELECT 1 FROM storage.drives d
                                   WHERE d.id = f.drive_id AND d.default_for_user = $7::uuid)
                      AND EXISTS (
                          SELECT 1 FROM storage.folders t
                           WHERE t.id = f.mount_target_id AND NOT t.is_trashed
                             AND (
                                 EXISTS (SELECT 1 FROM storage.role_grants g
                                           JOIN storage.folders a ON a.id = g.resource_id
                                          WHERE g.resource_type = 'folder' AND a.lpath @> t.lpath
                                            AND (g.expires_at IS NULL OR g.expires_at > NOW())
                                            AND ((g.subject_type = 'user'  AND g.subject_id = $7::uuid)
                                              OR (g.subject_type = 'group' AND g.subject_id IN (SELECT storage.caller_group_ids($7::uuid)))))
                              OR EXISTS (SELECT 1 FROM storage.role_grants g
                                          WHERE g.resource_type = 'drive' AND g.resource_id = t.drive_id
                                            AND (g.expires_at IS NULL OR g.expires_at > NOW())
                                            AND ((g.subject_type = 'user'  AND g.subject_id = $7::uuid)
                                              OR (g.subject_type = 'group' AND g.subject_id IN (SELECT storage.caller_group_ids($7::uuid)))))
                             )
                      )
                  )
              )
```

`$7` is already `caller_id` (`Uuid::nil()` for tokens, which matches no drive). Bind `$9` as `show_mounts && caller.user_id().is_some()` after `$8`. Outer SELECT list: add `mount_target_id, mount_target_drive_id, mount_kind`. Decoder: positions 17, 18, 19.

`list_subtree_folders` (`:1128`): add `AND fo.mount_target_id IS NULL`.

- [ ] **Step 5: Service and handler**

`FolderService::list_resources_paged_with_perms`: pass `self.share_mounts.is_some()` as the new argument. In `folder_handler.rs` listing mapping, set

```rust
                            mount: row.mount_target_id.map(|t| MountDto {
                                kind: row.mount_kind.clone().unwrap_or_else(|| "shared_folder".to_owned()),
                                target_id: Some(t),
                                target_drive_id: row.mount_target_drive_id,
                            }),
```

Frontend `types.ts` `FolderItem`: add

```ts
	/** Present on share mount rows (docs/plan/share-mounts.md). */
	mount?: { kind: 'shared_folder' | 'shared_drive' | 'external'; target_id?: string; target_drive_id?: string };
```

- [ ] **Step 6: Run tests, regenerate OpenAPI**

Run: `RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount_listing_tests && cargo test --workspace && cargo run --bin generate-openapi`
Expected: green; `resources/gen/openapi.json` diff shows `MountDto`.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add -A src/ resources/gen/openapi.json frontend/src/lib/api/types.ts
git commit -m "feat(share-mounts): MountDto, listing mount columns, R0 recipient-only filter"
```

---

### Task 6: R1 — a mount row answers for its target

**Files:**
- Modify: `src/application/services/folder_service.rs` (`get_folder_with_perms` `:444`, `list_resources_paged_with_perms` `:1419`, `create_folder_with_perms` `:340`)
- Modify: `src/interfaces/api/handlers/file_handler.rs:257-271` (upload), `src/interfaces/api/handlers/folder_handler.rs:307-336` (zip), `:586-640` (listing precheck)

**Interfaces:**
- Consumes: `ShareMountService::resolve(caller, id) -> Result<Option<ResolvedMount>>`.
- Produces: `FolderService::resolve_share_mount(&self, caller: Subject, id: &str) -> Result<Option<ResolvedMount>, DomainError>` (public; returns `Ok(None)` when the service is disabled).

- [ ] **Step 1: Add the helper to `FolderService`**

```rust
    /// R1: `Some(target)` when `id` is a share mount the caller may look through.
    pub async fn resolve_share_mount(&self, caller: Subject, id: &str) -> Result<Option<ResolvedMount>, DomainError> {
        match &self.share_mounts {
            Some(svc) => svc.resolve(caller, id).await,
            None => Ok(None),
        }
    }
```

- [ ] **Step 2: Listing**

In `list_resources_paged_with_perms`, before the AuthZ `require`, resolve: if `Some(m)`, replace `parent_id` with `m.target_id.to_string()` for the rest of the method (authz on the target then follows naturally). In the handler `list_folder_resources`, the `require_permission(Read, &id)` precheck runs before the service: when `id` is a mount, the recipient has Read on the row (own drive) so the precheck passes; a non-recipient with Read on the row (via a shared parent) reaches the service, where `resolve` returns `NotFound`. No handler change needed.

- [ ] **Step 3: Get**

In `get_folder_with_perms`: after the `require(Read, id)`, call `self.resolve_share_mount(caller, id).await?`. If `Some(m)`: fetch the mount row DTO as today (`self.get_folder(id)`), fetch the target folder (`self.folder_storage.get_folder(&m.target_id.to_string())`), copy `etag`, `modified_at` from the target DTO (`FolderDto::from(target)`), and set `dto.mount = Some(MountDto { kind: m.kind.as_str().to_owned(), target_id: Some(m.target_id), target_drive_id: Some(m.target_drive_id) })`. If the folder is a mount but `resolve` errored (`NotFound`), propagate the error. Note that `resolve` returning `Ok(None)` for an actual mount row cannot happen; an `Err` is the denial.

To also deny plain reads of a mount row by a non-recipient (R0), the `require(Read, id)` alone is not enough: call `resolve_share_mount` **unconditionally** right after it, and let its `Err(NotFound)` propagate.

- [ ] **Step 4: Create folder and upload into a mount**

`create_folder_with_perms`: right after the `Some(parent_id)` check, `if let Some(m) = self.resolve_share_mount(Subject::User(caller_id), parent_id).await? { dto.parent_id = Some(m.target_id.to_string()); }` and continue with the (now target) parent; make `dto` mutable and re-bind `parent_id` after the swap.

Upload (`file_handler.rs:257`): the precheck `require_permission(Create, fid)` must run against the target. Before the precheck:

```rust
                if let Some(ref fid) = folder_id
                    && let Some(m) = state
                        .applications
                        .folder_service_concrete
                        .resolve_share_mount(Subject::User(auth_user.id), fid)
                        .await
                        .map_err(Self::domain_error_response)?
                {
                    folder_id = Some(m.target_id.to_string());
                }
```

(Adapt to the surrounding error-return style; `domain_error_response` exists in that impl.)

Zip (`folder_handler.rs:307`): after the `require_permission(Read, &id)` precheck, resolve with `folder_service.resolve_share_mount(authorized_as, &id)`; if `Some(m)`, use `m.target_id.to_string()` for `get_folder_with_perms` and `create_folder_zip_stream`, but keep `folder.name` from the **mount** row for the archive name. Simplest: `let target_id = resolved.map(|m| m.target_id.to_string()).unwrap_or_else(|| id.clone());` and call `get_folder_with_perms(&target_id, ..)` and `create_folder_zip_stream(&target_id, &folder.name)`.

- [ ] **Step 5: Build and run tests**

Run: `cargo test --workspace`
Expected: green. End-to-end coverage lands in Task 11's Hurl scenario.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add -A src/
git commit -m "feat(share-mounts): mount rows answer for their target in list, get, create, upload, zip"
```

---

### Task 7: R2/R3 guards — move, delete, copy, share, trash

**Files:**
- Modify: `src/application/services/folder_service.rs` (`move_folder_with_perms` `:840`, `delete_folder_with_perms` `:1047`)
- Modify: `src/application/services/file_management_service.rs:830-863` (copy)
- Modify: `src/application/services/trash_service.rs:248-296` (folder arm)
- Modify: `src/interfaces/api/handlers/grant_handler.rs:290-300` (`create_grant`), `src/application/services/share_service.rs:278-300` (`create_shared_link`)
- Modify: `src/interfaces/api/handlers/folder_handler.rs:252-300` (`delete_folder_with_trash_impl`)
- Create: `migrations/20261102000001_copy_folder_tree_skip_mounts.sql`

**Interfaces:**
- Consumes: `ShareMountService::{is_mount, unmount, relocate_mounts_under}`.

- [ ] **Step 1: Move**

In `move_folder_with_perms`, after the external-mount classify block and before `source_resource`:

```rust
        if let Some(svc) = &self.share_mounts {
            if let Some(parent_id) = &dto.parent_id
                && svc.is_mount(parent_id).await?
            {
                return Err(DomainError::conflict("Folder", "cannot move into a share mount"));
            }
            if svc.is_mount(id).await?
                && let Some(parent_id) = &dto.parent_id
            {
                let src = Uuid::parse_str(id).map_err(|_| DomainError::not_found("Folder", id))?;
                let dst = Uuid::parse_str(parent_id).map_err(|_| DomainError::not_found("Folder", parent_id.as_str()))?;
                let (sd, dd) = match &self.drive_repo {
                    Some(dr) => (
                        dr.drive_id_for_folder(src).await.map_err(|e| DomainError::internal_error("Folder", e.to_string()))?,
                        dr.drive_id_for_folder(dst).await.map_err(|e| DomainError::internal_error("Folder", e.to_string()))?,
                    ),
                    None => (Uuid::nil(), Uuid::nil()),
                };
                if sd != dd {
                    return Err(DomainError::conflict("Folder", "a share mount stays in its personal drive"));
                }
            }
        }
```

- [ ] **Step 2: Delete and trash**

`delete_folder_with_perms`: after the external-mount classify block:

```rust
        if let Some(svc) = &self.share_mounts
            && svc.is_mount(id).await?
        {
            let mount_id = Uuid::parse_str(id).map_err(|_| DomainError::not_found("Folder", id))?;
            return svc.unmount(caller_id, mount_id).await;
        }
```

`TrashService::move_to_trash`, folder arm, right after the `authz.require(Delete, ...)`:

```rust
                if let Some(svc) = &self.share_mounts {
                    if svc.is_mount(item_id).await? {
                        return svc.unmount(user_id, folder_id).await;
                    }
                    svc.relocate_mounts_under(user_id, folder_id).await?;
                }
```

`delete_folder_with_trash_impl` in the handler needs no change: it routes to `trash_service.move_to_trash` first, which now handles both cases, and falls back to `delete_folder_with_perms`, which also does.

- [ ] **Step 3: Copy**

`copy_folder_tree_with_perms` (`file_management_service.rs:830`): first statement:

```rust
        if let Some(svc) = &self.share_mounts
            && (svc.is_mount(source_folder_id).await?
                || match &target_parent_id { Some(p) => svc.is_mount(p).await?, None => false })
        {
            return Err(DomainError::conflict("Folder", "share mounts cannot be copied or copied into"));
        }
```

(Copy **into** a mount is refused in P0 for simplicity; copying into the target id directly keeps working.)

Migration `20261102000001_copy_folder_tree_skip_mounts.sql`: copy the whole `CREATE OR REPLACE FUNCTION storage.copy_folder_tree(...)` ... `$$ LANGUAGE plpgsql;` block from `migrations/20261019000000_copy_file_satellites.sql:188-332` verbatim, and in the subtree scan at the original lines 240-241 (`WHERE NOT fo.is_trashed AND fo.lpath <@ v_root_lpath`) add `AND fo.mount_target_id IS NULL`. Check line 272 (`WHERE NOT fo.is_trashed` in a second scan) and add the same predicate if that scan also walks the subtree. Header comment: one line, "copy_folder_tree skips share mount rows (R3)".

- [ ] **Step 4: Share guards**

`create_grant` (`grant_handler.rs`, before the `set_role`/`set_member_role` branch at `:297`):

```rust
    if let Resource::Folder(fid) = resource
        && let Some(svc) = state.share_mount_service.as_ref()
        && matches!(svc.is_mount(&fid.to_string()).await, Ok(true))
    {
        tracing::info!(target: "audit", event = "share_mount.rejected", reason = "grant_on_mount",
            caller_id = %caller_id, resource_id = %fid, "👮🏻‍♂️ grant on a share mount rejected");
        return AppError::from(DomainError::conflict("Grant", "a share mount cannot be shared")).into_response();
    }
```

This is a handler-side check of a structural fact, not an AuthZ decision; it mirrors the drive-kind branch that already lives there. `create_shared_link` (`share_service.rs:278`): `ShareService` needs `share_mounts: Option<Arc<ShareMountService>>` + `with_share_mounts` (wire in `di.rs` where `ShareService` is built); after `verify_item_exists`, if `item_type == Folder` and `is_mount(&dto.item_id)` → same audit line and `Err(DomainError::conflict("Share", "a share mount cannot be shared").into())` (map through `ShareServiceError` if the method returns that type; check the existing `?` conversions).

- [ ] **Step 5: Build and test**

Run: `cargo test --workspace && RUSTFLAGS='--cfg integration_tests' cargo test --lib share_mount`
Expected: green.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add -A src/ migrations/20261102000001_copy_folder_tree_skip_mounts.sql
git commit -m "feat(share-mounts): guards for move, delete, copy, share; trash relocates mounts"
```

---

### Task 8: Lifecycle hooks on grants and membership, recipient self-revoke

**Files:**
- Modify: `src/interfaces/api/handlers/grant_handler.rs` (`create_grant` after `:331`, `revoke_grant` `:540-580`, `set_role` `:866-896`)
- Modify: `src/application/services/drive_management_service.rs` (`set_member_role` `:195-293`, `remove_member` `:302-367`, add `with_share_mounts`)
- Modify: `src/application/services/subject_group_service.rs` (`add_member` `:434-437`, `remove_member` `:558-561`, add `with_share_mounts`)
- Modify: `src/common/di.rs` (chain `with_share_mounts` on both services)

**Interfaces:**
- Consumes: `ShareMountService::{on_folder_granted, on_drive_member_set, on_subject_revoked, reconcile}`.

- [ ] **Step 1: Grant handler hooks**

In `create_grant`, after the audit line `role_grant.created`:

```rust
    if let Some(svc) = state.share_mount_service.as_ref() {
        let r = match resource {
            Resource::Folder(fid) => svc.on_folder_granted(subject, fid).await,
            Resource::Drive(did) => svc.on_drive_member_set(subject, did).await,
            _ => Ok(()),
        };
        if let Err(e) = r {
            warn!("share mount reconcile after grant failed: {e}");
        }
    }
```

In `revoke_grant`, after the audit line `role_grant.revoked` and before the bus publish, same shape with `svc.on_subject_revoked(subject)`. In `set_role` (role change only) nothing is needed.

**Recipient self-revoke** (spec § API): in `revoke_grant`, change the authorisation condition

```rust
        if granter != caller_id
            && subject != Subject::User(caller_id)
            && let Err(e) = authz.require(Subject::User(caller_id), Permission::Share, resource).await
```

so the user subject of a grant may remove it. Add a Hurl assertion for this in Task 11 (dave revokes his own grant; alice's listing of grants on the folder no longer shows dave).

- [ ] **Step 2: Drive membership hooks**

`DriveManagementService`: field `share_mounts: Option<Arc<ShareMountService>>`, `with_share_mounts`. In `set_member_role` after the audit line: `if let Some(svc) = &self.share_mounts && let Err(e) = svc.on_drive_member_set(subject, drive_id).await { tracing::warn!("share mount reconcile failed: {e}"); }`. In `remove_member` after its audit line: `svc.on_subject_revoked(subject)`.

- [ ] **Step 3: Group membership hooks**

`SubjectGroupService`: same field/builder. In `add_member` and `remove_member`, inside the existing invalidation loop over `self.invalidation_targets(member)`, add `if let Some(svc) = &self.share_mounts && let Err(e) = svc.reconcile(uid).await { tracing::warn!(...) }`.

Wire both in `di.rs` (`:2550`, `:2568`): `.with_share_mounts(svc.clone())` when `share_mount_service` is `Some`. Because `ShareMountService` holds the group service through the `OnceLock` (Task 4), call `svc.set_group_service(subject_group_service.clone())` right after the group service is built.

- [ ] **Step 4: Build and test**

Run: `cargo test --workspace`
Expected: green.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add -A src/
git commit -m "feat(share-mounts): reconcile on grant, revoke, membership and group changes; self-revoke"
```

---

### Task 9: *Shared with me* carries `mount_id` / `declined`; `POST /api/mounts`

**Files:**
- Modify: `src/application/dtos/grant_dto.rs:441-451`
- Modify: `src/interfaces/api/handlers/grant_handler.rs:1032-1120` (`list_shared_with_me`)
- Create: `src/interfaces/api/handlers/mount_handler.rs`
- Modify: `src/interfaces/api/handlers/mod.rs`, `src/interfaces/api/routes.rs` (register `/mounts` under the protected API next to `/grants`)

**Interfaces:**
- Produces: `SharedWithMeItemDto { .., #[serde(skip_serializing_if = "Option::is_none")] pub mount_id: Option<Uuid>, pub declined: bool }`; `POST /api/mounts { "target_id": "<uuid>" }` → `201 FolderDto` (the mount row, with `mount` block).

- [ ] **Step 1: DTO and handler enrichment**

Add the two fields. In `list_shared_with_me`, after `drive_map` is built and before the items loop:

```rust
    let (mount_by_target, declined) = match state.share_mount_service.as_ref() {
        Some(svc) => {
            let mut targets: Vec<Uuid> = folder_ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect();
            targets.extend(drive_map.values().map(|d| d.root_folder_id));
            match tokio::join!(svc.mount_ids_for_targets(caller_id, &targets), svc.declined_for(caller_id, &targets)) {
                (Ok(m), Ok(d)) => (m, d),
                _ => (HashMap::new(), HashSet::new()),
            }
        }
        None => (HashMap::new(), HashSet::new()),
    };
```

At each of the three `SharedWithMeItemDto {` literals (`:1078`, `:1100`, `:1115`), compute the target id (`folder uuid`, `drive.root_folder_id`, or none for files) and set `mount_id: mount_by_target.get(&target).copied(), declined: declined.contains(&target)` (files: `None`, `false`).

- [ ] **Step 2: `POST /api/mounts`**

```rust
//! Share mount endpoints (docs/plan/share-mounts.md § API).
use crate::application::dtos::folder_dto::{FolderDto, MountDto};
use crate::common::di::AppState;
use crate::domain::services::authorization::Subject;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RemountDto {
    pub target_id: Uuid,
}

#[utoipa::path(post, path = "/api/mounts", request_body = RemountDto,
    responses((status = 201, body = FolderDto), (status = 404)), security(("bearerAuth" = [])), tag = "mounts")]
pub async fn remount(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(dto): Json<RemountDto>,
) -> impl IntoResponse {
    let Some(svc) = state.share_mount_service.as_ref() else {
        return AppError::not_found("Folder not found").into_response();
    };
    let resolved = match svc.remount(auth_user.id, dto.target_id).await {
        Ok(r) => r,
        Err(e) => return AppError::from(e).into_response(),
    };
    match state
        .applications
        .folder_service_concrete
        .get_folder_with_perms(&resolved.mount_id.to_string(), Subject::User(auth_user.id))
        .await
    {
        Ok(folder) => (StatusCode::CREATED, Json(folder)).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}
```

Check the exact import paths (`AuthUser` lives where `grant_handler.rs` imports it from; `AppError::not_found` signature as used in `folder_handler.rs:620`). Register `pub mod mount_handler;` and in `routes.rs` next to the grants router: `.nest("/mounts", Router::new().route("/", post(mount_handler::remount)).with_state(app_state.clone()))`. Add the handler to the OpenAPI registry (grep `grant_handler::create_grant` in `src/interfaces/api/openapi.rs` or wherever `#[openapi(paths(...))]` lists handlers, and add `mount_handler::remount` plus `RemountDto`/`MountDto` to `components(schemas(...))`).

- [ ] **Step 3: Build, regenerate OpenAPI**

Run: `cargo test --workspace && cargo run --bin generate-openapi`

- [ ] **Step 4: Commit**

```bash
cargo fmt --all && cargo clippy --all-features --all-targets -- -D warnings
git add -A src/ resources/gen/openapi.json
git commit -m "feat(share-mounts): shared-with-me mount_id/declined, POST /api/mounts remount"
```

---

### Task 10: Frontend — badge, links, remount action, i18n

**Files:**
- Modify: `frontend/src/lib/api/endpoints/grants.ts:174-180`
- Create: `frontend/src/lib/api/endpoints/mounts.ts`
- Modify: `frontend/src/routes/files/[...path]/+page.svelte:2213-2262` (add `rowBadge` snippet)
- Modify: `frontend/src/routes/shared-with-me/+page.svelte:190-211`
- Modify: `frontend/static/locales/*.json` (16 files), `files` block
- Test: `frontend/src/lib/api/endpoints/mounts.test.ts`, `frontend/src/routes/shared-with-me/page.test.ts` (new)

**Interfaces:**
- Produces: `remount(targetId: string): Promise<FolderItem>`; `IncomingGrantItem.mount_id?: string; declined?: boolean`.

- [ ] **Step 1: Failing endpoint test**

`frontend/src/lib/api/endpoints/mounts.test.ts`:

```ts
import { it, expect, vi } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(async () => ({ ok: true, json: async () => ({ id: 'm1', name: 'Docs' }) }))
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({ 'X-CSRF': 't' }) }));

import { apiFetch } from '$lib/api/client';
import { remount } from './mounts';

it('posts the target id to /api/mounts', async () => {
	const folder = await remount('t1');
	expect(folder.id).toBe('m1');
	const [url, init] = (apiFetch as unknown as ReturnType<typeof vi.fn>).mock.calls[0];
	expect(url).toBe('/api/mounts');
	expect(init.method).toBe('POST');
	expect(JSON.parse(init.body as string)).toEqual({ target_id: 't1' });
});
```

Run: `cd frontend && npx vitest run src/lib/api/endpoints/mounts.test.ts` → FAIL (module missing).

- [ ] **Step 2: Endpoint**

`frontend/src/lib/api/endpoints/mounts.ts`:

```ts
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { FolderItem } from '$lib/api/types';

/** Re-creates the caller's mount of a declined shared folder or drive. */
export async function remount(targetId: string): Promise<FolderItem> {
	const res = await apiFetch('/api/mounts', {
		method: 'POST',
		headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
		body: JSON.stringify({ target_id: targetId })
	});
	if (!res.ok) throw new Error(`remount failed: ${res.status}`);
	return (await res.json()) as FolderItem;
}
```

Match the exact `apiFetch` call shape used by `deleteFolder` in `folders.ts:310` (credentials, header merging). Add to `IncomingGrantItem`: `mount_id?: string; declined?: boolean;`.

- [ ] **Step 3: Badge on the files page**

Inside the `<ResourceList …>` in `files/[...path]/+page.svelte`, add a snippet:

```svelte
		{#snippet rowBadge(item)}
			{#if !isFile(item) && item.mount}
				<span
					class="mount-badge"
					title={item.mount.kind === 'shared_drive'
						? t('files.mount_shared_drive', 'Shared drive')
						: t('files.mount_shared_folder', 'Shared folder')}
					data-testid={`mount-badge-${item.id}`}
				>
					<Icon name={item.mount.kind === 'shared_drive' ? 'users' : 'share-alt'} />
				</span>
			{/if}
		{/snippet}
```

Style in the page's `<style>`: `.mount-badge { color: var(--color-badge-blue-text); font-size: 0.65em; }` (BEM block; all colours via tokens). Verify `Icon` is already imported in the page; if not, import from `$lib/icons/Icon.svelte`.

- [ ] **Step 4: Shared with me**

In `open()`: folders navigate to the mount when present:

```ts
	function open(item: FileItem | FolderItem) {
		if (!isFile(item)) {
			const grant = raw.find((g) => g.resource.id === item.id);
			goto(resolve(`/files/${grant?.mount_id ?? item.id}`));
			return;
		}
```

Add a **Mount** action for declined items: find where per-item actions are rendered in this page (the `cardOverlay`/kebab area); add a button with `data-testid={`remount-${item.id}`}`, label `t('shared_with_me.remount', 'Mount')`, shown when `grant.declined`, calling `remount(item.id)` then navigating to `/files/${folder.id}`. Keep it minimal; follow the existing button component used on this page.

- [ ] **Step 5: i18n**

Add to the `files` block of **every** file in `frontend/static/locales/` (`ar de en es fa fr hi it ja ko nl pl pt ru zh-TW zh`): `"mount_shared_folder": "Shared folder"`, `"mount_shared_drive": "Shared drive"`, and to a `shared_with_me` block (create if missing, mirror en.json's structure): `"remount": "Mount"`. Non-English files take the English text for now (existing practice; `scripts/bulk_translate.mjs` fills translations later). Run `node scripts/check-locales.mjs` → must pass.

- [ ] **Step 6: Page test**

`frontend/src/routes/shared-with-me/page.test.ts`, modelled on `routes/shared/page.test.ts:1-35`: mock `$app/navigation` (`goto`), `$app/paths` (`resolve: (p) => p`), `$lib/api/endpoints/grants` (`fetchSharedWithMe` returning one folder grant with `mount_id: 'm1'`), `$lib/api/endpoints/mounts`; render the page; `fireEvent.click` the folder row's open target (use the `data-testid` ResourceList gives rows: grep `data-testid` in `ResourceList.svelte` for the row/open element); assert `goto` was called with `/files/m1`.

- [ ] **Step 7: Checks**

Run: `cd frontend && npm run check && npm run test:unit`
Expected: green.

- [ ] **Step 8: Commit**

```bash
git add -A frontend/
git commit -m "feat(share-mounts): mount badge, shared-with-me links to the mount, remount action"
```

---

### Task 11: Hurl end-to-end scenario and full verification

**Files:**
- Create: `tests/api/share_mounts.hurl`
- Modify: `tests/api/run.sh` (add `"$API_DIR/share_mounts.hurl" \` right after the `drives_membership.hurl` line, ~`:362`)
- Modify: `tests/api/README.md` (one table row)

- [ ] **Step 1: Write the scenario**

```hurl
# =============================================================
# OxiCloud — share mount points (docs/plan/share-mounts.md P0)
# =============================================================
# alice grants a folder to mia -> mount appears in mia's root, answers for the
# target, is invisible to a third user, can be renamed, declined, remounted.
# Runs after drives_membership.hurl; creates its own users.
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }
HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
alice_user_id: jsonpath "$.user.full.user.id"

GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "mia", "password": "MiaPassword1!", "email": "mia@example.com", "role": "user" }
HTTP 201
[Captures]
mia_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "noah", "password": "NoahPassword1!", "email": "noah@example.com", "role": "user" }
HTTP 201
[Captures]
noah_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mia", "password": "MiaPassword1!" }
HTTP 200
[Captures]
mia_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "noah", "password": "NoahPassword1!" }
HTTP 200
[Captures]
noah_token: jsonpath "$.access_token"

GET {{base_url}}/api/folders
Authorization: Bearer {{mia_token}}
HTTP 200
[Captures]
mia_home_id: jsonpath "$[0].id"

# alice: Reports/Q3, plus mia already has her own "Reports" -> suffix expected
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Reports", "parent_id": "{{alice_home_id}}" }
HTTP 201
[Captures]
reports_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Q3", "parent_id": "{{reports_id}}" }
HTTP 201
[Captures]
q3_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{mia_token}}
Content-Type: application/json
{ "name": "Reports", "parent_id": "{{mia_home_id}}" }
HTTP 201
[Captures]
mia_own_reports_id: jsonpath "$.id"

# grant -> mount "Reports (2)" in mia's root
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "subject": { "type": "user", "id": "{{mia_user_id}}" }, "resource": { "type": "folder", "id": "{{reports_id}}" }, "role": "viewer" }
HTTP 201
[Captures]
grant_id: jsonpath "$.grants[0].id"

GET {{base_url}}/api/folders/{{mia_home_id}}/resources?resource_types=folder&order_by=name
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[1].resource.name" == "Reports (2)"
jsonpath "$.items[1].resource.mount.kind" == "shared_folder"
jsonpath "$.items[1].resource.mount.target_id" == "{{reports_id}}"
[Captures]
mount_id: jsonpath "$.items[1].resource.id"

# R1: listing the mount lists the target's children
GET {{base_url}}/api/folders/{{mount_id}}/resources?resource_types=folder
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{q3_id}}"

GET {{base_url}}/api/folders/{{mount_id}}
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.mount.target_id" == "{{reports_id}}"
jsonpath "$.name" == "Reports (2)"

# Shared with me links to the mount
GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.items[0].mount_id" == "{{mount_id}}"
jsonpath "$.items[0].declined" == false

# R0: mia shares her root with noah; noah must not see or open the mount
POST {{base_url}}/api/grants
Authorization: Bearer {{mia_token}}
Content-Type: application/json
{ "subject": { "type": "user", "id": "{{noah_user_id}}" }, "resource": { "type": "folder", "id": "{{mia_home_id}}" }, "role": "viewer" }
HTTP 201

GET {{base_url}}/api/folders/{{mia_home_id}}/resources?resource_types=folder
Authorization: Bearer {{noah_token}}
HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{mia_own_reports_id}}"

GET {{base_url}}/api/folders/{{mount_id}}
Authorization: Bearer {{noah_token}}
HTTP 404

GET {{base_url}}/api/folders/{{mount_id}}/resources
Authorization: Bearer {{noah_token}}
HTTP 404

# R2: rename is local; sharing a mount is refused; copying it is refused
PUT {{base_url}}/api/folders/{{mount_id}}/rename
Authorization: Bearer {{mia_token}}
Content-Type: application/json
{ "name": "Alice reports" }
HTTP 200

GET {{base_url}}/api/folders/{{reports_id}}
Authorization: Bearer {{alice_token}}
HTTP 200
[Asserts]
jsonpath "$.name" == "Reports"

POST {{base_url}}/api/grants
Authorization: Bearer {{mia_token}}
Content-Type: application/json
{ "subject": { "type": "user", "id": "{{noah_user_id}}" }, "resource": { "type": "folder", "id": "{{mount_id}}" }, "role": "viewer" }
HTTP 409

# trash of an ancestor relocates the mount: move mount into mia's own Reports, trash Reports
PUT {{base_url}}/api/folders/{{mount_id}}/move
Authorization: Bearer {{mia_token}}
Content-Type: application/json
{ "parent_id": "{{mia_own_reports_id}}" }
HTTP 200

DELETE {{base_url}}/api/folders/{{mia_own_reports_id}}
Authorization: Bearer {{mia_token}}
HTTP 204

GET {{base_url}}/api/folders/{{mount_id}}
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.parent_id" == "{{mia_home_id}}"

# decline (delete the mount) -> gone, declined=true; remount restores
DELETE {{base_url}}/api/folders/{{mount_id}}
Authorization: Bearer {{mia_token}}
HTTP 204

GET {{base_url}}/api/folders/{{mount_id}}
Authorization: Bearer {{mia_token}}
HTTP 404

GET {{base_url}}/api/folders/{{reports_id}}
Authorization: Bearer {{alice_token}}
HTTP 200

GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.items[0].declined" == true

POST {{base_url}}/api/mounts
Authorization: Bearer {{mia_token}}
Content-Type: application/json
{ "target_id": "{{reports_id}}" }
HTTP 201
[Asserts]
jsonpath "$.mount.target_id" == "{{reports_id}}"
[Captures]
mount2_id: jsonpath "$.id"

# recipient self-revoke removes the grant and the mount
DELETE {{base_url}}/api/grants/{{grant_id}}
Authorization: Bearer {{mia_token}}
HTTP 204

GET {{base_url}}/api/folders/{{mount2_id}}
Authorization: Bearer {{mia_token}}
HTTP 404

# shared drive membership mounts the drive root; the mount is not declinable
POST {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "kind": "shared", "name": "Team", "owner": { "type": "user", "id": "{{alice_user_id}}" } }
HTTP 201
[Captures]
team_drive_id: jsonpath "$.id"
team_root_id: jsonpath "$.root_folder_id"

POST {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "subject": { "type": "user", "id": "{{mia_user_id}}" }, "role": "editor" }
HTTP 201

GET {{base_url}}/api/folders/{{mia_home_id}}/resources?resource_types=folder&order_by=name
Authorization: Bearer {{mia_token}}
HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.name == 'Team')].resource.mount.kind" includes "shared_drive"
[Captures]
team_mount_id: jsonpath "$.items[?(@.resource.name == 'Team')].resource.id" nth 0

DELETE {{base_url}}/api/folders/{{team_mount_id}}
Authorization: Bearer {{mia_token}}
HTTP 409
```

Verify against the real responses as you go (`POST /api/drives` capture names, `POST /members` status code: check `drives_membership.hurl` for the status it asserts and adapt). Note the `delete_folder_with_trash` route: with trash enabled it returns 204; if the test server config disables trash, `delete_folder_with_perms` runs the same unmount path.

- [ ] **Step 2: Register and run**

Add the file to `HURL_FILES` in `run.sh` after `drives_membership.hurl`, add a README row, then:

Run: `bash tests/api/run.sh share_mounts`
Expected: all steps pass. Iterate on the server until they do.

- [ ] **Step 3: Full verification**

Run, in order, and paste outputs in the commit/PR notes:

```bash
just check
just test
just test-integration share_mount
just api-test
cd frontend && npm run check && npm run test:unit
```

Expected: every command green.

- [ ] **Step 4: Commit**

```bash
git add tests/api/share_mounts.hurl tests/api/run.sh tests/api/README.md
git commit -m "test(share-mounts): Hurl end-to-end scenario"
```

---

## Not in P0 (tracked in the spec)

- Name URLs, `GET /api/folders/resolve`, ancestors hop, `folderHref`, All files / Personal files toggle, etag propagation across mounts: **P1**.
- Trash response `relocated_mounts` for the SPA toast: lands with P1 (the trash endpoint currently answers 204; changing it is an API-shape change best done with the P1 frontend work).
- Consistency checks, periodic reconcile job: **P2**.
- WebDAV surfaces: mounts appear in `/webdav` and `/remote.php/dav` root listings automatically (same listing repository), but PROPFIND **into** a mount by path needs the P1 path resolver. P0 ships listing-level visibility only; do not claim NC sync support until P1.
