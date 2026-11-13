# Links — shares in the recipient's tree, and a UUID path to every share

> **Status**: implemented on `vitas` (see § Implementation notes and known limits for deviations). Proposed 2026-11-01. Revises `share-mounts.md`. What that
> document calls a *mount* is called a **link** here; its primitive (a folder
> row with `mount_target_id`), invariants I1–I5, resolution rules R0–R3,
> placement and naming, declines, path resolution and ETag derivation stay.
> This document changes **when** links are created, adds a mode switch, and
> adds `@shared/<uuid>`. Answers open questions 6 and 7 of `share-mounts.md`.
> The `link` concept was handed over by the maintainer on #791 (2026-10-23).

## Context

Today a folder shared with a user has **no path**. It exists only in the
*Shared with me* list (`GET /api/grants/incoming/resources`); the native
WebDAV root and `/remote.php/dav/files/<user>/` show the personal drive, and
`/webdav/@drive/<sel>/` resolves only drives the caller holds a **drive**
grant on (`DrivePgRepository::query_readable_by`). A granted folder in
somebody's personal drive is unreachable over WebDAV: scope resolution fails
before AuthZ is ever asked. A NextCloud client sees shared drives only as
separate accounts (`user~<drive uuid>`, chosen on the login-flow drive
picker) and granted folders not at all.

The behaviour wanted is NextCloud's: every incoming share appears in the
recipient's tree, a name collision gets ` (2)`, removing it hides it, and it
can be brought back from *Shared with me*. The word *link* is used because
the same primitive will later serve user-placed shortcuts; for now links are
used **only** to put shares into the recipient's tree.

## Decisions

- **SPA URLs stay id-based** (`/files/<uuid>`). No name-based URLs.
- **`@shared/<uuid>` exists in every mode**, including `off`. It is the
  guaranteed path; links are convenience on top.
- **Links only mount shares** for now. No user-created links, no file links.
- **`auto` links every share**: folder grants and shared-drive memberships,
  no per-grant opt-in.
- **Placement is global**: the drive root or `OXICLOUD_SHARE_MOUNT_FOLDER`.
  Users cannot choose their own share folder yet; they can rename and move
  individual links (R2).
- **Collisions and removal behave as in NextCloud**: ` (2)` suffix, removal is
  a decline, restore from *Shared with me*.
- **Links are created on two kinds of event only** — a grant becoming
  effective and group membership — through a durable queue that database
  triggers fill in the event's transaction. No reconcile on login, no
  periodic reconcile; one explicit backfill when switching to `auto`.
- **An event names pairs, the job settles them.** Every (recipient, target)
  pair an event touches is brought in line with the *current* grants:
  readable and unlinked → create, unreadable and linked → delete. Order of
  events does not matter.
- **Losing access ends everything**: the link and its decline are deleted.
- **"Readable" means "holds a non-expired grant path"**, independent of the
  target's trash state. A trashed target keeps its link, hidden.

## `@shared/<uuid>` (native WebDAV)

```
/webdav/@shared/                      → one entry per folder or file shared with me
/webdav/@shared/<uuid>/<path below>   → content of that shared folder
/webdav/@shared/<file uuid>           → that shared file
```

