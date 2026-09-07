# Plan — Hidden system drive for user-owned objects

**Status:** design captured 2026-08-21. Not implemented. Sibling to
`docs/plan/derived-blobs.md`, which answers "where does *derived*
content live"; this one answers "where do *user-owned binaries* live".
The two share one rule — **point at a file, never at a blob** — and
that rule is the reason neither needs new blob-referencing tables.

## Problem — binaries in the users row

`auth.users.image TEXT` (migration `20260526000000_add_user_image.sql`)
holds the avatar inline, up to 512 KiB. It is the wrong home, and the
cost is already measured rather than theoretical:

- **It TOASTs, and every wide read pays.** The repository comment on
  `get_users_by_ids` records a group fan-out that "detoasted + shipped
  + parsed M avatars purely to discard them", fixed by adding a narrow
  projection (`benches/ROUND12.md §Q1`, `ROUND13.md §Q1`). That
  workaround exists *because* the column is in the wrong place; the
  rule it leaves behind — "add a wide sibling rather than widening
  this one back" — is a permanent tax on every future query.
- **Base64 inflation.** A ~384 KB image becomes ~512 KB of TEXT.
- **No dedup.** N users sharing a default or IdP-supplied avatar cost
  N copies.
- **None of the storage stack applies** — no `EncryptedBlobBackend`,
  no backend migration, no key rotation, no local cache, no
  consistency coverage.
- **Backups and replication carry it.** Binary weight lands in the
  logical dump and on every replica, forever.

The same pressure is coming for the UI background and a signature
image, so this needs a general answer, not another column.

## The rule — point at a file, never at a blob

```sql
ALTER TABLE auth.users
  ADD COLUMN avatar_file_id     UUID REFERENCES storage.files(id) ON DELETE SET NULL,
  ADD COLUMN background_file_id UUID REFERENCES storage.files(id) ON DELETE SET NULL,
  ADD COLUMN signature_file_id  UUID REFERENCES storage.files(id) ON DELETE SET NULL;
```

`storage.files` is already a `BlobReferenceSource`, already covered by
every consistency edge, already GC-integrated, already copy- and
version-aware. A pointer to a *file* therefore adds **zero new
reference sources and zero new consistency edges** — `auth.users`
holds no blob reference at all, only a pointer to a row that does.
Deleting the file decrements the blob refcount through the existing
file-deletion path.

Point at `id`, never at a name or path: a rename must not break a
profile.

For a small fixed set, **columns beat a table.** A table becomes right
only when the object set is open-ended, and it would cost exactly what
the pointer avoids.

### Rejected alternatives

| Option | Why not |
|---|---|
| **New table → blobs/chunks** | Needs a new `BlobReferenceSource`, a new fragment in the manifest sweep, and a new dangling check. Precisely the complexity the pointer removes. |
| **Direct backend paths** (`profile/{uuid}/avatar.png`) | `backend_migration` enumerates *blobs*, so a Local→S3 cutover **silently drops every avatar**. `EncryptedBlobBackend` is hash-keyed, so writes either bypass encryption or need a parallel path. `backend_consistency` raises `unknown_backend_file` (severity `anomaly`, "non-canonical file in blob namespace") per object on every sweep. And a fixed key overwritten in place breaks the immutability everything else rests on, killing `Cache-Control: immutable`. |
| **Reserved folder in the user's own drive** (`.profile/`, `.oxiprofile/`) | The user can write to it, so there are **two write paths and only one is validated** — the upload endpoint's format/size checks are bypassable over WebDAV and sync. It is in the sync tree, so "hidden" is not hidden in the protocols that matter. Existence-by-path drags back the extension problem. And any reserved name squats a namespace users own — `.profile` is a POSIX shell file, so it collides by default for anyone syncing a Linux home. |
| **One drive per user** | Per-user creation at signup, backfill for existing users, per-user quota exemption, and a cascade on account deletion. All avoidable — see below. |

