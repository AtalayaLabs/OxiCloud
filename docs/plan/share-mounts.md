# Share mount points — feature proposal

> **Status**: feature proposal, draft 2026-10-06. Not yet reviewed upstream.
> Builds on `drive.md` (drives, `role_grants`) and borrows the mount-root
> idea from external mounts (`migrations/20260805000000_external_mounts.sql`).
> Open questions are listed at the end.

## Context

Two things a NextCloud user takes for granted are missing in OxiCloud today:

1. **Shared things have no place in my tree.** A folder someone granted me, or
   a shared drive I am a member of, exists only as an entry in the flat
   *Shared with me* list (`GET /api/grants/incoming/resources`). Clicking it
   jumps to `/files/<uuid>` of a folder in somebody else's drive. The native
   WebDAV root and the NextCloud-compat `/remote.php/dav/files/<user>/` show
   the personal drive only, so **the NextCloud desktop and mobile clients
   never see anything that was shared with the user**. For a product whose
   stated customer target is NextCloud users and whose main feature axis is
   collaboration, that is a gap, not a nicety.

2. **Shared content has no place in the recipient's tree.** It cannot be
   organised, renamed or found by browsing, only through the list.

Both problems have the same root cause: the user has no **namespace of their
own** into which shared resources are projected. NextCloud solves it with
*mount points*: an incoming share is mounted into the recipient's home at a
recipient-chosen name; name collisions get a ` (2)` suffix; the recipient can
rename or move the mount point; deleting it declines the share. Group Folders
(the NextCloud analogue of OxiCloud shared drives) are mounted the same way.
Everything downstream (sync clients, breadcrumbs, "Personal files" vs "All
files") falls out of that one concept.

This proposal introduces the same concept, adapted to OxiCloud's drive model.
Folder URLs stay id-based (§ URL surface).

## Goals

- A granted folder, and every shared drive the caller is a member of, appears
  as a folder in the caller's **personal drive**, named after the source,
  collision-suffixed, renamable and movable by the recipient only.
- Both WebDAV surfaces list those mounts, so NextCloud clients sync shared
  content with **no client-side change**.
- NextCloud's navigation vocabulary: **All files** (the tree, mounts included)
  and **Personal files** (the same tree with mounts hidden). Existing views
  (*Shared with me*, *My shares*, *Recent*, *Favorites*, *Trash*) remain.

## Non-goals (v1)

- Mounting **single shared files**. NextCloud does it; here it would need a
  second mechanism on `storage.files`. Follow-up (§ Phasing P3).
- **Reshare.** Mounts never widen access. Resharing is a separate permission
  that does not exist yet and is not introduced here.
- Changing external mounts. They keep their provider-backed virtual subtree;
  only the DTO marker is unified (§ API) so the frontend has one "this is a
  mount" shape.
- Exposing shared drives at a separate URL root (`/files/@drive/<name>/…`).
  Considered during design; superseded by mounting shared drives into the
  personal drive, which needs no routing sigil and no reserved folder name.

## The model

### A mount is a folder row with a target

```sql
ALTER TABLE storage.folders
    ADD COLUMN mount_target_id UUID
        REFERENCES storage.folders(id) ON DELETE CASCADE;

CREATE INDEX idx_folders_mount_target
    ON storage.folders(mount_target_id) WHERE mount_target_id IS NOT NULL;
```

A row with `mount_target_id IS NOT NULL` is a **mount**. It lives in the
recipient's personal drive like any other folder (so `drive_id`, `parent_id`,
`path`, `lpath`, the unique-name indexes, favorites, `tree_modified_at` all
apply unchanged) and **has no children of its own**. Its content is the target
folder's content, which keeps living in the target's drive with the target's
`drive_id`, quota, trash and grants.

The target is always a **folder**:

| Mounted thing | `mount_target_id` |
|---|---|
| Granted folder (`role_grants.resource_type = 'folder'`) | that folder |
| Shared drive membership (`resource_type = 'drive'`) | `drives.root_folder_id` of that drive |

