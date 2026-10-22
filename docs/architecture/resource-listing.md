# Resource Listing API Contract

Every OxiCloud endpoint that returns a **collection** of items must follow the conventions in
this document.  Consistency makes the REST API predictable for clients and keeps server-side
code easy to audit and extend.

## TL;DR checklist

- [ ] Response is `CursorListResponse<T>` → `{ items: T[], next_cursor?: string }`
- [ ] Query embeds `CursorQuery` via `#[serde(flatten)]`
- [ ] `limit` is clamped with `q.paging.limit_clamped()` — never trust the raw value
- [ ] Cursor is decoded with `q.paging.decode_cursor::<MyCursor>()` — invalid cursor → first page
- [ ] Cursor struct implements `PageCursor` (one bare `impl` line)
- [ ] Cursor includes **every column** in `ORDER BY` plus a unique tiebreaker (`id`)
- [ ] `order_by` param is present (as a typed enum) even if only one axis is meaningful today
- [ ] `order_by`'s enum is registered in `components(schemas(..))`, not just `ToSchema`-derived
- [ ] The keyset predicate and the `ORDER BY` name the **same** column, for every axis
- [ ] SQL fetches `limit + 1` rows to detect whether a next page exists

---

## Response envelope — `CursorListResponse<T>`

All listing endpoints return the same wrapper (defined in
`src/application/dtos/cursor.rs`):

```json
{
  "items": [ … ],
  "next_cursor": "eyJncmFudGVkX2F0IjoiMjAyNi…"
}
```

| Field | Type | Rules |
|---|---|---|
| `items` | `T[]` | The page of results. Length ≤ `limit`. |
| `next_cursor` | `string` | **Omitted** (not `null`) when this is the last page. |

**Never** include `total`, `page`, or `offset` — computing a total requires a `COUNT(*)` that
does not scale.

---

## Resource content field

When an item can be a **file or a folder** (or any future resource type), use a single
`resource` field rather than nullable `file`/`folder` siblings.  The existing
`resource_type` discriminator tells the client which shape to expect.

```json
{
  "resource_type": "file",
  "resource": { "id": "…", "name": "photo.jpg", "size": 204800, … },
  …
}
```

Adding a third resource type in the future only requires a new `resource_type` variant —
the wrapper shape stays the same, so older clients that don't know the new variant simply
skip the item.

### Rust — `ResourceContentDto`

```rust
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]   // ← serialises as the inner object; no wrapper key
pub enum ResourceContentDto {
    File(FileDto),
    Folder(FolderDto),
    // Playlist(PlaylistDto),  ← add future variants here
}
```

### JavaScript / JSDoc

```js
/**
 * @typedef {Object} MyListItem
 * @property {'file'|'folder'} resource_type
 * @property {FileItem|FolderItem} resource   // always present; shape follows resource_type
 */

const f = item.resource;
if (item.resource_type === 'folder') { /* FolderItem fields */ }
else                                 { /* FileItem fields  */ }
```

---

## Standard query parameters — `CursorQuery`

`CursorQuery` (in `src/application/dtos/cursor.rs`) carries the three fields every listing
endpoint needs.  Use it directly as `Query<CursorQuery>` when there are no extra filters.
When extra params are needed, **repeat the three fields** in your endpoint-specific struct
— Axum's query extractor uses `serde_urlencoded` which does not support
`#[serde(flatten)]`:

```rust
#[derive(Debug, Deserialize, IntoParams)]
pub struct MyQuery {
    // Standard cursor fields — repeated (not flattened) due to serde_urlencoded limitation
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
    // Typed per-endpoint enum, NOT a String — see §Sort values.
    pub order_by: Option<MyEndpointOrderBy>,
    #[serde(default)]
    pub reverse: bool,
    // Endpoint-specific extra
    pub status: Option<String>,
}
```

| Parameter | Type | Default | Constraints |
|---|---|---|---|
| `limit` | integer | 50 | 1–200; use `q.paging.limit_clamped()` |
| `cursor` | string | — | Opaque; absent on first page |
| `order_by` | enum | endpoint-defined | Typed per endpoint; 400 on unknown. See §Sort values |
| `reverse` | boolean | `false` | Flips the direction of whichever axis `order_by` selects |

### Naming conventions