Underneath all of it: these objects are owned by the **application on
the user's behalf**, not by the user as documents. Putting them in a
document tree conflates the two, and every problem above follows.

## The hidden system drive

**One shared drive, not one per user.** `kind = 'system'`, alongside
today's `CHECK (kind IN ('personal', 'shared'))`. Files inside are
owned by their respective users via the normal `created_by` /
ownership columns; the drive is only a container.

Sharing one drive drops per-user creation at signup, backfill for
existing users, and per-user quota exemption. Deleting a user becomes
a query over `storage.files` rather than a drive cascade.

Properties it needs:

- **Hidden at drive enumeration.** This is the single filter point,
  and it is why the drive beats a folder: a folder must be filtered in
  directory listings, search results, recent items, trash, photo
  indexing and sync deltas, whereas a drive is filtered once where
  drives are listed. Every surface must honour it — REST, WebDAV,
  NextCloud, search, quota reporting. **A missed filter is the
  characteristic bug of this design**, so it deserves a test per
  surface rather than per call site.
- **Trash disabled.** Otherwise every replaced avatar lands in a trash
  nobody can see, holding a blob reference that GC cannot reclaim
  while retention keeps it alive — invisible storage growth with no
  signal. Deletion here is immediate.
- **Exempt from the user quota envelope.** Nobody should pay quota for
  their own avatar.
- **Created at install, fail-fast at boot.** If the drive is missing,
  panic rather than silently disabling profile objects — a silently
  absent avatar surface is worse than a refusal to start.
- **Visible to admins.** Ops need to see it for storage accounting
  even though it is hidden from users.

## Visibility — per kind, in code

Reads go through a service method carrying an explicit policy, audited
like any other authorization decision. Because the column set is fixed
and small, the policy is a `match`, not stored data — there is nothing
to misconfigure, and adding a column forces adding an arm:

| Object | Who may read | Why |
|---|---|---|
| `avatar` | **the same rule as profile visibility** | Not "any authenticated user". `AGENTS.md` has `user_profile.rejected` return **404, never 403**, for an external caller with no relationship, specifically so existence cannot be confirmed. An avatar endpoint answering 200 for any caller is an oracle around that control. |
| `background` | owner only | Nobody else has a reason to fetch it. |
| `signature` | owner only, plus the document render path | A handwritten signature is forgery material. It is "public" only in the sense that it appears on documents you may already read — which argues for rendering it into those documents, not exposing it as a directly-readable object. |

Note the consequence: the drive's own permission model is **not** what
governs these reads. The object lives in a drive and is read through a
different door. That is a deliberate choice, not an oversight — record
it so nobody later "fixes" it by granting cross-user drive access.

## What must NOT live here

> **If losing control of it is a security incident rather than a
> cosmetic bug, it stays in the database.** The system drive is for
> user-facing binaries.

So E2E/Vault key material — public key bundle, passphrase-wrapped
private key, recovery kit — stays in `auth.users` columns. Four
reasons, the last decisive:

1. **Failure-mode asymmetry.** The characteristic bug here is a missed
   listing filter. For a wallpaper that is cosmetic; for key material
   it is disclosure.
2. **Atomicity.** Rotating a passphrase rewraps the private key
   *together with* the credential change. A DB column makes that one
   transaction; a file write plus a column update cannot be atomic.
3. **Size.** A few KB — blob storage buys nothing.
4. **`EncryptedBlobBackend` encrypts under a key the server holds.**
   For E2E material the whole premise is that the server *cannot*
   decrypt. Routing a wrapped private key through the blob layer
   encrypts it twice, once under a key the operator controls, adding
   no protection while creating the impression of it.

Users who want to store genuinely private *files* already have the
personal drive, with the full AuthZ engine behind it. There is no gap.

## Migrating the avatar off `auth.users.image`

Volume is one row per user, so unlike the thumbnail migration this
needs **no read-through phase** — a single batch job is enough.