No synthetic ids. Everything below the mount is addressed by its real UUID in
the target drive, so the existing `*_with_perms` methods, realtime folder
topics, favorites, search hits and thumbnails all work on it unchanged. The
**only** special node is the mount row itself, and the only special step is
"when asked about a mount row, answer about its target" (§ Resolution).

Why not a side table (like `storage.external_mounts`)? The mount's identity
*is* a folder (it needs a parent, a name, a position in `lpath`, uniqueness
against siblings), and every consumer that lists folders must be able to see
"this one is a mount" without a second query. One nullable FK on the row is the
cheapest honest representation. The recipient needs no column either: the
mount is in a personal drive, so the recipient is `drives.default_for_user`.

### Invariants (enforced in the database, not remembered by reviewers)

Trigger-enforced on `storage.folders` (and `storage.files` for I2):

- **I1 — personal drive only.** A mount row's `drive_id` must be a drive with
  `kind = 'personal'`. Nobody can plant a mount in a shared drive.
- **I2 — leaf.** No `storage.folders` or `storage.files` row may have a mount
  as `parent_id`/`folder_id`. (Content lives under the target.)
- **I3 — no mount-of-mount.** `mount_target_id` must point at a row whose own
  `mount_target_id IS NULL`.
- **I4 — not self or own drive.** The target's `drive_id` differs from the
  mount's `drive_id`.
- **I5 — one mount per (recipient drive, target).** Partial unique index on
  `(drive_id, mount_target_id) WHERE mount_target_id IS NOT NULL`. Trashed
  rows are deliberately included: a mount is never meant to reach the trash
  (R2), and if one ever does, restoring it must not produce a second mount
  of the same target.

Name uniqueness comes for free from `idx_folders_unique_name` /
`idx_folders_unique_name_root` (scoped by `drive_id`), which is exactly the
NextCloud collision rule: two mounts, or a mount and an own folder, cannot
share a name under the same parent.

### Declined mounts

NextCloud lets a recipient remove a received share from their tree without
the sharer noticing; the share then shows as "rejected" and can be accepted
again. The equivalent here:

```sql
CREATE TABLE storage.share_mount_declines (
    recipient_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    target_folder_id UUID NOT NULL REFERENCES storage.folders(id) ON DELETE CASCADE,
    declined_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recipient_id, target_folder_id)
);
```

Deleting a mount removes the folder row and inserts a decline marker. The
reconciler (§ Lifecycle) never recreates a declined target. *Shared with me*
shows declined items with a **Mount** action, which deletes the marker and
creates the mount again.

**Decline is hide, never revoke**, for direct and group folder grants
alike. The grant row is untouched and the sharer notices nothing. NextCloud
deletes a *user* share when the recipient "unshares from self" and only
hides *group* shares; one behaviour for both is simpler and reversible. A recipient who wants the grant gone uses
**Remove sharing** on a direct grant (§ API), which is an ordinary revoke.

Shared-drive membership mounts are **not** declinable, like NextCloud Group
Folders: a member's tree always contains every shared drive they belong to.
Deleting such a mount is refused (409 `mount_not_declinable`). Only folder
grants can be declined.

### Target placement and naming

New mounts are created under the **share folder**: a configurable path inside
the personal drive, `OXICLOUD_SHARE_MOUNT_FOLDER` (NextCloud `share_folder`).
Default is empty, meaning the drive root. A non-empty value is created on
demand. The recipient can move mounts anywhere else in their personal drive
afterwards.

The name is the target folder's name (for a shared drive: its display name,
which *is* the root folder's name). On a unique-index conflict the creator
retries with ` (2)`, ` (3)`, … exactly like NextCloud. Because the retry
happens against the real index there is no race window; two concurrent
grants for same-named folders cannot both win the plain name.

## Lifecycle

### When mounts appear