- Use `order_by`, **not** `sort_by`, `order`, `orderBy`, or `sort`. (`sort_by` was
  the deviation on the two grant listings; §7 part 1 normalised it.)
- Values are **snake_case**: `granted_at`, `name`, `size`, `granted_by`.
- **Direction is the separate `reverse` boolean. Never append `_desc` to the
  axis.** The `_desc` suffix pattern was retired 2026-07-26 — §7 part 3e found
  dead `name_desc` / `date_desc` match arms still shadowing live code a year
  later, which is how folder sorting silently ignored four axes on the default
  search path.
- Default sort is the most natural recency order (usually `created_at DESC`).
- Make `order_by` a **typed enum** with `#[default]` on the endpoint's default
  variant, and register it in `components(schemas(..))` in `interfaces/api/mod.rs`
  — `ToSchema` alone emits a `$ref` to a schema that is not in the document.
  serde then returns **HTTP 400 naming the accepted variants** for an unknown
  value, with no hand-written guard to drift.

---

## Cursor design — `PageCursor` trait

Use **keyset (seek) pagination** — never offset-based pagination.

### Why not offset?

`OFFSET N` forces the database to scan and discard N rows on every page load, which becomes
unacceptably slow for large collections.  Keyset pagination skips directly to the right row
via an index seek, regardless of page depth.

### Implementing a cursor

`PageCursor` (in `src/application/dtos/cursor.rs`) provides `encode`/`decode` as default
methods.  A cursor struct needs only a bare `impl` line:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,                    // tiebreaker — must be unique
}

impl PageCursor for MyCursor {}      // encode/decode for free
```

**The cursor must include every column in `ORDER BY`** plus a unique tiebreaker so that two
rows with identical sort values never cause items to be skipped or repeated.

| Sort | Cursor fields |
|---|---|
| `created_at DESC` (default) | `created_at`, `id` |
| `granted_by ASC` | `granted_by`, `created_at`, `id` |
| `name ASC` | `name`, `id` |

Encoding is URL-safe base64url (no padding) over a JSON payload — opaque to API callers.
An undecodable cursor is treated as "start from the top" (never an error).

---

## SQL implementation

Fetch **`limit + 1`** rows.  If more than `limit` rows are returned, a next page exists:
truncate to `limit` and encode the last kept item as the next cursor.

```sql
-- Default: ORDER BY created_at DESC, id DESC
WHERE (
    $cursor_created_at IS NULL                                           -- first page
    OR created_at < $cursor_created_at
    OR (created_at = $cursor_created_at AND id < $cursor_id::uuid)
)
ORDER BY created_at DESC, id DESC
LIMIT $limit + 1
```

For an additional sort dimension (e.g. `order_by=granted_by`):

```sql
-- ORDER BY granted_by ASC, created_at DESC, id DESC
WHERE (
    $cursor_granted_by IS NULL
    OR granted_by > $cursor_granted_by
    OR (granted_by = $cursor_granted_by AND created_at < $cursor_created_at)
    OR (granted_by = $cursor_granted_by AND created_at = $cursor_created_at
        AND id < $cursor_id::uuid)
)
ORDER BY granted_by ASC, created_at DESC, id DESC
LIMIT $limit + 1
```

---

## Rust implementation skeleton

```rust
// ── DTO layer ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct ThingCursor { pub created_at: DateTime<Utc>, pub id: Uuid }
impl PageCursor for ThingCursor {}

#[derive(Deserialize, IntoParams)]
pub struct ThingQuery {
    #[serde(flatten)]
    pub paging: CursorQuery,
    pub status: Option<String>,
}

// ── Handler ──────────────────────────────────────────────────────────────────

pub async fn list_things(
    Query(q): Query<ThingQuery>,
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    let limit  = q.paging.limit_clamped();
    let cursor = q.paging.decode_cursor::<ThingCursor>();
    // Typed enum: `unwrap_or_default()` yields the #[default] variant, so
    // there is no string literal here to drift from the accepted set.
    let order_by = q.order_by.unwrap_or_default();

    // Service returns limit+1 rows; the cursor comes from the service layer
    // (it knows which columns to include based on the sort).
    let (rows, next_cursor) = state.service
        .list_things(auth_user.id, limit + 1, cursor, sort)
        .await?;

    Json(CursorListResponse::with_cursor(
        rows.into_iter().take(limit).map(ThingDto::from).collect(),
        next_cursor.map(|c| c.encode()),
    ))
}
```

---

## JavaScript consumption pattern

```js
// static/js/core/types.js
/**
 * @template T
 * @typedef {Object} CursorListResponse
 * @property {T[]} items
 * @property {string} [next_cursor]   // absent on last page
 */