**Phase 1.** Add the pointer columns and the system drive. Write path
switches to files; read path prefers `avatar_file_id` and falls back
to `image` when null.

**Phase 2.** `profile_image_import`, a registered `JobRegistry` job
(subject-first naming, per convention). For each user with a non-null
`image`:

1. Decode the data URI; skip and log if it does not parse, rather than
   failing the batch.
2. `store_from_stream` the decoded bytes → derived blob + manifest.
3. Insert a `storage.files` row in the system drive, owned by that
   user.
4. Set `avatar_file_id`.

Idempotent (`WHERE avatar_file_id IS NULL`), resumable via a user-id
cursor, and reports imported / skipped-unparseable / failed counts.

**Phase 3.** Drop `auth.users.image` and the fallback, gated on the
job reporting zero remaining. Dropping the column is what actually
reclaims the TOAST weight and retires the narrow-projection rule in
`get_users_by_ids`.

**IdP-sourced avatars.** OIDC login already refreshes the avatar
("same IdP avatar, already verified" — `user_pg_repository.rs:1435`).
That path must be converted at Phase 1, not Phase 3, or it keeps
writing to a column the migration is draining.

## Object catalogue

**Now:** avatar, UI background, signature image.

**Strong future candidates** — these are what justify a drive rather
than three columns and a corner:

| Object | Why it fits |
|---|---|
| **Data exports** (GDPR takeout, drive-export zip) | Generated async, large, downloadable, should expire. Today there is nowhere to put them. |
| **Staged imports** (Google Takeout, NextCloud export) | Multi-step ingestion needs durability beyond a temp file. |
| **Share-page branding / logo** | Per-user or per-org, served on public share pages. |

**Same problem, different owner — this drive does not help:**
`carddav.contacts.photo_url TEXT` (contact photos, today a URL or an
inlined data URI) and CalDAV `ATTACH` event attachments. They are keyed
by contact and by event, not by user. But the *pointer* generalises:
`contacts.photo_file_id UUID REFERENCES storage.files(id)` solves them
with no new blob-referencing table either — they simply live in the
address book's or calendar's own drive rather than here. Own plan.

## Operational details

- **Replace must delete.** Write new file → update pointer →
  hard-delete the previous file. `ON DELETE SET NULL` protects the
  pointer when a file vanishes, but nothing deletes the old file
  because the pointer moved.
- **Validation lives at the endpoint** and is now the only write path,
  which is the point of not using a user-writable location. Enforce
  format, dimensions and size there.
- **Account deletion** deletes the user's files in the system drive
  explicitly; the pointer columns are on the row being deleted anyway.

## Non-goals

- **Secrets of any kind.** See the discriminator above.
- **Per-user system drives.** One shared drive; revisit only if
  per-user quota or trash semantics ever become necessary.
- **Contact photos and event attachments.** Same pointer pattern,
  different owner, different drive — separate plan.
- **A generic "user objects" API.** The column set is fixed and small
  on purpose. Reach for a table only when it demonstrably is not.

## Open questions

- Who owns the system drive row itself, and what does
  `drives_consistency` expect of a drive with no human owner?
- Does the signature object survive the "owner only" rule, or does
  document rendering need a broader read path than expected?
- Should exports live here or in a short-lived namespace with its own
  expiry, given they are the only candidate with a natural TTL?

## References

- `docs/plan/derived-blobs.md` — the sibling plan; shares the
  point-at-a-file rule and documents the consistency coverage matrix
  these objects inherit for free.
- `migrations/20260526000000_add_user_image.sql` — the column being
  retired.
- `migrations/20260802100000_drives_schema_additive.sql` — the
  `kind IN ('personal','shared')` constraint this extends.
- `src/AGENTS.md` — the backend-abstraction rules, and the
  anti-enumeration pattern the avatar visibility rule follows.
- Memory `project_drive_naming_and_vault_reservation` — "Vault"
  reserved for the future E2E kind whose key material this plan
  explicitly keeps out of the drive.