| Event | Action | Where |
|---|---|---|
| Grant created with a **user** subject on a folder | create mount for that user, synchronously | `create_grant` handler → `ShareMountService::ensure_mount` |
| Grant created with a **group** subject on a folder | create mounts for the group's transitive users, synchronously (same fan-out `create_grant` already does for bell notifications) | same |
| Shared drive member added (user or group) | create mounts targeting `drives.root_folder_id` | `DriveManagementService::set_member_role` |
| User added to a group | reconcile that user | `SubjectGroupService::add_member` |
| Login / session bootstrap | reconcile the caller (cheap, idempotent; covers everything above if an event was missed) | auth service |

**Fan-out cost (group grants).** Reconcile of one user is a fixed handful of
indexed queries, so a group grant costs one reconcile per transitive member
inside the granting request. P0 does this synchronously: correct, and fine for
the groups a 10k-user instance typically shares with (tens of members). A grant
to a very large group (hundreds or more) makes that request slow. P2 moves the
group fan-out to the job registry above a member threshold; until the job
reaches a member, that member sees the share in *Shared with me* and gets the
mount on their next login or periodic reconcile, whichever comes first.
Nothing is lost while the job is pending, only delayed.
| Periodic | reconcile users with a stale `auth.users.mounts_reconciled_at`, cursor-paged per `jobs-handling-recoverable-error.md` | job registry |

**Reconcile(user)** is the single source of truth and everything else is an
optimisation for immediacy:

1. Compute the set of **mount-worthy targets** for the user: every incoming
   `folder` grant (direct or via groups, not expired, `granted_by != user`)
   plus the root folder of every shared drive they hold a role on,
   **minus** any folder grant whose target has an ancestor (`lpath @>`) that
   is also in the set or whose drive the user is a member of (nested grants
   do not get a second mount; NextCloud behaves the same).
2. Create missing mounts, skipping declined targets.
3. Delete mounts whose target is no longer in the set.

This is the same query shape as `list_incoming_resources_paged` plus one
anti-join; it runs in one round trip and is bounded by the number of the
user's grants, not by users or files.

### When mounts disappear

| Event | Effect on the mount row |
|---|---|
| Grant revoked / expired / membership removed | deleted by reconcile. For **user** subjects the revoke handler also deletes synchronously; it already publishes `AuthzChanged` on the user's topic, which the SPA turns into "access revoked" and navigates away |
| Target folder hard-deleted | `ON DELETE CASCADE` on `mount_target_id` |
| Target folder trashed | mount hidden by the listing filter (target unreadable: `NOT is_trashed` is part of readability); restored automatically when the target is restored |
| Recipient deletes the mount | folder row deleted, decline marker inserted. **Nothing in the target drive changes** |
| Recipient's account deleted | cascades through `drives` |

**Security does not depend on reconcile timing.** A mount row is *rendered*
only if the caller can currently Read its target (§ Resolution). Reconcile
is garbage collection, not an access check.

## Resolution — the one special step

### Rule R0 — only the recipient sees a mount

A mount row is visible to, and resolvable by, **exactly one subject: the user
whose default personal drive contains it**. Everyone else gets neither the
row in a listing nor a hit on direct access (404, anti-enumeration).

This matters because a personal folder *containing* a mount can itself be
shared. Without R0 the sharer's recipients would see the mount row, and a
naive "resolve mount as its owner" implementation would be a confused deputy:
an implicit reshare of something the mount owner has no right to reshare.
Mounts therefore **never change the identity of the caller**; the content
behind a mount is authorised against the caller's own grants on the target.

R0 is enforced in two places and nowhere else:

- **Listing SQL** (`list_resources_paged`, WebDAV batch listings, search):
  `WHERE mount_target_id IS NULL OR drive_id = <caller's personal drive>`,
  AND for mount rows the existing readability predicate applied to the
  **target** (folder cascade grant or drive membership). Mount rows a caller
  cannot read through are dropped, not shown as broken.