// View module (e.g. sharedWithMeView.js)
let _cursor = null;
let _loading = false;

async function loadPage() {
    if (_loading) return;
    _loading = true;
    try {
        const params = new URLSearchParams({ limit: '50' });
        if (_orderBy) params.set('order_by', _orderBy);
        if (_reverse) params.set('reverse', 'true');
        if (_cursor)  params.set('cursor',  _cursor);

        /** @type {CursorListResponse<MyItem>} */
        const data = await fetch(`/api/things?${params}`).then(r => r.json());
        renderItems(data.items);
        _cursor = data.next_cursor ?? null;
        loadMoreBtn.hidden = _cursor === null;
    } finally {
        _loading = false;
    }
}

// Reset on section entry or sort change:
function reset() { _cursor = null; clearList(); loadPage(); }
```

---

## Sort values reference

The parameter is **`order_by`** and the direction is the orthogonal **`reverse`**
boolean. Neither a `sort_by` parameter nor a `_desc` suffix exists any more —
both were retired in §7 of `docs/plan/normalize-file-dates.md`; see that file
for why the axis and the direction must stay separate concerns.

`order_by` is a **typed enum per endpoint**, not a string. The accepted set
differs per endpoint — trash has `expires_at`/`trashed_at`, favorites has
`favorited_at`, recent has `accessed_at`, search has `relevance`, and the two
grant listings accept *disjoint* sets — so an unknown value is refused at
wire-decode time with a 400 naming the accepted variants. Do not add a shared
union: it would accept nonsense on every endpoint but its own.

| Axis | SQL ORDER BY | Notes |
|---|---|---|
| `name` | `folder_first ASC, lower(name), id` | Folders before files; see sentinel below |
| `type` | `type_order ASC, lower(name), id` | Category grouping; see the table below |
| `size` | `size ASC, id` | Folders carry `-1` so they cluster, not interleave at 0 |
| `created_at` | `created_at, id` | Row birth on this server — immutable |
| `modified_at` | `content_modified_at, id` | CONTENT clock: a rename or move does NOT move it |
| `accessed_at` | `accessed_at, id` | `/recent` only |
| `favorited_at` | `favorited_at, id` | `/favorites` only |
| `expires_at` / `trashed_at` | that column, `id` | `/trash` only; `expires_at` is a FUTURE instant |
| `relevance` | `relevance_score DESC` | `/api/search` only; `reverse` is a no-op |
| `owner` | `lower(username), id` | Cross-drive listings |

A keyset predicate and its `ORDER BY` **must name the same column**. Pairing a
predicate on one column with an `ORDER BY` on another silently skips or repeats
rows across pages — the defect §2 found on the folder listing. The per-endpoint
enums exist so that the query builder and the cursor builder cannot disagree
about the axis; keep both matches exhaustive, with no catch-all arm.

### `type_order` — the category axis, and the reserved folder sentinel

Sorting or filtering by `type` orders on `type_order`, which every UNION listing
projects from two sources:

| Range | Meaning | Source |
|---|---|---|
| **`0`** | **RESERVED — folder rows** | `0::bigint AS type_order` in the folder branch |
| `1`–`99` | **RESERVED — do not use** | headroom for future non-file resource kinds |
| `100`–`1200` | file categories (below) | `storage.files.category_order` |
| `9999` | Other / unknown mime | `category_order_for`'s fallback arm |

**`0` and the `1`–`99` band are reserved and must never be assigned to a file
category.** Folders have no row in `storage.files`, so each listing synthesises
their value — currently in six places: `folder_db_repository`,
`favorites_pg_repository`, `recent_items_pg_repository`, `trash_db_repository`,
and both grant listings in `pg_acl_engine`. A new category at `0` would merge
folders into it in every one of them at once, and the only symptom would be a
subtly wrong sort order.

File categories come from `display_helpers::category_order_for(name, mime)`,
which is the **single source of truth**. Values are sparse multiples of 100 so a
category can be inserted between two existing ones without a renumber:

| Value | Category | Value | Category |
|---|---|---|---|
| 100 | Image | 800 | Archive |
| 200 | Video | 900 | Code |
| 300 | Audio | 1000 | Markdown |
| 400 | PDF | 1100 | Text |
| 500 | Document | 1200 | Installer |
| 600 | Spreadsheet | 9999 | Other / unknown |
| 700 | Presentation | | |

Three rules for implementers:

- **Never duplicate the mapping.** It exists in Rust
  (`category_order_for`) and, unavoidably, in the backfill of
  `20260527000001_files_category_order.sql`. A third copy will drift. When
  `copy_folder_tree` needed the value it derived it from a sibling row with the
  same mime rather than re-encoding the table in SQL — follow that precedent.
- **`category_order` is application-maintained; no trigger sets it.** It is
  bound app-side at three sites in `file_blob_write_repository.rs`. Any new path
  that INSERTs into `storage.files` must set it — `copy_folder_tree` did not,
  and carried the default until `20261103000009` fixed it.
- **It is a sort key AND a filter key.** `GET /api/search?type=` narrows on it
  (`fi.category_order = ANY($n)`), so a stale value does not merely mis-sort a
  row — it makes that row invisible under its own category. Anything that
  rewrites `mime_type` must rewrite this in the same statement; that obligation
  is recorded in `docs/plan/blob-resniff.md` §B for the `content_rescan` job.
  The UI's coarse "Documents" maps to a *set* of these values, expanded
  server-side only, in `category_orders_for_filter` next to
  `category_order_for` — see `docs/plan/search-type-filter-by-category.md`.

Only expose axes meaningful for the resource type. `order_by` must always exist
in the query struct even when one value is supported today, so adding a second
is not a breaking change.

---

## Endpoint compliance

| Endpoint | Cursor | `order_by` | Status |
|---|---|---|---|
| `GET /api/grants/incoming/resources` | ✅ | 🔜 planned | **Reference implementation** |
| `GET /api/photos` | ⚠️ `before` header | ❌ | Non-standard — migrate to body cursor |
| `GET /api/search` | ❌ offset | ✅ | Migrate cursor |
| `GET /api/folders/paginated` | ❌ page | ❌ | Migrate cursor |
| `GET /api/folders/{id}/contents/paginated` | — | — | ✅ **Removed** — use `/api/folders/{id}/resources` |
| `GET /api/admin/users` | ❌ offset | ❌ | Migrate cursor |
| `GET /api/address-books/{id}/contacts` | ❌ offset | ❌ | Migrate cursor |
| `GET /api/shares` | ❌ page | ❌ | Migrate cursor |
| `GET /api/playlists` | ❌ offset | ❌ | Migrate cursor |
| `GET /api/recent` | — | — | ✅ **Removed** — use `/api/recent/resources` |
| `GET /api/files` | ❌ **none** | ❌ | Unbounded — **urgent** |
| `GET /api/folders` | ❌ **none** | ❌ | Unbounded — **urgent** |
| `GET /api/folders/{id}/listing` | ❌ **none** | ❌ | Unbounded — **urgent** |
| `GET /api/favorites` | — | — | ✅ **Removed** — use `/api/favorites/resources` |
| `GET /api/trash` | — | — | ✅ **Removed** — use `/api/trash/resources` |
| `GET /api/grants/incoming` | ❌ **none** | ❌ | Unbounded |
| `GET /api/grants/outgoing` | ❌ **none** | ❌ | Unbounded |

---

## Migration guide — offset/none → cursor

1. **Add `CursorQuery`** via `#[serde(flatten)]` to the query struct; remove `page`,
   `offset`, `per_page`.
2. **Define a cursor struct** with the `ORDER BY` columns + `id`; add `impl PageCursor`.
3. **Adjust SQL** to the keyset `WHERE` pattern; fetch `limit + 1`.
4. **Return `CursorListResponse`** built with `from_oversized` or `with_cursor`.
5. **Remove** `total`, `total_pages`, `has_next`, `has_prev` from the response.
6. **Update the JS caller**: remove page tracking, add `_cursor` state, pass it on
   "Load more", reset to `null` on section entry.
7. **Update `types.js`**: remove old pagination typedef fields, add
   `next_cursor?: string` to the response typedef.
