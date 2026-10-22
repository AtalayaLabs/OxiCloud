# Search type filter — categories, not extensions

Status: **PLAN**, nothing implemented. Written 2026-10-22 from Ed's UX report.

## What triggered this

Searching with a type filter "does not work" from the UI. Investigated — the
**file** narrowing is correct; what leaks is **folders**. Confirmed not a
regression from the dates/enum branches: `push_criteria_filters`' `file_types`
predicate and the SPA's `TYPE_EXT` / `typeFilter` have zero diff lines against
`main`, and `search_folders`' signature is identical on both.

Three defects, in increasing order of how much they cost:

1. **Folders ignore the filter.** `search_folders(parent_id, name_contains,
   caller_id, recursive)` takes no type parameter at all, so every
   name-matching folder is returned whatever the filter says. Only the file
   branch narrows. This is the one a user sees.
2. **The category→extension mapping lives in the SPA.** `TYPE_EXT` in
   `routes/search/+page.svelte` hardcodes 12 image and 7 audio extensions and
   the client sends `type=mp3,wav,flac,aac,ogg,m4a,opus`. Miss an extension
   (`.jfif`, `.m4b`, `.opus` variants, `.tar.zst`) and the file silently
   disappears from its own category.
3. **Extensionless files can never match.** The predicate is
   `LOWER(SUBSTRING(fi.name FROM '\.([^.]+)$')) = ANY($n)`, which is NULL for a
   name with no dot. Verified against the dev DB: rows named `recent` and
   `2002-07-07 22:22:22` exist and are unreachable by any category filter even
   when `mime_type` is known.

## The column already exists — do NOT add one

Ed asked whether to map a category column in the DB like the DTO has. **It is
already there**: `storage.files.category_order`, `smallint`, indexed as
`idx_files_category_order`, added by `20260527000001_files_category_order.sql`
— whose own header gives this exact reason, *"instead of evaluating a long CASE
WHEN mime_type LIKE '…' chain at runtime"*. Values are sparse multiples of 100
so categories can be inserted later.

Source of truth is `display_helpers::category_order_for(name, mime)`:

| value | category | value | category |
|---|---|---|---|
| 100 | Image | 800 | Archive |
| 200 | Video | 900 | Code |
| 300 | Audio | 1000 | Markdown |
| 400 | PDF | 1100 | Text |
| 500 | Document | 1200 | Installer |
| 600 | Spreadsheet | 9999 | Other / unknown |
| 700 | Presentation | | |

Live distribution (dev instance, 4093 untrashed files): 100→4053, 200→4,
300→9, 400→4, 500→10, 700→2, 1000→3, 1100→8. **0 NULL, 0 zero** — every write
path populates it correctly today.

So the filter becomes one indexed integer predicate instead of a mime chain:

```sql
fi.category_order = ANY($n)      -- documents → [400,500,600,700,1000,1100]
```

## Prerequisite: `category_order` is application-maintained, with no trigger

Checked `pg_trigger` on `storage.files` — twelve triggers, none of them touch
`category_order`. It is bound app-side at three sites in
`file_blob_write_repository.rs` (lines ~434, ~1045, ~1076) via
`category_order_for`.

**That has already failed once.** `copy_folder_tree`'s file INSERT never named
the column, so copied rows carried the default; fixed in
`20261103000009_copy_folder_tree_category_order.sql`. The deliberate choice
there was to derive the value from a sibling row with the same mime rather than
duplicate `category_order_for`'s table into SQL — which is the right call and
the reason this plan must not add a fourth copy of the mapping either.

Before the filter can be trusted, **audit every path that creates a
`storage.files` row** and confirm it sets `category_order`: the three bind
sites, `copy_folder_tree`, restore-from-trash, the NC chunked assemble, delta
commit, collab-created files, and any migration that inserts. The column being
clean on one instance today is not evidence the paths are all correct — it is
evidence that the paths exercised on that instance are.