- **`FolderService::resolve_mount(caller, id)`**: for a UUID that is a mount
  row, returns the target id after `authz.require(caller, Read, target)` and
  the recipient check; the handful of entry points below call it first.

### Rule R1 — id-level redirect

Operations that take a folder id and mean "the content of this folder" are
answered for the **target**, with AuthZ on the target against the caller:

| Entry point | Mount row → |
|---|---|
| `GET /folders/{id}/resources`, `/{id}/folders`, `/{id}/files` | list target |
| `GET /folders/{id}` | the mount row's own identity (`id`, `name`, `parent_id`, `drive_id`, `path`) with content fields (`etag`, `modified_at`) taken from the target, plus the `mount` block (§ API). **Authority:** placement fields describe the mount and are what the SPA renders and navigates by; content fields describe the target and are authoritative for caching and change detection, so `etag` changes exactly when the target's content changes. `mount.target_id` names the folder those content fields come from. No field carries two meanings |
| `GET /folders/{id}/zip` | zip of target (Read on target) |
| upload / create folder / create file into `{id}` | into target (Create on target). Cross-drive policy `forbid_cross_drive_move` applies to moves, not to new uploads |
| move / copy *into* `{id}` | into target, through the existing cross-drive move/copy paths and their policy gates |
| WebDAV PROPFIND Depth 1 on the mount path | target's children, hrefs built from the client path (`webdav_href` already prints the *requested* path, not `db_path`) |
| WebDAV `getetag` of the mount | target's etag (§ ETag) |
| `oc:permissions` (NC) | computed from the caller's role on the target |

Below the mount everything is ordinary: real ids in the target drive.

### Rule R2 — operations on the mount row itself

| Operation on mount row M | Behaviour |
|---|---|
| Rename M | renames the mount row (recipient-local). Target untouched. Requires Update on M, which the recipient has as personal-drive owner |
| Move M inside own personal drive | ordinary move. Guards: destination not under another mount (I2 forbids children under a mount anyway), destination in the same personal drive (I1) |
| Move M into a shared drive / into a mount | 409 `cross_boundary_move` (existing error) |
| Delete M / trash M | folder-grant mount: **unmount**, row deleted + decline marker, no trash entry, *Shared with me* is the restore path. Shared-drive mount: 409 `mount_not_declinable`. Never cascades into the target |
| Trash an ancestor of M | Mounts are not content of the folder, so they are not trashed with it. Before the ancestor is trashed, every mount in its subtree is **relocated to its own recipient's share folder / drive root** (collision suffix applies), whoever does the trashing: an Owner-grantee on the recipient's folder may trash it too. Same for every client, no dialog; the trash response reports the relocated mounts so the SPA can show a toast. NextCloud does the same. Alternatives considered: asking the user (rejected as needless UI for a rare case), letting M ride along into trash (rejected because restoring would then re-mount via the trash path, which trash code must not know about) |
| Permanently delete an ancestor of M | M cascades with it (`parent_id ON DELETE CASCADE`), target untouched; reconcile recreates M at the share folder on next run. Never happens through the UI because of the relocation rule above |
| Copy M | 409 `mount_not_copyable`. Copying an ancestor skips M (ltree subtree copy excludes `mount_target_id IS NOT NULL`). Copying *content* out of a mount is an explicit cross-drive copy of the target's children, which already exists |
| Favorite M | ordinary favorite on the row |
| Share M (grant on M) | 409 `cannot_share_mount` (would be a reshare in disguise) |
| Public link on M | same 409 |

### Rule R3 — subtree operations treat mounts as leaves

Anything that walks `lpath @>` must exclude mount rows and never descend
through them: `copy_folder_tree`, `list_subtree_folders` (zip of an
ancestor), trash cascade, `tree_etag` propagation within the personal drive,
search-index crawls, the folders consistency pass. Mount rows have no
children (I2), so "never descend" is automatically true at the DB level;
"exclude the row" is one predicate per query. The consistency pass gains a
check (§ Consistency).