- The listing names grant roots: granted folders, granted **files**, and
  shared drives' root folders. Files are included because a direct file
  share has no other path anywhere (the gap the maintainer raised on #791);
  links do not cover them. **UUID only, never a name**: two shares may carry
  the same name, and resolving by name would need either a suffix rule or
  the first-match-wins lookup `@drive/<name>` has today.
- Resolution: `<uuid>` may be **any folder or file the caller can Read**,
  not only a grant root; the listing shows grant roots, the path accepts any
  readable resource. For a folder take its `drive_id` and `path`, set
  `db_path = folder.path + "/" + rest`, and continue with the existing
  `resolve_path_in_drive` + `authz.require` steps unchanged. A file UUID
  accepts no `rest`. An unreadable and a non-existent UUID both answer 404.
  No new table.
- **The `<uuid>` segment is an address, not a resource of the caller's.**
  MOVE, COPY, DELETE, PROPPATCH and MKCOL *on* `/webdav/@shared/<uuid>`
  itself answer 403: a UUID cannot be renamed, and "delete the share from
  my view" is a decline, which has no WebDAV verb. Everything *below* the
  segment is ordinary, authorised against the caller's role on the target.
- Every verb that takes a path takes this form, including the `Destination`
  header of MOVE and COPY; moves between `@shared` and the personal drive
  are cross-drive moves and go through the existing policy gates.
- Hrefs in every response are built from the **requested** path
  (`/webdav/@shared/<uuid>/…`), never from `db_path`, as `@drive` does today
  and as the P1 href-identity gate requires.
- `getetag` of `/webdav/@shared/` itself: derived at read time from the set
  of listed roots and the greatest `tree_modified_at` among them, like the
  drive-listing pseudo-root; no row is written.
- With an empty `OXICLOUD_WEBDAV_DRIVE_LISTING_PREFIX` (`/webdav/` is the
  drive listing), `@shared` is matched before the drive selector, so a drive
  named `@shared` is reachable by UUID only.
- Listing `PROPFIND /webdav/@shared/` Depth 1: href is the UUID,
  `displayname` is the folder name. Same source as *Shared with me*,
  declined shares included.
- The prefix sits beside `@drive` and shares its caveat: a root folder named
  `@shared` in the default drive is shadowed by it. Root names starting with
  `@` should be rejected on create and rename (follow-up).
- NextCloud-compatible DAV: not exposed. NC clients render the href segment,
  not `displayname`, so they would show a list of UUIDs. Links are the NC
  answer.

## Links

### Mode

`OXICLOUD_LINKS_MODE` = `off` | `auto` (default `off`), replacing
`OXICLOUD_ENABLE_SHARE_MOUNTS`. `manual` is reserved for user-placed links.

| Mode | `@shared/<uuid>` | shares appear as links |
|---|---|---|
| `off` | yes | no; existing link rows are filtered from listings and treated as absent by `resolve` |
| `auto` | yes | yes |

### What stays from `share-mounts.md`

Unchanged, read there:

- § The model — a link is a `storage.folders` row with `mount_target_id`, in
  the recipient's personal drive; I1–I5 enforced by triggers.
- § Declined mounts — deleting a link records a decline; *Shared with me*
  shows declined items with a **Link** action that clears the decline and
  recreates the link. **Every** link is declinable, shared-drive links
  included (decided below; `share-mounts.md` still says otherwise).
- § Target placement and naming — the share folder
  (`OXICLOUD_SHARE_MOUNT_FOLDER`, default `Shares`, empty = root), reused if a
  folder of that name exists, ` (2)` suffixing of link names against the real
  unique index.
- § Resolution (R0–R3), § Path resolution with mount hops, § ETag
  propagation, § Consistency.

### When links are created

| Event | Pairs settled |
|---|---|
| Grant on a folder or drive **becomes effective**: inserted, or `expires_at` moved from the past to the future or to `NULL` (`set_expiry_for_subject` does this today), user subject | (that user, target) |
| Same, group subject | (every transitive member, target) |
| User added to a group | (that user, target) for every grant held by the group or an ancestor group |
| Group A added into group B | (every transitive member of A, target) for every grant held by B or an ancestor of B |

Nothing else creates a link: no reconcile on login, no periodic reconcile.
The one exception is the explicit backfill when the mode is switched to `auto`
(§ Mode changes).

**Settling a pair** (recipient R, target T) means:

1. R has a default personal drive, T is not in it (I4), and, for folder
   grants, R is not the grantor of every grant path to T (P0's
   `granted_by <> recipient`, which applies to folder grants only; drive grants carry no such filter, so a
   drive's creator keeps a link to their own shared drive, as in P0);
   otherwise skip.
2. R **reads T** — holds a non-expired grant on T itself, direct or through
   `caller_group_ids` (a folder grant on T, or a drive grant on the drive
   whose root is T) — evaluated now, not at event time. An ancestor grant does
   not count. **Trash state is not part of this test.**
3. Reads, no link, no decline → create the link (share folder, ` (2)`
   suffix). Reads, link or decline exists → nothing.
4. Does not read, link or decline exists → delete both.

Because every event is settled against current state, order between events
is irrelevant: a grant followed by a quick revoke settles to "no link" no
matter which row the job sees first, and a membership event cannot resurrect
a link whose grant is already gone.

**Every granted target gets its own link.** P0's `mount_worthy_targets`
excludes three things that only a reconcile can keep true, and all three are
dropped:

- a folder grant nested under another granted folder (revoking the outer
  grant would have to create the inner link);
- a folder grant inside a shared drive the recipient is a member of
  (leaving the drive would have to create it, joining would have to delete
  it);
- a **trashed target** — a reconcile that drops trashed targets deletes the
  link, and restoring the target would then have to recreate it. The link
  stays, hidden by R0 (§ Hidden links).

A recipient with a viewer role on drive *Team* and an editor grant on
*Team/Docs* therefore sees both `Shares/Team` and `Shares/Docs`, as a
NextCloud user with a Group Folder and a share inside it does.

**The queue is filled by database triggers, drained by a job.** `role_grants`
is written from five places today (`pg_acl_engine`, `drive_pg_repository`,
`folder_db_repository`, `subject_group_service`, the load seed) and there is
no grant service; a hook in each caller is a guarantee a reviewer must
remember. Instead, `AFTER INSERT OR DELETE OR UPDATE OF expires_at ON
storage.role_grants` and `AFTER INSERT OR DELETE ON
auth.subject_group_members` insert a row into `storage.link_events`
(`subject_type`, `subject_id`, `resource_type`, `resource_id`,
`created_at`, `cursor`). The row commits with the event or not at all. A
registered job drains the queue: it expands each row to pairs, settles them
cursor-paged (`cursor` = last recipient done, so a crash resumes where it
stopped; settling twice is harmless), and deletes the row when every pair is
done (`jobs-handling-recoverable-error.md`). This is the shape of
`tree_etag_dirty` and of the durable deletion intent behind
`backend_reclaim`; the job registry itself is in-memory and cannot hold
pending work. The job runs on a short interval (an indexed probe of an
empty table costs nothing); the grant and membership services also poke it
after commit so the common case is immediate. One path for a user and for a
10k-member group. Inline creation inside the event's transaction is not
used, for two reasons:

- the ` (2)` retry loop catches `23505` from the real index
  (`ShareMountPgRepository::create_mount`); inside a transaction the first
  violation aborts the whole transaction, so the loop would need a
  `SAVEPOINT` per attempt;
- a recipient-side failure (no free name after 100 attempts, a broken share
  folder) must not roll back the sharer's grant.

A name exhaustion is an outcome recorded for that recipient, not a job
failure. Until the job reaches a recipient, the share is in *Shared with me*
and at `@shared/<uuid>`. Today's P0 handler logs a failed mount at `warn` and
carries on, which is only acceptable while login reconcile exists to repair
it.

**Role changes.** A role change that is an `UPDATE` of the grant row emits
no event and needs none. A role change implemented as delete + insert in one
transaction emits two events that settle to "readable" — fine — but if the
two ever ran in separate transactions a decline could be cleared in between.
Role changes should stay a single `UPDATE`.

### When links disappear

As `share-mounts.md` § When mounts disappear, with reconcile replaced by the
revoking service:

| Event | Effect |
|---|---|
| Grant revoked, user removed from a group, group removed from a group, drive membership removed, **group deleted** (its membership rows cascade, so the trigger fires per member) | the pairs are settled: every affected user who no longer reads the target loses link **and decline**. Runs in every mode, `off` included |
| Grant expired | same, by the expiry sweep (below) |
| User deletes a link | decline recorded, row deleted, target untouched |
| Target hard-deleted | `ON DELETE CASCADE` (links and declines) |
| Target trashed | hidden by the listing filter, back when restored; **not** an event |
| Ancestor of a link trashed | links relocated to the share folder / root (R2) |

**Declines end with access.** A decline is keyed on (recipient, target), not
on the grant. Kept after a revoke, it would hide a later, unrelated re-share
of the same folder forever; NextCloud does not have this problem because a
new share is a new row. So losing every grant path to the target clears the
decline, and the next grant is a new offer. A second grant on a target the
user still reads (say, a direct grant added beside a group grant) changes
nothing: the user never lost access, the decline stands.

**Expiry sweep.** Expiry has no row change, so no trigger fires. The link is
hidden at once by the readability filter (R0), but the row still holds its
name (§ Hidden links). A periodic job settles the pairs of every grant whose
`expires_at` fell between its last watermark and now. Settling only deletes
here (an expired grant is not a path), so the sweep never brings back a
link the user removed; it is not a reconcile. A grant renewed after the
sweep ran is an `UPDATE OF expires_at`, which is a creation event again.

Security never depends on these deletions: a link is rendered only if the
caller can Read its target (R0/R1).

### Hidden links

A link whose target the user cannot currently Read is hidden from listings
(R0), but its row still holds its name under the unique index. Until it is
deleted, the user gets `name_conflict` creating a folder called like a
thing they cannot see, and a new share of that name lands as `X (2)` next to
no visible `X`. Cases and their bound:

| Why hidden | How long |
|---|---|
| grant revoked | until the revoke job runs (seconds) |
| grant expired | until the expiry sweep runs (its interval) |
| target trashed | until the target is restored or purged. Not deleted: restoring must bring the link back |
| mode `off` | until the mode is back to `auto`, or the rows are dropped (§ Mode changes) |

The trashed-target case is accepted; the listing should show such a link as
unavailable rather than hide it, so the name conflict is explicable
(follow-up, frontend).

### Mode changes

- **`off` → `auto`** runs a one-off **backfill job**: settle every
  (user, target) pair of every current grant. This is the P0 reconcile
  without its three exclusions, run once and explicitly, never on login.
  Without it, grants from before the switch would have links only for users
  who join a group later, and members of the same group would see different
  trees. Mechanism: the mode is an env var, so the switch is detected at
  boot by comparing it with `admin_settings.links.mode_applied`; a mismatch
  towards `auto` enqueues the backfill, which records `mode_applied` when it
  completes. A crash mid-backfill repeats it on the next boot; settling is
  idempotent, so that costs time, not correctness.
- **`auto` → `off`** hides link rows; creation stops, deletion (revoke,
  expiry) keeps running. Rows hidden in `off` still hold names. An admin
  action drops all link rows and declines for an instance that leaves `auto`
  for good. `POST /api/links` answers 404 `feature_disabled` in `off`, as
  the other flagged routes do.
- The same backfill job is the repair action for the consistency pass.

## Changes against the P0 code

- `OXICLOUD_ENABLE_SHARE_MOUNTS` → `OXICLOUD_LINKS_MODE`.
- Link creation and deletion leave `grant_handler.rs` (which calls
  reconcile after grant and revoke today and swallows its failure). Triggers
  on `role_grants` and `subject_group_members` fill `storage.link_events`;
  a new job drains it. No caller of the five `role_grants` writers needs a
  hook, and the missing grant service is no longer a prerequisite.
- `ShareMountService::reconcile` becomes "settle these pairs", used by the
  queue job, the expiry sweep and the backfill. The login hook is removed,
  and with it `auth.users.mounts_reconciled_at` and the planned periodic
  reconcile.
- `mount_worthy_targets` loses `NOT f.is_trashed`, the `NOT IN my_drives`
  exclusion and the `lpath @>` anti-join; keeps `granted_by <> $1` and
  `drive_id <> personal`.
- Revoke clears declines; a new expiry sweep job; `admin_settings.links.mode_applied`.
- I1 trigger checks `drives.default_for_user = <recipient>`, not
  `kind = 'personal'`.
- The NC login-flow drive picker and the `user~<drive uuid>` composite
  username are removed (separate PR; independent of the mode).
- Folder rename notifies recipients whose link name differs.
- `POST /api/mounts` → `POST /api/links` (same body, same use: restore a
  declined share).
- User-facing wording: *link* instead of *mount*. Type and table names in
  code may follow later; the behaviour does not depend on them.

## Open questions

None.

Decided (2026-11-01):

- **Every link is declinable, shared-drive links included**, on one
  condition: the link must be restorable from *Shared with me*. That holds:
  `GET /api/grants/incoming/resources` already lists drive memberships
  (`ResourceTypeDto::Drive`), and a drive link's `mount_target_id` is the
  drive's `root_folder_id`, so the decline fits `share_mount_declines`
  keyed on `(recipient_id, target_folder_id)` unchanged. *Shared with me*
  shows a declined drive with the same **Link** action as a declined folder.
  NextCloud forbids declining Group Folders because the mount is the only
  way to reach one; here a shared drive stays reachable through the SPA
  drive switcher and `@drive`, so declining loses nothing. The 409
  `mount_not_declinable` branch, its audit reason
  `drive_mount_not_declinable` and its UI go away. A member removed from
  the drive loses link and decline like any other recipient (§ When links
  disappear). Closes `share-mounts.md` open question 2 the other way.

- **Links live in the recipient's default personal drive only**
  (`drives.default_for_user`, one per user by
  `idx_drives_default_for_user_unique`). I1 is tightened from "a personal
  drive" to "the recipient's default personal drive", which makes I5
  effectively one link per (user, target). Whether a user may hold further
  personal drives (raised by the maintainer on #791) is not this plan's
  question; links never land in them. Closes `share-mounts.md` open
  question 5.
- **The NC login-flow drive picker is retired**, in every mode
  (`login_v2_handler.rs`, `drive_picker.html`, the composite
  `user~<drive uuid>` account). NC login always yields the default personal
  drive; shared drives reach NC clients through links in `auto`. In `off`
  they are not reachable over NC-compat DAV, which is the state before the
  picker POC. Native WebDAV keeps `@drive`.
- **Owner renames the target: the link keeps its name, the recipient is
  told.** As in NextCloud, a rename in the owner's drive never touches a
  recipient's tree. Recipients whose link name differs from the target's
  new name get a bell notification ("*Reports* was renamed to *Reports
  2026* by its owner; your link is still *Reports*"), sent from the rename
  path through the existing notification service, one per recipient link.
  Recipients whose link already carries the new name, and declined
  recipients, get none. *Shared with me* shows the target's current name
  beside the link name when they differ.

## Testing

- `@shared`: listing equals *Shared with me*, files included; GET and
  PROPFIND below a granted folder; GET of a granted file; an expired or
  revoked grant → 404; a UUID the caller has no grant on → 404
  indistinguishable from a non-existent one; MOVE/DELETE/PROPPATCH on the
  `<uuid>` segment → 403; member hrefs stay under `/webdav/@shared/<uuid>/`.
- Creation: grant to a user, to a group, drive membership; joining a group
  creates links for all its grants; adding group A into group B creates
  links for A's members; two same-named shares → `X` and `X (2)`; an own
  folder `X` → link `X (2)`; the same target via user and group grant → one
  link; a nested grant → its own link; a folder grant inside a member drive
  → its own link; a grant on a folder in the recipient's own drive → no
  link; a grant to a user without a personal drive → no link, no error.
- Renewal: a grant that expired, was swept, and had `expires_at` extended →
  link again.
- Removal: deleting a link declines it; logging in again does not recreate
  it; **Link** in *Shared with me* restores it; revoking deletes the link
  and the decline; a re-share after revoke shows up again although the
  user had declined the first one; a second grant while the first still
  stands leaves the decline alone; deleting a group removes its members'
  links; deleting a shared-drive link declines it, the drive stays in the
  drive switcher and `@drive`, **Link** in *Shared with me* restores it,
  removing the member deletes link and decline.
- Group removal: a user removed from G keeps links to targets still
  readable through another group or a direct grant.
- Trash: trashing the target hides the link; an unrelated event on the same
  pair while it is trashed does **not** delete it; restoring shows it again.
- Expiry: the sweep deletes the link and frees its name; it never
  recreates a declined link.
- Mode: `off` → `auto` backfill links existing grants and respects
  declines; a revoke while `off` still deletes the hidden row.
- Delivery: the `link_events` row is written in the event's transaction
  (rollback of the grant → no row); a crash mid-job resumes; grant then
  immediate revoke leaves no link; name exhaustion for one
  recipient does not fail the job or the grant.
- Aliases: MOVE (native, NextCloud) and native COPY of a resource onto
  itself through two links, or of a link onto its own target → 403, nothing
  deleted (`80_alias.hurl`); a PROPFIND of a folder holding another user's
  links, or of `Shares/` after the target is trashed, lists no link.
- Everything under `share-mounts.md` § Testing for R0, R1 and the P1
  href-identity gate.

## Implementation notes and known limits

Rulings made while implementing; where they differ from the text above, they win.

- **Reads T (step 2).** "R reads T" means a grant on T itself: a folder grant
  on T, or a drive grant on the drive whose root is T. An ancestor grant does
  not count, and the event-driven settle only settles grant roots.
- **Revoke then re-grant before the settle runs.** The decline is kept,
  because the job never sees the access as lost. A re-share right after a
  revoke stays hidden until the user restores it from *Shared with me*.
- **WebDAV path hops** are implemented on native WebDAV, NextCloud DAV and
  chunked NextCloud uploads. A path through a link the caller can no longer
  read answers as a plain miss (404).
- **MOVE/COPY across aliases.** A link and a path through another link (or
  `@shared/<uuid>`) can name the same resource. Native and NextCloud MOVE and
  native COPY compare both resolved sides by drive and path, including a link
  row's target, and answer 403 (`webdav.rejected`, reason
  `source_destination_overlap` or `link_target_overlap`) when they are the
  same resource or one contains the other. The check runs before any
  `Overwrite: T` delete.
- **DAV listings apply R0.** PROPFIND child listings (native and NextCloud)
  use the same link-row predicate as the REST listing: a link appears only to
  its recipient, and only while the target is readable and not trashed.
- **Own-drive targets.** When a link's target ends up in the recipient's own
  personal drive, settle deletes the link (and any decline); a share folder
  with no free name is a `NameExhausted` outcome, not an error.
- **ETags.** A folder's effective DAV etag is the greater of its own tree
  mtime and the mtime of the caller's readable link targets below it. A link
  row reports its target's etag. In `off` mode links contribute nothing, and
  neither do unreadable targets.
- **REST etags (R20).** `/api/folders` DTO etags stay plain and are not
  link-derived; the SPA ignores folder etags. A future REST etag consumer must
  go through `FolderService::link_etags`.
- **Etag limit.** If a grant expires before settle drops the link row,
  ancestor etags may not change, and they can return to an earlier value
  (ABA). This lasts only until settle removes the row.
- **Not implemented (R19, deferred).** NextCloud REPORT/SEARCH (favorites,
  search) does not walk through links: items under a link are missing from
  those results, and nothing leaks. Etags of `@shared` listings and of NC
  REPORT/SEARCH are not link-derived either.

### Operating reference

| Name | Meaning |
|---|---|
| `OXICLOUD_LINKS_MODE` | `off` (default) or `auto`; an unparsable value is logged as an error and links stay off |
| `OXICLOUD_SHARE_MOUNT_FOLDER` | folder under the personal root where links land (default `Shares`, empty = root) |
| `OXICLOUD_LINKS_SETTLE_INTERVAL_MS` | cadence of `links_settle` (default `2000`, min `200`) |
| `OXICLOUD_LINKS_EXPIRY_SWEEP_SECS` | cadence of `links_expiry_sweep` (default `600`, positive integer) |
| `OXICLOUD_ENABLE_SHARE_MOUNTS` | retired; ignored with a warning at boot |
| `links_settle` | drains `storage.link_events` and settles the named pairs; runs in every mode |
| `links_expiry_sweep` | queues events for grants that expired since the last sweep |
| `links_backfill` | on-demand; queues every current grant, records `links.mode_applied`; requires `auto` |
| `links_drop` | on-demand; deletes all links and all declines; requires `off` |

## Related defect

`/webdav/@drive/<name>` picks the first readable drive with that root name,
ordered by `LOWER(name)` without a tie-breaker
(`DrivePgRepository::query_readable_by`). Two readable drives with the same
name resolve nondeterministically, and the order can flip when the per-user
cache is rebuilt. This is why `@shared` accepts UUIDs only. Tracked
separately.