A cheap durable guard was proposed here — a `CHECK (category_order <> 0)` or a
consistency-job detector counting rows whose stored value disagrees with
`category_order_for(name, mime)`.

> **SUPERSEDED (2026-10-22). Do not build the detector.** The `content_rescan`
> job in `docs/plan/blob-resniff.md` §B recomputes mime per content and so must
> recompute this same expression anyway — it both detects and *corrects*, where
> a detector only reports. Two readers of one denormalised column is how the two
> drift, which is the failure this guard was meant to prevent. The ownership of
> `category_order` freshness is recorded there, including why the fan-out has to
> be per row rather than per hash.
>
> The audit above also came back clean on a live instance (4093 rows, 0 NULL,
> 0 defaulted), and the column default is `9999` ("Other"), **not** `0` — so a
> missed write path degrades to Other and can never collide with the reserved
> folder sentinel. The detector guarded a hypothesis; `content_rescan` creates
> the real staleness, because retyping a file's mime without its category makes
> that row invisible under its own category in this very filter.

## Wire change

`type` stops being a list of extensions and becomes one category token.

```
before   GET /api/search?type=mp3,wav,flac,aac,ogg,m4a,opus
after    GET /api/search?type=audio
```

Accepted values, as a typed enum (§7's pattern — `SearchOrderBy` et al.), so an
unknown value is a 400 naming the set rather than a silent empty page:

```
all | folders | image | video | audio | document | archive | code | other
```

`document` maps to the set `[400,500,600,700,1000,1100]` — PDF, office, text,
markdown — because the UI's single "Documents" affordance is coarser than the
DB's grouping. **That mapping lives server-side only.** Putting it in the SPA
would just move the client-side guess from extensions to integers.

`folders` is the new value that fixes defect 1, and it needs no change to
`search_folders`: with any value other than `all` or `folders`, the file branch
runs and the **folder branch is skipped entirely**. `folders` inverts it. That
is strictly simpler than threading a type parameter through
`search_folders`, and it also makes "Images" exclude folders — which is what a
user picking Images means.

Breaking, with no deprecation window, on the same grounds §6 and §7 took: the
endpoint is recent and the SPA is its only consumer. The old form would not
silently change meaning — `type=mp3` becomes an unknown category and 400s, so a
stale client gets a loud error rather than wrong results.

## Steps

1. **Audit + guard** `category_order` write paths (above). Blocking: the
   filter is only as good as the column.
2. **`SearchTypeFilter` enum** in `search_dto.rs`, `ToSchema`, registered in
   `interfaces/api/mod.rs` — registration is required, not implied by
   `ToSchema`; see `project_openapi_enum_gotchas`.
3. **Map category → `Vec<i16>`** in one server-side function next to
   `category_order_for`, so both live in the same file and cannot drift.
4. **Swap the predicate** from the `SUBSTRING` extension match to
   `category_order = ANY($n)`, in both `push_criteria_filters` and
   `bind_criteria_filters` — the file comment says the two must stay in sync,
   so a change to one without the other is the bug to avoid.
5. **Skip the folder branch** unless the filter is `all` or `folders`.
6. **SPA**: delete `TYPE_EXT`, send the category token, add the "Folders"
   option to `TYPES`. Type the axis off the generated enum (§7 part 3g's
   pattern) so a stale token fails `npm run check`.
7. **Tests**: Hurl pinning each category returns only matching rows, that
   `folders` returns only folders, that a non-`all` filter returns no folders,
   and that an unknown value 400s naming the set. A `.opus` and an
   extensionless file with a known mime are the two fixtures that would have
   caught defects 2 and 3.

## Open question

Whether `other` (9999) should be offered in the UI at all. It is the bucket for
unknown mime, so it doubles as a way to find files whose type detection failed —
useful to an operator, noise to a user. Suggest shipping it in the enum (the
server must accept it for completeness) but leaving it out of `TYPES` until
someone asks.