**Note — `Depth: infinity` is not a gap.** Treating mounts as leaves for
subtree walks does not hide shared content from WebDAV clients: no client
gets a whole subtree in one PROPFIND today. Native WebDAV rejects
`Depth: infinity` with 403 and `<D:propfind-finite-depth/>` (RFC 4918 §9.1),
and the NextCloud layer answers any depth other than `0` with one level.
Clients walk the tree level by level, and each level passes through the
mount by R1.

### Path resolution with mount hops

`PathResolverService::resolve_path_in_drive(caller, drive_id, path)` learns
one new move. Walk the segments; whenever the current folder is a mount row
(R0 applies), continue the walk from its target, in the target's drive. This
serves WebDAV paths only; SPA URLs stay id-based (§ URL surface).

One resolver, two consumers: native WebDAV (`resolve_webdav_scope` → PROPFIND/GET/MKCOL/PUT/MOVE/COPY/DELETE) and NC
WebDAV (`nc_to_internal_path` + `nc_resolve_or_fallback`). The repository
lookup stays `get_folder_by_path(path, drive_id)`; the resolver just splits
the path at mount hops and rebases the remainder onto the target's path.

## ETag propagation across mounts

NextCloud sync clients poll the root with `Depth: 0` and descend only where
etags changed, so a change under a shared folder must change the recipient's
root etag and every etag on the path down to it. OxiCloud's `tree_etag_dirty`
queue bumps ancestors with `lpath @>` **filtered by `drive_id`**, which stops
at the drive boundary — correct for drives, wrong for mounts.

**Rejected: propagate by writing.** Bumping `tree_modified_at` on every mount
row of every recipient, and from there up each recipient's personal tree,
turns one upload into a shared drive into one ancestor chain per recipient,
each ending on that recipient's root row. With 50 members that is 50 extra
write chains per upload, all contending on root rows the existing queue
already serialises. The cost grows with recipients × depth and lands on the
hot path.

**Chosen: derive at read time.** Nothing is written across the mount edge.
The etag a recipient sees for a folder in their personal drive is computed
from two inputs:

- the folder's own `tree_modified_at` (changes inside the personal drive,
  propagated as today), and
- the greatest `tree_modified_at` among the **targets** of mounts inside that
  folder's subtree.

The second input is one indexed query per PROPFIND: mounts are rows in the
recipient's own drive (`lpath <@ folder`, `mount_target_id IS NOT NULL`),
joined to their targets. A user has few mounts (tens), so the query is
bounded by the user's own share count, not by the instance or by the other
recipients. The mount row itself reports its target's etag (R1). Writes in a
shared drive keep costing exactly what they cost today.

Cost moves to reads of folders that contain mounts, which is where it
belongs: it is paid by the user who looks, proportionally to what they have
mounted. Folders without mounts below them skip the extra query entirely via
an `EXISTS` short-circuit on `idx_folders_mount_target`.

Without one of the two, a NextCloud client would show the mount but never
notice changes under it, a *silent* staleness that is not acceptable for the
first release that claims NC clients see shares (§ Phasing P1). This section
is the gate for P1.

## URL surface (SPA)

**URLs stay id-based** (`/files/<folder uuid>`), as `drive.md` §9 defines,
including folders reached through a mount. Maintainer decision on review:
one link is the same for everyone and keeps working across renames and
moves, so anybody can share it and every recipient with access can open it
(the Google Drive / Notion model). A name path is per caller — the same
folder reads differently for its owner and for a recipient whose mount was
renamed or suffixed — so it cannot be that shared link. Name-based URLs are
therefore out of scope.

| URL | Resolves to |
|---|---|
| `/files/<uuid>` | the folder, unchanged. For a mount row: the mount (R1 answers for its target) |
| `/files/…?view=personal` | Personal files: same tree, mount rows hidden. The sidebar has two entries like NextCloud, **All files** (`/files`) and **Personal files** (`/files?view=personal`) |
| `/files/…?file=<uuid>` | unchanged: opens the file viewer |
| `/shared-with-me` | unchanged list view; each item links to its mount (`/files/<mount id>`), declined items show **Mount** |

## API

### New

- `POST /api/mounts` `{ "target_id": "<folder uuid>" }` → `FolderDto` of the
  new mount. Used by **Mount** on a declined item. Target must be
  mount-worthy for the caller (same predicate as reconcile), else 404.
- `DELETE /api/folders/{id}` on a mount row performs the unmount (R2); no new
  route.
- Trash of a folder returns `relocated_mounts: [FolderDto]` (usually empty)
  so the SPA can tell the user where their shares went.
- `DELETE /api/grants/{id}` additionally allows the caller who is the
  **user subject** of the grant (today: granter or Share holder only). This
  gives *Shared with me* a **Remove sharing** action for direct grants. It is
  independent of mounts and can ship first.
- `GET /api/config` → `features.share_mounts: bool`.

### Changed

- `FolderDto` gains an optional block, shared with external mounts so the
  frontend has one shape:

  ```json
  "mount": {
    "kind": "shared_folder" | "shared_drive" | "external",
    "target_id": "<folder uuid>",       // absent for external
    "target_drive_id": "<drive uuid>",  // absent for external
    "role": "viewer" | "editor" | …     // caller's effective role on the target
  }
  ```

  External mounts today are indistinguishable from folders in the DTO; this
  closes that gap as a side effect.

- `FolderAncestorDto` gains `mount: Option<MountCrumb>` (the hop marker).
- `SharedWithMeItemDto` gains `mount_id: Option<Uuid>` and `declined: bool`.

### Config

- `OXICLOUD_ENABLE_SHARE_MOUNTS` (default `false`) — feature flag, same
  convention as `OXICLOUD_ENABLE_EXTERNAL_MOUNTS`. Off: no reconcile, mount
  rows filtered from listings, `resolve` treats mounts as absent. The change
  touches listing, both WebDAV surfaces, trash, copy, move, search and zip,
  so it ships opt-in for at least one release and flips to on once it has
  run on real instances.
- `OXICLOUD_SHARE_MOUNT_FOLDER` (default `""`) — where new mounts land.

## Consistency

The folders consistency pass (discovery-only, per `storage-consistency.md`)
gains these checks for rows with `mount_target_id IS NOT NULL`:

- I1–I5 hold (defence in depth over the triggers).
- The target exists and is not trashed; otherwise report `mount.dangling`.
- The recipient (`drives.default_for_user`) currently has Read on the target;
  otherwise report `mount.stale` (reconcile should have removed it).
- The target is not shadowed by a nearer mount of the same recipient
  (`mount.nested`).

Reports only; repair is reconcile, run explicitly.

## Migration and backfill

1. Schema migration: column, index, triggers, `share_mount_declines`,
   `auth.users.mounts_reconciled_at TIMESTAMPTZ NULL` (driver for the
   periodic job; NULL = never reconciled).
2. **No bulk backfill in the migration.** Mounts are created by the first
   reconcile of each user (login or periodic job). A 10k-user instance
   therefore pays the cost spread over the first day instead of in one
   startup transaction, and a user who never logs in again never gets rows.
3. The periodic job is cursor-paged over users; it never reports success for
   a user it skipped.

Rollback: set `OXICLOUD_ENABLE_SHARE_MOUNTS=false` (the default). Rows stay, are filtered
everywhere, and can be dropped later with the column.

## Testing

- **DB**: trigger tests for I1–I5 (`sqlx` tests in the repository module).
- **Service**: `ShareMountService` reconcile — direct grant, group grant,
  nested grant dedup, drive membership, decline, revoke, expiry, collision
  suffixing, share-folder creation.
- **AuthZ** (the ones that matter most):
  - R0: recipient A shares a personal folder containing mount M to B; B's
    listing has no M; B's `GET /folders/M` is 404.
  - A grant on M, a public link on M, a copy of M: all 409.
  - Resolving `/files/M/sub` as B (who has no grant on M's target): 404.
- **Path resolver**: hop through a mount in REST, native WebDAV PROPFIND
  Depth 1 (hrefs keep the client path), NC WebDAV PROPFIND and GET, MOVE
  across the hop rejected with 403/409 as today for cross-drive.
- **ETag**: change under target → mount etag and personal-root etag change.
- **Hurl**: `tests/api/share_mounts.hurl` end-to-end (grant → mount appears →
  rename → resolve by name → decline → remount), and an NC-compat scenario
  listing the user root with a mounted share.
- **Frontend**: page test for *All files* vs *Personal files* filtering.

## Phasing

- **P0 — mounts exist.** Schema + invariants, `ShareMountService` with
  reconcile and event hooks, R0 listing filter, R1 id-redirects, R2
  guards, DTO `mount` block, frontend badge on mount rows, *Shared with me*
  links to the mount. URLs still UUID. NC WebDAV shows mounts. Flag default
  **off**.
- **P1 — WebDAV paths.** Mount-aware `PathResolverService` for both WebDAV
  surfaces, *All files* / *Personal files*, etag propagation across mounts.
  After P1 the NC client story is complete.
- **P2 — hygiene.** Consistency checks, periodic reconcile job, share-folder
  config, zip of an ancestor includes mounted content for the recipient
  (optional).
- **P3 — later.** Single-file mounts; reshare permission; Photos/Music gain
  the same *Personal* filter.

## Decisions taken during design (so they are not re-litigated)

- **Mount into the personal drive, not a separate `/shared` namespace.** A
  second tree would recreate the "it exists but has no path" problem for
  anything not in it, and would need its own collision and naming rules.
- **Shared drives are mounted too.** One model for everything that is not
  mine. The sidebar drive picker becomes a shortcut to the mount.
- **Recipient-local naming with ` (2)` suffixing**, exactly like NextCloud.
  Not stateless disambiguation (`Team~072af8d2`): that changes a URL when a
  second same-named share arrives.
- **Mounts may live anywhere in the personal drive**, not root-only. The
  invariants that matter are I1–I3; "root only" would buy one check in
  move and cost the user their folder organisation.
- **Deleting a mount declines, it does not trash.** Trash must never hold
  a row whose "restore" has side effects outside the drive.
- **Folder URLs stay id-based** (maintainer decision): one shareable link per
  folder, the same for every recipient, stable across rename and move. No
  name-based SPA URLs.

## Open questions

1. ~~Trashing an ancestor with mounts~~ **Decided**: mounts are relocated
   to the share folder / root, always, no dialog (see R2). Decline is hide,
   never revoke; a recipient may revoke a direct grant explicitly.
2. ~~Declinable drive mounts~~ **Decided**: not declinable, like Group
   Folders.
3. ~~Personal filter~~ **Decided**: two sidebar entries as in NextCloud,
   one route, `?view=personal`.
4. ~~`drive.md` §9~~ **Decided**: §9 keeps its rationale; a pointer was
   added above its URL table and the two superseded rows are marked legacy.

## Appendix — observations from the external-mounts survey

Found while reading the external-mount integration as precedent; unrelated
to this proposal's scope but worth tickets:

- `list_folders_batch_with_perms` has no mount branch, so WebDAV `Depth: 1`
  on an external mount root lists files but no subdirectories.
- The `require_permission` pre-checks added 2026-09-14 in
  `list_folder_resources` and the download handler parse a UUID before the
  mount branch runs, so `ext:` child ids 404 over REST.
- Native and NC WebDAV handlers `Uuid::parse_str` the resolved DTO id before
  AuthZ, so paths below an external mount root 404 on both surfaces.
- Nothing stops trashing an external mount root or an ancestor of it; the
  registry is not reloaded on either.

Filed as #792, #793, #794 and #795.
