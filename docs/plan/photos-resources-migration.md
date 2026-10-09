# Plan — `/api/photos` → `/api/photos/resources` (normalized envelope + shared ETag/cursor engine)

**Status:** draft 2026-10-04. Driven by the observation that `/api/photos`
is the one listing endpoint that still returns a bare array with ad-hoc
pagination (`X-Next-Cursor` header, numeric `?before` input). Every other
listing surface — favorites, recents, trashed, folder contents, incoming
grants, outgoing grants — speaks the `CursorListResponse<T>` envelope.
Normalising photos aligns the API; the follow-up §2 also makes the ETag
revalidation path that `/api/photos` carries today available to the whole
resources family for free.

| item | state |
|---|---|
| §1 Introduce `GET /api/photos/resources` returning the normalized envelope | **TODO** |
| §2 Lift ETag computation into a shared `CursorListResponse` helper used by every `/resources` endpoint | **TODO** |
| §2b Client-side `If-None-Match` + response-body reuse on top of §2's server ETags | **TODO** |
| §3 Make the sort axis configurable (`captured_at` default, `created_at` fallback) | **TODO** |
| §4 Hard-cut: remove `GET /api/photos` in the same release | **TODO** |
| §5 Shared `caller_accessible_drives()` SQL function to fix multi-grant duplicate rows across every listing | **TODO** |
| §6 `?kind=photo\|video` filter on `/api/photos/resources` | **TODO** |
| §6b `?drive_id=<uuid>` filter + supporting composite index on the photo listing | **TODO** |
| §7 Facet-filter axis — `?person_id=` now, `?keyword=` and `?location=` later | **PROPOSAL** |
| §8 Realtime push for new photos via message bus | **DEFERRED** |
| §9 Dedup `/api/photos/resources` rows by `content_hash` server-side | **DONE** |
| §9b Delete-UX for photos with sibling blobs (gallery promoting a sibling after trash) | **TODO** |

---

## §1 — `GET /api/photos/resources` — TODO

### Shape

Response matches the `CursorListResponse<T>` envelope:

```json
{
  "items": [
    {
      "resource_type": "file",
      "resource": {
        "File": { /* standard FileDto fields */ }
      },
      "width": 4032,
      "height": 3024,
      "sort_date": 1727893200,
      "captured_at": 1727893200,
      "orientation": 6,
      "has_gps": true
    },
    …
  ],
  "next_cursor": "<opaque-base64>"
}
```

- **Item variant** reuses `ResourceContentDto::File(FileDto)` — no new
  `Photo` variant on `ResourceTypeDto` / `ResourceContentDto`. The
  photo-specific fields land at the item level, siblings to `resource`,
  same pattern favorites uses for `favorited_at`.
- **`sort_date`** stays — it is the value the cursor keys off. See §3
  for how its derivation is parameterized.
- **`captured_at: Option<i64>`** — raw EXIF `DateTimeOriginal`, epoch
  seconds. `null` when the file carries no EXIF date. Lets the client
  distinguish "photo taken on this date" from "upload-date fallback"
  that `sort_date` collapses into one value today — needed for a UI
  that wants to show "taken on" vs "uploaded on" differently, or flag
  photos without EXIF.
- **`orientation: Option<i16>`** — raw EXIF orientation (TIFF 1-8
  enum). `null` when absent. The client applies a CSS rotate to the
  full-resolution image tile BEFORE the bytes arrive, eliminating the
  "appears sideways, flips" flash on `<img>` load in browsers that
  don't auto-rotate (Safari pre-13 historically, and any browser
  rendering a WebP that was transcoded without preserving the EXIF
  rotation tag). Note: **thumbnails already arrive display-correct** —
  the server-side thumbnail generator bakes rotation into the output
  pixels during the WebP/AVIF encode (`ThumbnailService`), so this
  field is strictly for the full-res lightbox / download viewer, not
  the grid tiles.
- **`has_gps: bool`** — derived server-side as
  `latitude.is_some() && longitude.is_some()`. **Presence signal
  only** — the raw coordinates stay off the main listing. Enables a
  "📍 has location" badge and a "photos with location" client-side
  filter without disclosing per-photo coordinates on every gallery
  tile. Important on public share links where a visitor seeing GPS
  for every photo would often be unintended by the sharer. For the
  Places map, continue to use `/api/photos/geo` which clusters coords
  server-side; for a specific photo's exact lat/lng, continue to use
  the per-item `/api/files/{id}/metadata` endpoint (full EXIF payload
  there is behind an explicit per-photo call, which is the right
  disclosure boundary).

### What's deliberately NOT on the listing

- **Raw `latitude` / `longitude`.** Per-photo coords stay off the
  gallery for the privacy reasons above. The Places map keeps using
  `/api/photos/geo` for clustered views; the per-photo detail endpoint
  surfaces exact coords when the caller explicitly asks for one photo.
- **`camera_make` / `camera_model`.** Low value in a grid scroll;
  belongs on a detail panel (lightbox "i" button) or a future
  "filter by camera" search facet. Not worth the per-row serialization
  on every gallery page.
- **ISO / aperture / shutter / focal length / flash.** Not stored in
  `storage.file_metadata` today, so moot.

### Cursor

Opaque base64 `PhotosCursor` (same shape family as `FavoritesCursor` /
`RecentCursor`):

```
PhotosCursor {
    order_by:   "captured_at" | "created_at",   // see §3
    direction:  "desc" | "asc",
    sort_value: i64,          // the row's sort axis value (epoch)
    file_id:    Uuid,         // tie-breaker on equal sort_value
}
```

Query parameters:

- `?cursor=<opaque>` — opaque, replaces the current `?before=<epoch>`
- `?limit=<n>` — same semantics (clamped 1..500, default 200)
- `?order_by=captured_at|created_at` — see §3; default `captured_at`

Deliberately NOT re-adding the raw numeric `?before=` — SPA is the only
consumer and the cursor contains everything the SPA used `before` for.

### Item-level fields carried from `storage.file_metadata`

The repo call `list_media_files` already returns `(w, h)` per row via a
`LEFT JOIN storage.file_metadata`. Three new columns to add to the
SELECT: `fm.captured_at`, `fm.orientation`, and the derived
`(fm.latitude IS NOT NULL AND fm.longitude IS NOT NULL) AS has_gps`.
Zero new joins — `file_metadata` is already in the graph for `(w, h)`.

The repo return type gains:
`list_media_files -> (Vec<File>, Vec<(i64, Option<i64>, Option<i16>, bool)>, Vec<(Option<i32>, Option<i32>)>, Vec<(bool, bool)>)`
or (preferred) a `PhotoRowMeta` struct per row so the parallel-vectors
pattern doesn't balloon with every new field:

```rust
pub struct PhotoRowMeta {
    pub sort_date: i64,
    pub captured_at: Option<i64>,
    pub orientation: Option<i16>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub has_gps: bool,
    pub is_favorite: bool,
    pub is_shared: bool,
}
```

Handler transform moves them from today's `PhotoDto { ..., width,
height }` to the resource-item `{ resource, width, height,
sort_date, captured_at, orientation, has_gps }` shape.

### Deprecating `FileDto::sort_date`

`FileDto::sort_date` is populated only by the photos endpoint today. Once
the normalized envelope carries `sort_date` at the item level, the field
on `FileDto` can be removed — one less schema-level smell (an optional
field that only ever populates on one route is noise everywhere else).

Grep targets before removing:
- `rg "sort_date" src/application/dtos/`
- `rg "sort_date" frontend/src/`

### FE blast radius

One call site in `frontend/src/lib/api/endpoints/photos.ts::listPhotos`
(~15 LOC): new URL, parse `items`/`next_cursor` out of the body, read
`items[i].resource.File` instead of `items[i]` directly. The Photos tab
rendering code paths already consume `FileDto` + `(width, height)` —
plumbing through from the new shape is a tiny restructure, not a
semantic change.

### Empty-result semantics

A user whose gallery is empty — or whose cursor has advanced past the
last item — gets **200 OK** with `{"items": [], "next_cursor": null}`.
Not 404. Consistent with every other `/resources` endpoint today
(favorites, recents, trash, folder contents). An HTTP error class is
reserved for actual failures (auth missing, invalid cursor, server
error); "there is nothing to show" is a legitimate state the client
can render as an empty-state hint ("No photos yet").

### Scope — `/api/photos/geo` is NOT touched

The `/api/photos/geo` endpoint stays unchanged. It serves a different
shape (clustered map markers, keyed by `(bbox, zoom)`) for a different
UX (Places map). Explicitly out of scope for this migration. It
already has its own `ToSchema`-annotated `GeoCluster` DTO (added in
the OpenAPI sweep); nothing more to do there.

### Interaction with §2b — upload invalidation

Uploads to Photos make the gallery listing stale. The browser's HTTP
cache (and §2b's SPA cache) revalidate correctly on next-fetch via
`If-None-Match`, so correctness is preserved — but the user waits a
round-trip to see their new photo. For upload-triggered events the
right UX is **prefix invalidation**:

```ts
// in apiFetch, on 2xx POST/PUT/DELETE whose URL matches an upload path:
etagCache.invalidatePrefix('/api/photos/resources');
```

A `POST /api/files/upload` of an image → wrapper drops every cached
`/api/photos/resources*` key → next `listPhotos` is a cold network
fetch returning the gallery WITH the new photo. The user sees it
immediately on first scroll instead of after a server round-trip
spent returning 200-with-fresh-body.

Alternative (handled by §2b's `apiFetchStaleWhileRevalidate`): show
the previous gallery immediately with the new photo missing, swap in
the fresh version when revalidation returns. Correct but confusing
after an explicit upload action. Prefer prefix invalidation on
mutating events; prefer stale-while-revalidate on passive
navigation.

**Deliberately NOT routed over the message bus.** A sibling browser
tab (or another device) that is viewing the Photos tab while a
different session uploads a photo will NOT receive a push
invalidation over the WS message bus. Reasons:

- The browser's HTTP cache carries `Cache-Control: no-cache`, so
  the next explicit navigation / tab-focus event triggers an
  `If-None-Match` revalidation on its own. The server returns 200
  with the fresh list that now includes the new photo. No push
  needed.
- Routing cache-invalidation events over the message bus would
  mean each `/resources` endpoint declaring a bus topic
  (`photos:updated`, `favorites:updated`, …), the FE subscribing
  per-tab, and the server emitting on every mutation. That's a
  real architectural axis with its own correctness surface
  (dropped messages, reconnect windows, cross-user leaks); not
  earned by the "sibling-tab staleness" cost it would address.
- The invalidation-only-on-mutating-client approach keeps the
  pattern simple: a tab that caused the mutation knows it caused
  the mutation and drops its own cached entries. A tab that
  didn't cause the mutation picks up fresh state at its next
  action. That matches how browsers already treat navigation
  elsewhere.

If sibling-tab realtime becomes a requirement for a specific
feature (e.g., real-time collab lists where two users edit the
same shared folder), that's a feature-specific push the collab
message bus already carries, not a cross-cutting cache concern.

### Test matrix

New hurl scenario `tests/api/photos_resources.hurl`:

| # | Setup | Action | Assertion |
|---|---|---|---|
| 1 | Admin, empty gallery | GET `/api/photos/resources` | 200 + `items == []` + `next_cursor` absent |
| 2 | Upload one photo with EXIF date | GET same | 200 + exactly one item + `width`, `height`, `sort_date`, `captured_at`, `orientation`, `has_gps` all present at item level |
| 3 | Upload a photo with no EXIF (synthetic PNG) | GET same | 200 + the EXIF-less item has `captured_at: null`, `orientation: null`, `has_gps: false` |
| 4 | Upload 10 photos | GET `?limit=5` | 200 + 5 items + `next_cursor` present. GET with that cursor → next 5 items, cursor absent on last page. No file_id appears on both pages. |
| 5 | Upload with GPS EXIF | GET with that file | `has_gps: true`. Raw lat/lng NOT in the response body (confirms privacy posture — grep `latitude`/`longitude` on body is empty). |
| 6 | Cursor mismatch | GET `?order_by=captured_at&cursor=<cursor-from-?order_by=created_at-request>` | 400 Bad Request (round-trip stability §3) |
| 7 | Multi-grant dedup (regression for §5 bug) | Upload one photo to a drive; grant the caller access via BOTH a direct user grant AND a group grant | GET → exactly ONE item in the response. The file_id appears once. |
| 8 | ETag reuse | GET; capture ETag; GET with `If-None-Match: <etag>` | 304 Not Modified + empty body |
| 9 | ETag invalidation on upload | GET; capture ETag; upload a photo; GET with `If-None-Match: <etag>` | 200 (not 304) + new photo in response |

Scenarios 7-9 pin the semantic properties the migration adds; 6 pins
the §3 cursor-integrity rule; 1-3 pin the item-level fields.

### Error envelope conformance

Errors returned by `/api/photos/resources` match OxiCloud's standard
error envelope with the `error_type` discriminator — same middleware,
same `AppError::from()` chain the other `/resources` endpoints use.
No surface-specific error shape. (The current `/api/photos` emits
`{"error": "..."}` on 500 only; the normalized shape fixes that
alongside the rest of the migration.)

---

## §2 — Shared ETag computation across every `/resources` endpoint — TODO

### Why

Only `/api/photos` emits an ETag today, computed from
`(before, limit, max_modified_at, count)`. Browsers reload the gallery
revalidate with `If-None-Match`, and an unchanged tab returns an empty
304 — avoids reshipping 500 photo tiles when nothing changed.

Favorites, recents, trash, folder contents all repeat the same read
pattern: paginated listing of resources that rarely change between
consecutive views. Each one would benefit from the same 304-on-reload
behaviour. Repeating the ad-hoc ETag code five times is the wrong
shape — pull it into one place.

### What

Extend `CursorListResponse<T>` (or a thin wrapper around it) to carry
a weak ETag computed from the inputs + outputs:

```rust
impl<T: Serialize> CursorListResponse<T> {
    /// Compute a weak ETag from cursor + limit + a per-row freshness
    /// signal + row count. Callers provide `fresh_signal` because the
    /// right signal differs per endpoint: for photos it's
    /// `max(modified_at)`; for recents it's `max(accessed_at)`; for
    /// trash it's `max(trashed_at)`.
    pub fn weak_etag(
        &self,
        cursor_input: Option<&str>,
        limit: usize,
        fresh_signal: u64,
    ) -> String { … }
}
```

Handlers call:

```rust
let response = CursorListResponse::from_oversized(rows, limit, cursor_fn);
let etag = response.weak_etag(cursor_input, limit, max_modified);
if request_matches_etag(&request_headers, &etag) {
    return not_modified();
}
let body = sized_json(..., &response);
body.headers_mut().insert(ETAG, etag);
body.headers_mut().insert(CACHE_CONTROL, "private, no-cache");
body
```

Three call-site LoC per handler. Five handlers = ~15 LoC total for the
whole feature.

### Not a hash of the serialized body

Deliberately NOT `blake3(serialized_body)`. Reasons:

- The point is to short-circuit **before** serialization — hashing the
  body would require serializing first, defeating the purpose.
- The response shape is stable; cursor + limit + freshness is a correct
  identity for "nothing changed on this page".

### Follow-ups out of scope

- Strong ETags (byte-for-byte identity) — overkill for listings.
- Cross-handler cache-key sharing (two endpoints with the same effective
  listing) — not something we have today.

---

## §2b — Client-side `If-None-Match` + response-body reuse — TODO

### Why

Server-side ETag is only half the picture: the body is only saved if
the client actually sends `If-None-Match` on refetch AND preserves the
previous body on 304. Today the SPA relies entirely on the browser's
built-in HTTP cache for both: `apiFetch` doesn't set an explicit
`cache` mode, so the browser auto-revalidates on cached entries with
`Cache-Control: no-cache`, and converts a 304 back into the cached
response for the JS consumer — transparent savings.

**That is usually enough.** The browser's HTTP cache honors
`Cache-Control` and `ETag` correctly; the SPA already benefits without
writing a single line of ETag-handling code. On a navigate-away-and-
back to the Photos tab, the second fetch resolves fast with no body
on the wire.

**When it is not enough** — three concrete cases this §2b addresses:

1. **Private / incognito browsing** — HTTP cache is session-scoped and
   smaller; eviction happens quickly on media-heavy sessions.
2. **Mobile under memory pressure** — browser caches get purged
   aggressively by iOS Safari and Chrome on Android. A gallery tab
   that was 304-fast an hour ago drops to full re-send on the next
   visit.
3. **Stale-while-revalidate UX** — show the previous `items[]` the
   instant the component mounts, THEN kick a background revalidation
   request. The browser's HTTP cache gives us the 304 fast path but
   not the "render immediately from cache, then revalidate" pattern —
   that needs explicit SPA control.

### What

Add a thin ETag-aware wrapper on top of `apiFetch` living in
`frontend/src/lib/api/`:

```ts
// frontend/src/lib/api/etag_cache.ts  (new file)
interface Cached<T> { etag: string; body: T; storedAt: number }
const store = new Map<string, Cached<unknown>>();

// LRU-ish, bounded by entry count (not bytes — most listings are
// small) with simple drop-oldest-on-overflow. Entries never TTL
// explicitly — revalidation keeps them fresh or evicts via overflow.
const MAX_ENTRIES = 32;

export async function apiFetchEtagged<T>(
  url: string,
  opts: RequestInit = {}
): Promise<{ body: T; from: 'network' | 'cache' | 'revalidated' }> {
  const prev = store.get(url);
  const headers = new Headers(opts.headers);
  if (prev) headers.set('If-None-Match', prev.etag);
  const res = await apiFetch(url, { ...opts, headers });
  if (res.status === 304 && prev) {
    return { body: prev.body as T, from: 'revalidated' };
  }
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  const body = (await res.json()) as T;
  const etag = res.headers.get('ETag');
  if (etag) {
    if (store.size >= MAX_ENTRIES) store.delete(store.keys().next().value);
    store.set(url, { etag, body, storedAt: Date.now() });
  }
  return { body, from: 'network' };
}
```

Consumers call it where they want the explicit layer:

```ts
// in frontend/src/lib/api/endpoints/photos.ts
export async function listPhotos(cursor?: string, limit = 200) {
  const q = new URLSearchParams({ limit: String(limit) });
  if (cursor) q.set('cursor', cursor);
  const { body, from } = await apiFetchEtagged<ResourcesEnvelope>(
    `/api/photos/resources?${q}`
  );
  // `from` lets a caller surface "cached" vs "fresh" if they care
  return body;
}
```

### Stale-while-revalidate variant (optional, same cache)

A second entry point for callers who want the previous body
immediately while a revalidation request runs in the background:

```ts
export function apiFetchStaleWhileRevalidate<T>(url, opts)
  : { stale: T | null; fresh: Promise<T> }
```

- `stale` is the cached body (null on first fetch)
- `fresh` resolves with the revalidated body
- Component shows `stale` immediately, swaps to `fresh` when it
  arrives. Visible UX win on tab-switch flicker.

### Invalidation on write

A mutation that changes a listing (upload a photo, favourite a
file, trash a folder) must drop the matching cache entries. Two
approaches:

- **URL-prefix invalidation**: on any `POST`/`PUT`/`DELETE` whose
  URL matches a known listing root, drop all entries whose URL
  starts with the mutation's "scope". E.g. a photo upload drops
  everything cached under `/api/photos/resources`. Simple, broad,
  works for the five `/resources` endpoints.
- **Explicit invalidation call**: `etagCache.invalidate('/api/photos/')`
  called from each mutating endpoint. More code, but zero
  false-positives.

Start with URL-prefix invalidation on `apiFetch`'s own write
methods. Explicit `invalidate()` is always available as the escape
hatch for exotic cases.

### Interaction with the browser HTTP cache

**The explicit cache does NOT replace the browser cache — both run.**
The explicit cache wins on hit (0 ms, synchronous `.get()`); on miss
or on explicit stale-while-revalidate, the fetch underneath still
benefits from the browser's `If-None-Match` auto-revalidation. Double
caching, but each layer is doing something different: the explicit
one is in-memory JSON-objects keyed by URL (fast reads, SPA state
awareness); the browser one is on-disk encoded bytes (persistent
across page reloads).

### Why not just use SvelteKit's data loader cache?

SvelteKit's `invalidate()` / `invalidateAll()` operates on `+page.ts`
`load` functions. OxiCloud's listings aren't loaded that way — they
fire from component code via `apiFetch`. Rewriting them as `load`
functions is a much bigger refactor than this ~80-LoC wrapper, with
no additional benefit for the specific case (listing endpoints with
cursor-paginated bodies).

### Scope — where to adopt

Not every endpoint benefits. Adopt where the saved re-transfer is
large OR the latency win is visible on tab-switch:

- `/api/photos/resources` — large body, hot tab-switch target
- `/api/favorites/resources` — small body but reload-heavy
- `/api/recent/resources` — small body, reload-heavy
- `/api/trash/resources` — small to large, less reload-heavy
- `/api/folders/{id}/resources` — most common nav; biggest cumulative
  win

Each one is a one-line swap `apiFetch` → `apiFetchEtagged` at the
endpoint-module layer; no component-level change.

### Not adopted — hot write paths

Collab bus events, DPoP proofs, every `POST`/`PUT`/`DELETE`. Pure GET
listings only.

### Gated on §2 shipping first

§2 wires ETag emission on the five `/resources` endpoints. §2b's
cache is a no-op without server ETags. Order: §2 → §2b.

---

## §3 — Configurable sort axis — TODO

### Why

Today sort is hard-coded `COALESCE(captured_at, created_at) DESC` — the
photographer's view: "when was this photo taken". A sibling view
useful for operators and some workflows is "when did this file arrive
on the server" (`created_at` only, no COALESCE). Mixing them silently
makes it impossible to answer either question cleanly.

Also, once the opaque cursor engine (§1) carries the sort axis, letting
the client pick is nearly free — the cursor already encodes
`order_by` for round-trip stability, so adding user-facing control is
just surfacing a query parameter that the engine already respects.

### What

Query parameter on `GET /api/photos/resources`:

```
?order_by=captured_at   # default — EXIF DateTimeOriginal, falls back to created_at
?order_by=created_at    # file row's created_at only, no EXIF COALESCE
```

Repo change: `list_media_files(caller_id, cursor, limit, order_by)` —
the SQL branches on the parameter to pick the ORDER BY column. Both
columns already indexed (file_metadata.captured_at + file_blobs.created_at).

Cursor validation: `cursor.order_by` must match the request's `order_by`.
A mismatch returns 400 — avoids silently drifting pagination when the
client flips the sort axis mid-scroll.

### Not planned

Arbitrary sort axes (filename, size, …) — Photos is specifically a
date-ordered view. Other axes are a Files-tab concern.

---

## §4 — Hard-cut: remove `GET /api/photos` in the same release — TODO

### Why

Only consumer is the SPA (one call site in
`frontend/src/lib/api/endpoints/photos.ts`). No external API clients
documented. The phased deprecation approach (keep old + add new +
Sunset header + remove later) costs maintenance for a transition period
whose only user is a codebase we fully control.

### What

Same PR that lands §1:

- Add `GET /api/photos/resources` route
- Update SPA's `listPhotos` to the new endpoint + shape
- Remove `GET /api/photos` route
- Remove `pub struct PhotoDto` from `photos_handler.rs`
- Remove `FileDto::sort_date` field (now item-level)

### Risk

Any external script using `/api/photos` directly breaks. Mitigation:
flag prominently in the changelog of the release carrying this. OxiCloud
does not advertise this endpoint as part of a stable third-party API
surface; the risk is "someone wrote a one-off script". Acceptable.

---

## §5 — Shared `storage.caller_accessible_drives()` SQL function — TODO

### Why

Multiple PG repos build the same CTE inline:

```sql
WITH accessible AS (
    SELECT d.id
      FROM storage.drives_effective d
      JOIN storage.role_grants g ON g.resource_type = 'drive'
                                 AND g.resource_id = d.id
     WHERE ( (g.subject_type = 'user'  AND g.subject_id = $1)
          OR (g.subject_type = 'group' AND g.subject_id IN
                  (SELECT storage.caller_group_ids($1))) )
       AND (g.expires_at IS NULL OR g.expires_at > NOW())
       AND <optional policy filter>
)
```

Call sites (grep for `caller_group_ids`):

- `file_blob_read_repository.rs::list_media_files` — the one that
  triggered the `each_key_duplicate` bug on the Photos page
- `file_blob_read_repository.rs` — two more methods using the same
  pattern (listing files, listing by scope)
- `drive_pg_repository.rs` — drive enumeration
- `folder_db_repository.rs` — folder listings

Each inline CTE emits the same drive_id **once per matching grant
row** — a user with both a direct `user` grant AND a `group` grant
on the same drive ends up listed twice, and every CROSS JOIN LATERAL
or sibling join downstream fans each file N times. The symptom
surfaces first on Photos because the FE's virtualized grid has a
keyed `{#each}` block that halts on duplicate keys. Other listings
likely have the same bug but don't throw — they just render
duplicates until the FE picks them up as a cosmetic issue.

### What

A single SQL function in a migration:

```sql
CREATE OR REPLACE FUNCTION storage.caller_accessible_drives(
    caller_id   uuid,
    policy_flag text DEFAULT NULL      -- e.g. 'include_in_photo_index', NULL = no policy filter
) RETURNS TABLE (drive_id uuid)
LANGUAGE sql STABLE AS $$
    SELECT d.id
      FROM storage.drives_effective d
     WHERE EXISTS (
         SELECT 1 FROM storage.role_grants g
          WHERE g.resource_type = 'drive'
            AND g.resource_id   = d.id
            AND ( (g.subject_type = 'user'  AND g.subject_id = caller_id)
               OR (g.subject_type = 'group' AND g.subject_id IN
                       (SELECT storage.caller_group_ids(caller_id))) )
            AND (g.expires_at IS NULL OR g.expires_at > NOW())
       )
       AND (
            policy_flag IS NULL
         OR (d.effective_policies->>policy_flag)::boolean = true
       );
$$;
```

Every call site becomes:

```sql
WITH accessible AS (
    SELECT drive_id AS id
      FROM storage.caller_accessible_drives($1, 'include_in_photo_index')
)
SELECT …  -- unchanged
```

### Properties the function pins

- **Distinct drive ids by construction** — `EXISTS` collapses the
  multi-grant fan-out into one row per drive.
- **STABLE** — no side effects, no clock reads inside; planner can
  cache per-query and inline.
- **One place to touch** when ReBAC or group semantics change.
  Today a change (`storage.role_grants` → successor table, group
  nesting rules, grant expiry semantics) means editing six repo
  files; after §5 it's one function body.

### Dependency ordering

§5 benefits the migration most when both land together — the
migration touches `list_media_files` anyway, refactoring to the
shared function at the same time avoids a two-step rewrite.

Alternative: hotfix the Photos CTE first (rewrite the inline CTE
to `EXISTS` form; one file, one commit) to unblock the Photos page
today, then do §5 later as a cleanup.

### Regression test

Add a hurl scenario or a repo-unit test for the multi-grant case:

- Set up a user with both a direct `user` grant AND a `group` grant
  (via membership) on the same drive
- Upload one photo
- Call `GET /api/photos` (or `/api/photos/resources` once §1 lands)
- Assert the response carries the file exactly once

Without this test the bug is invisible until a user hits it in the
UI — where it shows as either frozen scrolling (keyed each throws)
or duplicate tiles (lists without strict key enforcement).

---

## §6 — `?kind=photo|video` filter — TODO

### Why

Current filter at the SQL level is
`mime_type LIKE 'image/%' OR mime_type LIKE 'video/%'`. A photos
browse UX typically wants EITHER photos OR videos at a time, not
both interleaved — "Photos" tab, "Videos" tab, "All media" view.
Today the client has to post-filter in JS which wastes bytes and
breaks pagination counts (an `?limit=200` where half the rows are
videos the client throws away actually returned only 100 photos).

### What

Query parameter on `/api/photos/resources`:

```
?kind=photo            # mime_type LIKE 'image/%'
?kind=video            # mime_type LIKE 'video/%'
?kind=all              # default; both mime families (today's behaviour)
```

Server-side: the predicate branches on the parameter. The repo
query already fetches `mime_type`; adding a WHERE condition is a
one-line change in `list_media_files`.

Cursor must encode the `kind` for round-trip stability (same rule
as `order_by` in §3) — a cursor obtained with `?kind=photo` can't
be used against `?kind=video` without yielding wrong pagination.
Mismatch → 400.

### Default stays "all"

Keep the default behaviour of returning both image and video rows
so the migration doesn't silently change what today's client sees.
A future SPA change can opt into `?kind=photo` for the "Photos"
tab and `?kind=video` for a dedicated videos view.

---

## §6b — `?drive_id=<uuid>` filter + supporting photo index — TODO

### Why

The photo index spans **every drive the caller can see** that has
`policies.include_in_photo_index = true` — personal drive, every
shared drive the caller has a grant on, every secondary personal
drive. That's correct for the default "all your photos" view, and
matches how Google Photos / iCloud present a single stream across
all accessible sources.

But the UX also needs the opposite view: "photos in **this** drive".
Concrete cases:

- A user navigates into a shared drive (`#/files/@drive/Family`)
  and wants a "Photos in this drive" tab that mirrors the Files tab
  scope.
- An admin troubleshooting a specific shared drive wants to see
  only its media without the personal-drive noise.
- A future `/s/<token>` photo-sharing surface for a single shared
  drive needs to scope the listing to just that drive.

Without a server-side filter, the SPA has to either (a) fetch the
cross-drive feed and filter in JS — breaks pagination counts, same
problem §6 solves for `kind` — or (b) build a parallel endpoint
per scope. Both are wrong shapes for a feature the data model
already supports.

### What

Query parameter on `/api/photos/resources`:

```
?drive_id=<uuid>       # restrict to a single drive the caller has access to
                       # (omitted → cross-drive view, today's behaviour)
```

Server-side:

- **Access check first.** The `accessible` CTE already materialises
  the set of drive ids the caller can see. If the caller passes a
  `drive_id` that isn't in that set, treat the response as empty
  (`200 OK` with `items: []`) — same anti-enumeration shape as the
  rest of OxiCloud: a visible drive filter returns rows; an
  invisible one returns the same empty response as a drive that
  simply has no photos. No 403, no 404, no "this drive exists but
  you can't see it" leak.
- **Predicate.** Inside the LATERAL probe, the filter is a single
  `AND fi.drive_id = $N` layered on top of the existing accessible
  gate. The cross-drive case keeps iterating all accessible drives;
  the scoped case probes exactly one.

### Supporting index — no new work needed

The existing `idx_files_media_timeline_by_drive` (migration
`20260901000001`, partial covering index on `(drive_id,
media_sort_date DESC)` filtered to non-trashed image/video rows)
is **already the right shape** for the per-drive filter. In fact
the single-drive case is strictly more selective than the current
multi-drive query:

- Today: `CROSS JOIN LATERAL` iterates `N` accessible drives, each
  one a bounded `LIMIT k` scan on the same composite index.
- With `?drive_id=<one>`: a single bounded index scan — identical
  cost to one lateral probe out of `N`.

Pin this with an `EXPLAIN` check during implementation: the plan
for `?drive_id=<uuid>` must still show `Index Scan Backward using
idx_files_media_timeline_by_drive`, not a seq scan or a bitmap heap
scan. Benches/PHOTOS-TIMELINE.md's shape confirms the index can
serve a single-drive scoped query; the only risk is a future
refactor silently losing the `drive_id` leading key.

### Cursor integrity

Cursor must encode `drive_id` for round-trip stability (same rule
as `order_by` in §3, `kind` in §6, filters in §7). A cursor
obtained with `?drive_id=A` must not be reusable against
`?drive_id=B` or against the cross-drive view — mismatch → 400.
Prevents silent pagination drift when the client flips scope.

### Behavioural guarantee: drive-scoped stays live

A per-drive view is where "live" matters most — a user on
`#/files/@drive/Family` watching the Photos tab for that drive
expects new uploads to that drive to appear quickly. The same
`/resources` cache + `If-None-Match` revalidation pattern (§2 /
§2b) carries it: an upload to the drive invalidates
`/api/photos/resources?drive_id=<drive>` by URL-prefix match on
the mutating client, and sibling tabs pick it up on next
navigation. §8 would upgrade sibling-tab latency if it ever lands.

### Interaction with `?kind=` (§6)

The two filters compose: `?drive_id=A&kind=photo` yields only the
photos in drive A. Cursor encodes both; mismatch on either →
400. One handler-level validation, no additional server
plumbing — both are WHERE predicates on the same LATERAL probe.

### Test additions (append to §1 matrix)

| # | Setup | Action | Assertion |
|---|---|---|---|
| 10 | Two accessible drives A, B each with 3 photos | GET `?drive_id=A` | 200 + exactly 3 items + every item's `resource.File.folder_id` resolves to a folder in drive A |
| 11 | Caller has NO grant on drive X | GET `?drive_id=X` | 200 + `items: []` (anti-enum: same shape as a drive with no photos) |
| 12 | Cursor mismatch | GET `?drive_id=A&cursor=<cursor-from-?drive_id=B-request>` | 400 Bad Request |
| 13 | `?drive_id=` + `?kind=photo` compose | upload 2 photos + 1 video in drive A; upload 1 photo in drive B | `?drive_id=A&kind=photo` → 2 items |

---

## §7 — Facet-filter axis: `?person_id=` now, keywords and locations later — PROPOSAL

### Why

OxiCloud already has three photo-adjacent indexers:

1. **People** (`people_handler.rs::person_photos`) — face detection
   clusters photos by identified person. Today it has its own
   listing endpoint; could consolidate under this one.
2. **Places** (`places_service` + `/api/photos/geo`) — groups
   photos by GPS cluster.
3. **Future keyword / scene tags** — not implemented. A later
   AI-powered pipeline (CLIP, ONNX classifier, …) could emit
   `keyword=cat` / `keyword=beach` / `keyword=sunset` tags, stored
   as a separate table joined by file_id.
4. **Future named-location clustering** — reverse-geocode the
   `latitude`/`longitude` into "Paris", "France", "Louvre" and
   store as another indexed facet.

Each of those is a FILTER on the same photo-listing axis.
Reinventing a per-facet endpoint every time (`/api/photos/by-person/{id}`,
`/api/photos/by-keyword/{tag}`, `/api/photos/by-location/{slug}`)
would duplicate the pagination + ETag + sort + envelope boilerplate
across N routes. One endpoint with filter query parameters shares
all the infrastructure.

### What — `?person_id=<uuid>` as the first facet

Phase 1 (near-term): add `?person_id=<uuid>` to
`/api/photos/resources`. The repo query gains an inner join or
EXISTS clause against the face-clusters table, filtered by the
chosen person. Response shape unchanged — same `CursorListResponse`,
same item-level fields. Consolidates `person_photos` under one
endpoint; the standalone `/api/people/{id}/photos` route can be
deprecated or kept as an alias.

Phase 2 (future): add `?keyword=cat`, `?keyword=beach`, etc. when
the AI tagging pipeline lands. Repo joins against a `photo_tags`
table (schema: `(file_id, tag text, confidence real, source
text)`). Multiple `?keyword=` params would AND-combine (photo has
BOTH tags). An `?keyword_any=cat,dog` for OR-combine if needed.

Phase 3 (future): add `?location=<slug>` filters when the
reverse-geocoding pipeline lands. Repo joins against a
`photo_locations` table or an inline hierarchical name match on a
future `photo_locations_hierarchy` structure.

### What — cursor + ETag integrity across filters

Same rule as §3's sort axis and §6's kind filter: the opaque
cursor encodes every active filter (`person_id`, `keyword`,
`location`, `kind`, `order_by`), and a cursor obtained with one
filter set must not be accepted against a different filter set.
Mismatch → 400. Prevents silent pagination drift.

ETag is computed from the SAME inputs: `(order_by, kind, filters,
limit, max_modified, count)`. A filter change produces a different
ETag, so stale cached pages don't short-circuit on `If-None-Match`.

### Why "PROPOSAL" and not "TODO"

Only `?person_id=` has a stakeholder today (consolidate the
existing `person_photos` endpoint). `?keyword` and `?location`
depend on AI pipelines that don't exist yet. The AXIS is defined
here so when those pipelines land, the endpoint already has a
place for them — no re-architecture needed. But implementation
waits on actual data to filter on.

### Order of implementation

1. `?kind=` (§6) — lands with the migration or immediately after.
   Cheap, uses only `mime_type` which is in every row.
2. `?person_id=` — once §6 is in, extend the same cursor-filter
   serializer to carry person_id. Still a mechanical change.
3. `?keyword=` — gated on the keyword-tagging pipeline existing.
4. `?location=` — gated on reverse-geocoding + hierarchical
   location storage existing.

---

## §8 — Realtime push for new photos via message bus — DEFERRED

### Why

§1's "deliberately NOT routed over the message bus" subsection
chose cache invalidation + revalidation on next navigation over a
WS push. That stays the right default. But the UX gap it leaves
is real, and this section captures the design so when it is
earned by observed user need, there is no re-architecture
required — only implementation.

The gap: a user who is actively watching the Photos tab while
another source drops a photo into the index. "Another source"
means:

- A sibling browser tab under the same session uploading from the
  Files tab or from drag-and-drop into Photos.
- A desktop WebDAV sync client (Nextcloud desktop, Cyberduck,
  rclone) dropping a batch of camera-roll files.
- Another user with a grant on a shared drive uploading from
  their own session.

In every case, the server knows the moment the new photo lands.
The watching tab doesn't — it finds out either on next focus /
navigation (if §2b's cache listens to `visibilitychange`) or on
the next explicit action by the user. Latency can be minutes.

### Why deferred

Three reasons, in order of weight:

1. **The common case is already handled.** Browser revalidation
   on tab focus (effectively free via `Cache-Control: no-cache`)
   covers the "I was elsewhere and come back" scenario, which is
   the majority of sibling-tab staleness. Only "I am staring at
   the Photos tab while uploads happen somewhere else" needs
   realtime, and that is a narrower user population.
2. **A simpler intermediate exists.** A focused-tab poll —
   `/api/photos/resources?limit=1` with `If-None-Match` every
   60s while the tab is visible — gets 304s almost for free and
   hits the fresh case within one poll window. ~10 LoC of SPA,
   no server change. If this ever gets built, it covers 90% of
   §8's win at 5% of §8's cost.
3. **Message bus already carries real cross-cutting concerns**
   (collab edits, chunked-upload progress). Adding "listings
   became stale" as a bus event class is a new axis with its
   own correctness surface (dropped messages, reconnect windows,
   per-user fan-out costs). Not earned by this one feature
   alone; earned once a second or third listing endpoint also
   wants the same push.

### Complexity, if it ever lands

**Server-side: ~100-150 LoC.**

Piggyback on `FileLifecycleHook::on_file_created`
(`src/infrastructure/services/thumbnail_service.rs`) which
already fires on every new file. Filter to image/video mimes,
then publish on the bus:

```rust
if ThumbnailService::is_supported_image(ct)
    || video_frame.is_supported_video(ct)
{
    let item = build_photo_item_payload(file_id, blob_hash, …);
    // Per-user fan-out: emit to the owner, PLUS every user with a
    // photo-scope grant on the drive. Drive membership expansion
    // uses the same `storage.caller_accessible_drives()` function
    // §5 introduces, inverted: given a drive, which callers?
    for subscriber in subscribers_for_drive(drive_id, 'include_in_photo_index') {
        bus.publish_to_user(subscriber, "photos.added", item.clone());
    }
}
```

Three real tasks under the surface:

1. **Reconstruct the item payload** without re-running the full
   `list_media_files` query. Width, height, sort_date, captured_at,
   orientation, has_gps + inner FileDto. Cheap: all in hand at
   upload time or one `file_metadata` lookup away.
2. **Permission fan-out.** For personal drives this is one user
   (the owner). For shared drives it enumerates the direct and
   transitive-group grants with `include_in_photo_index = true`.
   Needs a bounded enumeration path — a shared drive with 500
   grants should not emit 500 synchronous bus publishes on the
   upload hot path. Use a background task or a batched bus
   broadcast primitive.
3. **Topic + subscription authz.** New topic namespace
   (`photos.added:<user_id>`) with a subscribe-time check: user A
   can only subscribe to its own topic; the server routes events
   on the server side using the fan-out above.

**Client-side: ~50-100 LoC.**

The hard part isn't receiving the push — it's deciding what to
DO with it given cursor pagination + virtualized scroll. Four
cases:

| Case | Where in the sorted stream | What to do |
|---|---|---|
| A | newer than every item currently rendered | prepend at index 0; show a sticky "N new photos" badge; let the user click to scroll + reset |
| B | between two items already in memory | sorted-insert in-memory; virtual scroller handles position via item-key stability |
| C | older than the last loaded page's cursor | ignore; the next natural scroll fetches its page server-side and includes it correctly |
| D | user is actively scrolling | don't mutate the viewport mid-scroll; buffer the event and apply on next idle |

Plus always: **dedupe on `file_id`** on receive. A push + a
simultaneous natural pagination fetch can race, both delivering
the same row; without dedupe, the keyed `{#each}` throws
`each_key_duplicate` — the exact bug that frozen the Photos page
on 2026-10-04 and that §5 fixes server-side. Client-side dedupe
is defense in depth.

Cursor state is **not** invalidated by the push: the cursor is
a sort_value + file_id, stable against inserts anywhere in the
stream. The complexity is only in the UX layer.

### Correctness semantics

A push is a hint, not a source of truth. A tab that misses a
push (reconnect window, WS hiccup, server restart) must still
converge:

- The next `apiFetchEtagged` revalidation returns the fresh
  list, picking up the missed row.
- The cache invalidation path (§2b URL-prefix drop) runs on the
  mutating tab's own writes regardless of bus state.
- Message-bus delivery stays best-effort; this feature cannot
  depend on exactly-once semantics.

### Design axes alignment

- **Resilience.** No invariant depends on push delivery; worst
  case the user waits for next revalidation. ✓
- **Security.** Fan-out MUST re-run the drive-visibility check
  per subscriber at emit time. A grant revoked between upload
  and emit must not leak the row to a now-unprivileged
  subscriber. The event payload MUST NOT carry raw coordinates
  or EXIF beyond what the listing endpoint returns today.
- **Performance.** Fan-out cost scales with recipient count per
  drive. For a 10k-user instance with a shared drive accessible
  to many, batch the publish or defer to an off-hot-path worker.
- **Privacy.** The push payload matches the `/resources` item
  shape — no new disclosure surface.

### What would move §8 from DEFERRED to TODO

- Observed user demand: operators or users report that watching
  a WebDAV sync land files in Photos feels broken.
- A second listing endpoint wants the same push semantics
  (`/api/favorites/resources`, `/api/recent/resources`), making
  the "listings-became-stale" bus event class earn its generality.
- The simpler §8-lite focused-tab poll ships first and user
  testing confirms it is insufficient.

Until then, §8 stays off the roadmap on purpose; §1's
revalidation path is the shipping answer.

---

## §9 — Dedup `/api/photos/resources` rows by `content_hash` — DONE

Shipped 2026-10-08. Policy pick: **within-drive dedup**, not
cross-drive — a blob visible via grants on two drives surfaces
once per drive. See the implementation note below for the SQL
shape and the regression test (`photos_resources.hurl` step 8d).

---

## §9b — Delete-UX for photos with sibling blobs — TODO

### Why

§9's `DISTINCT ON (blob_hash)` picks the newest file row per unique
blob within a drive. If the user trashes the gallery-visible
representative, the next natural refetch (scroll past the sentinel,
filter flip, new upload) picks the next-newest sibling of the same
blob and the "deleted" photo reappears under a different file row
and path. The user sees "I deleted this photo. 5 minutes later it
was back." — a confidence-destroying bug on an otherwise-correct
gallery dedup.

Observed 2026-10-08. Flagged in the §9 shipping notes as
non-blocking, because the dedup win is substantial and the
mitigation is a well-shaped follow-up rather than a design flaw in
§9 itself.

### Shape — three-layer mitigation

Build on §9's SQL + the existing `storage.blobs.ref_count`
infrastructure. Three layers; the earlier design's bulk-trash
layer dropped once the sibling-list shape made it unnecessary
(see rationale below).

0. **Route split — hardening prep (ships standalone before §9b
   proper).** The existing `GET /api/dedup/check/{hash}` multiplexes
   an admin-only `ref_count` field onto the user response via a
   role gate inside the handler. §9b replaces the gate with a
   route-level split so the two audiences can't share a wire
   shape at all:

   - `GET /api/dedup/check/{hash}` → **user-scoped**; the
     `ref_count` field is DROPPED entirely (not just omitted on a
     non-admin call). Response is 200 with the §9b siblings list
     (next layer), or **404** on no visible references. Replaces
     the previous `200 {"exists": false}` anti-enum shape with a
     cleaner HTTP-level anti-enum: not-found and not-accessible
     are indistinguishable.
   - `GET /api/admin/dedup/check/{hash}` → **admin-scoped**
     (middleware-gated under the `/api/admin/*` layer). Returns
     200 with `{ hash, existing_size, ref_count }` or 404 on
     blob-not-on-instance. `ref_count` is unconditional (not
     Option) on the admin route.

   Hurl migration:
   - `refcount_same_content_rewrite.hurl` (4 GETs) → moves to
     the admin route. Admin login already in place; `ref_count ==
     N` assertions keep working verbatim.
   - `derived_blob_copy.hurl` (4 GETs) → stays on the user route.
     Assertions flip: drop `jsonpath "$.exists" == true` (implicit
     in 200), keep `existing_size`, and the not-found case moves
     from `HTTP 200 + $.exists == false` to `HTTP 404`.

   No production FE caller to migrate — the FE uses
   `POST /api/dedup/check-batch` for upload-skip, which stays on
   its current path with its own response shape.

1. **On the listing — `has_blob_siblings: bool`** on
   `PhotoResourceItemDto`. One extra per-row `EXISTS` in the
   `list_media_resources` SELECT, backed by
   `idx_files_media_dedup_by_drive_blob` (`(drive_id, blob_hash,
   media_sort_date DESC, id DESC)` partial, shipped with §9).
   Index seek per row — ~0.1 ms × 60 rows = ~6 ms added on the
   listing hot path. Same order of magnitude as the two per-row
   EXISTS already running (`is_favorite`, `is_shared`).

2. **At delete time — `GET /api/dedup/check/{hash}` returns the
   caller-visible sibling file-rows.** The modal reads the list
   verbatim; the FE batches the ticked `file_id`s into the
   existing `POST /api/batch/trash` to perform the actual delete.
   Shape:

       // 200 (blob reachable by the caller):
       {
         "hash": "<blake3>",
         "existing_size": 12345,
         "siblings": [
           {
             "file_id": "aaa…",
             "name": "IMG_1234.jpg",
             "drive_id": "ddd…",
             "folder_id": "fff…",
             "can_delete": true,     // Permission::Delete
             "can_update": true,     // Permission::Update (rename/move)
             "can_share": true       // Permission::Share
           },
           {
             "file_id": "bbb…",
             "name": "IMG_1234.jpg",
             "drive_id": "eee…",
             "folder_id": "ggg…",
             "can_delete": false,    // Viewer on this drive
             "can_update": false,
             "can_share": false
           }
         ],
         "count": 2,         // length of `siblings[]`
         "limit": 500,       // SQL LIMIT applied to the enumeration
         "truncated": false  // true when the caller has > `limit`
                             //   visible siblings; FE renders a
                             //   "Showing N of M copies" affordance
       }
       // 404 (no visible references — not found OR not accessible)

   Field conventions:
   - Permission `can_*` fields mirror the `Permission` enum
     verbs (`Read`, `Create`, `Share`, `Comment`, `Delete`,
     `Update`, `Manage`) — same vocabulary as the AuthZ engine so
     the modal copy maps 1:1 to the server-side gate. `can_read`
     is omitted (always true — we only return rows the caller can
     read); start with `can_delete` + `can_update` + `can_share`
     and add more verbs when a modal action needs them.
   - `name` + `drive_id` + `folder_id` are enough for the modal
     to render a "Family Drive / Vacation 2024 / IMG_1234.jpg"
     breadcrumb against the drives store it already has; a
     resolved `folder_path` would need a second SQL join per row
     and isn't earned until the UX stretch demands it.
   - LIMIT 500 is a hard safety cap — pathological 5000-copy
     cases return `truncated: true` with the first 500. The FE
     renders a "Load more copies" affordance that re-fetches with
     a cursor param (shape TBD when the truncated case actually
     ships; the common 2–10 sibling path never hits it).

   AuthZ: the SQL walks `storage.files` on the hash + a
   caller-visibility predicate (reader+ role on the drive). The
   per-sibling `can_delete` / `can_update` / `can_share` fields
   project the Permission gate the AuthZ engine would evaluate at
   action time, so the modal is honest. The existing
   `user_owns_blob_reference` in `blob_handler.rs` doesn't fit —
   different predicate scope. A sibling query is a parallel
   addition.

   **No `POST /api/dedup/{hash}/trash-mine` layer-3 endpoint.**
   The siblings list hands every `file_id` to the FE, which calls
   the already-existing `POST /api/batch/trash` with the ticked
   subset. The caller explicitly names what to trash — no "server
   decides the scope" step, zero blast-radius risk. The earlier
   design's bulk endpoint existed only to compensate for a
   count-only response that didn't expose the file_ids; the
   siblings list obsoletes it.

### UX — the modal

The confirmation dialog reads the full siblings list and derives
counts locally:

    const deletable = siblings.filter(s => s.can_delete);
    const nonDeletable = siblings.filter(s => !s.can_delete);

Branches:

- `siblings.length === 1 && siblings[0].can_delete`:
  *"Delete this photo?"* — single-row flow, same as today.
- `deletable.length > 1 && nonDeletable.length === 0`:
  Render the deletable rows as a checked-by-default list
  (file name, drive + folder breadcrumb per entry) with
  "Delete {{n}} copies" + "Delete only this one". User can
  uncheck specific rows to narrow the trash set.
- `deletable.length >= 1 && nonDeletable.length >= 1`:
  Same list, with non-deletable rows rendered disabled (greyed,
  no checkbox, "Read-only on {{drive}}" tooltip). Button copy:
  *"Delete {{n}} selected copies"* — the ticked set only. The
  non-deletable rows are visible so the user knows other copies
  remain.
- `deletable.length === 0 && nonDeletable.length >= 1`:
  shouldn't arise from a delete click on a visible-and-deletable
  row — render as a disabled-delete affordance for safety:
  *"You can see this photo in {{m}} place(s) but can't delete
  any of them."*
- `truncated: true`: footer link *"Load more copies"* refetches
  with a cursor param. Common 2–10 case never shows this.

Trash flow: user confirms with a ticked set of `file_id`s → FE
calls `POST /api/batch/trash` with those ids verbatim. The
server-side batch endpoint already AuthZ-validates per file, so
unauthorized ticks (shouldn't happen given the `can_delete`
filter but defensive) 403 silently per row.

For batch delete across multiple selected items with siblings, the
flow is the obvious extension: call `check/{hash}` once per unique
hash in the selection, aggregate the sibling lists, present one
modal with the per-blob breakdown.

### Why not reuse `ref_count`

- Admin-only (safe default — leaks cross-user info otherwise).
  Layer 0 above makes this a route-level constraint rather than
  a field-level one.
- Global across every drive — not caller-scoped.
- Only hurl tests consume it today
  (`refcount_same_content_rewrite.hurl`,
  `derived_blob_copy.hurl`). The route split is a tiny migration
  for them; `ref_count` keeps the same semantics on the admin
  route after the move.

### Test matrix — regression hurl REQUIRED

Shipping §9b MUST land with hurl scenarios on
`photos_resources.hurl` (or a new `photos_delete_siblings.hurl`)
that pin:

1. **Route-split prep (layer 0, standalone commit before the rest):**
   - Admin login + `GET /api/admin/dedup/check/{hash}` on an
     accessible blob → 200 with `ref_count` present.
   - Regular user + `GET /api/dedup/check/{hash}` on an
     accessible blob → 200 with the new sibling-list shape, no
     `ref_count`.
   - Regular user + `GET /api/admin/dedup/check/{hash}` → 403
     (middleware gate).
   - Any caller + `GET /api/dedup/check/{hash}` on an
     unreachable hash → 404 (anti-enum).
   - **Token subject** (public-share link caller) +
     `GET /api/dedup/check/{hash}` → 404 (handler-level guard).
     Dedup checks are session-only; link visitors never probe the
     endpoint. Audit anchor on top of the AuthZ gate so the
     caller surface stays strictly bounded.
   - Migrate `refcount_same_content_rewrite.hurl`'s 4 GETs to the
     admin route verbatim; migrate `derived_blob_copy.hurl`'s 4
     GETs to the new shape (drop `exists`, keep `existing_size`,
     flip not-found from `$.exists === false` to `HTTP 404`).

2. **Siblings signal on the listing (layer 1):** upload two copies
   of the same fixture under different folders → the surviving
   listing item has `has_blob_siblings === true`.

3. **Siblings list — happy path (layer 2):** upload the same
   fixture into two folders on the same drive, then
   `GET /api/dedup/check/{hash}` as the uploader:

   ```hurl
   GET {{base_url}}/api/dedup/check/{{photo_hash}}
   Authorization: Bearer {{alice_token}}

   HTTP 200
   [Asserts]
   jsonpath "$.hash"              == "{{photo_hash}}"
   jsonpath "$.existing_size"     exists
   jsonpath "$.count"             == 2
   jsonpath "$.truncated"         == false
   jsonpath "$.siblings"          count == 2
   # Every row carries file_id + name + drive + folder:
   jsonpath "$.siblings[0].file_id"    exists
   jsonpath "$.siblings[0].name"       exists
   jsonpath "$.siblings[0].drive_id"   exists
   jsonpath "$.siblings[0].folder_id"  exists
   # Permission flags mirror the Permission enum vocabulary:
   jsonpath "$.siblings[0].can_delete" == true
   jsonpath "$.siblings[0].can_update" == true
   jsonpath "$.siblings[0].can_share"  == true
   # Both ids are the expected two file-rows:
   jsonpath "$.siblings[*].file_id" contains "{{photo_file_id_a}}"
   jsonpath "$.siblings[*].file_id" contains "{{photo_file_id_b}}"
   ```

4. **Visibility asymmetry (layer 2):** grant a second user Viewer
   on one of the drives containing a copy; from the second
   user's perspective:
   - The 200 shape still fires because the caller has at least
     one visible row.
   - `jsonpath "$.count" == 1`
   - `jsonpath "$.siblings[0].can_delete" == false`
   - `jsonpath "$.siblings[0].can_update" == false`
   - `jsonpath "$.siblings[0].can_share"  == false`

5. **Bulk-trash via the existing batch endpoint:** `POST
   /api/batch/trash` with the two `file_id`s from scenario 3 →
   both trashed. Subsequent `GET /api/dedup/check/{hash}` returns
   **HTTP 404** (no visible references after batch-trash). The
   admin `GET /api/admin/dedup/check/{hash}` still returns 200
   with `ref_count` reflecting the trashed siblings' ref drop
   (admin surface is global, not caller-scoped).

6. **Admin diagnostic surface unchanged:** `refcount_same_content_rewrite.hurl`
   keeps asserting `jsonpath "$.ref_count" == 1` after the route
   migration. Keeps the §9 admin diagnostic contract intact.

### Hot-path cost summary

- Listing: +~6 ms per 60-row page (one index-only EXISTS per row).
- Delete click: +10-50 ms one-shot `/api/dedup/check/{hash}` call
  per unique hash in the selection — never on the scroll loop.
- Bulk-trash call: one roundtrip at delete-confirmation time,
  returns a count.

### Scope ambiguity resolved alongside shipping

- The §9 "cross-drive dedup" question stays answered as
  "within-drive only" — no change in §9b.
- The §9b sibling counts (both `accessible` and `deletable`) ARE
  cross-drive by nature, so `trash-mine` can delete copies in
  OTHER drives the caller writes to. The modal copy must say
  this clearly ("Delete all 5 copies across every drive you can
  trash from") so the user isn't surprised when a photo gone from
  the gallery is also gone from their Files tab in a shared drive.

### Why TODO, not PROPOSAL

The design converged in-session; the three layers above are the
answer and nobody has flagged a competing shape. The gating is
scheduling + a hurl author seat, not design.

---

## §9 — Dedup `/api/photos/resources` rows by `content_hash` — PROPOSAL

### Why

Observed 2026-10-08 on a live gallery: a user who copied the same
image into N folders — or ran a bulk import that fanned the same
blob across folders / drives — sees it N times on the Photos tab.
In one test case a user with ~5000 file rows pointing at a single
blob saw 5000 near-identical tiles. The server returns one row per
file-record, which is the right semantic for every other listing
(the Files view, trash, favourites, share surface) — a user who
organised two copies of a photo into two folders cares about both
rows. For a photo gallery whose unit is "a captured moment", not
"a storage row", the N-copies display is noise.

### What

Collapse the `/api/photos/resources` row set at the SQL layer so
each `content_hash` surfaces exactly one representative. The
natural shape is `DISTINCT ON (fi.blob_hash)` on the inner lateral
probe, ordered by `media_sort_date DESC, fi.id DESC` so the row
that wins is the newest copy — same semantic a client-side
first-occurrence-wins pass would pick, pinned at the server.

Cursor + ETag integrity: `PhotosCursor` already keys off
`(sort_value, file_id)` and the ORDER BY on this query is already
`(media_sort_date DESC, id DESC)`, so DISTINCT ON preserves a
valid keyset position. The weak ETag's `row_count` + `next_cursor`
+ `fresh_signal` all compute off the deduped row set naturally —
no formula change.

### Scope ambiguity worth resolving before implementation

- **Cross-drive dedup.** A blob visible via grants on two drives
  (personal + shared) — collapse into one tile, or show one per
  drive? Two defensible readings:
  - *One moment, one tile* → fully-global DISTINCT. Matches
    iCloud / Google Photos. Hides provenance.
  - *One moment per visibility scope* → DISTINCT within drive but
    not across. The shared drive's copy and the personal copy
    both show, with their own `drive_id`. Clicking either goes
    to its grant-scoped lightbox.
  Default proposal: *within-drive dedup, keep across drives.*
  Covers the common case (user organised a photo into folders in
  one drive) without hiding cross-drive copies the user might
  legitimately want to interact with. Revisit when the first
  cross-drive-copy complaint arrives.

- **Lightbox + delete UX.** If the gallery shows one of N copies
  and the user deletes it, the next pagination refresh promotes
  a sibling. The user watches the photo "come back". Needs a
  story: either a one-click "delete every copy of this blob"
  (which honours per-file grants) or a UI tell that explains
  "there are N copies — [manage]". Can land after §9 ships (not
  blocking), but worth flagging in the shipping release notes
  so users aren't surprised.

### Follow-up — FE cleanup

The current FE has NO client-side dedup (reverted in `ebb15488`
after discussion). Once §9 ships server-side, there is nothing
to delete on the FE; the gallery is correct by construction.

### Test matrix — regression hurl REQUIRED on shipping

Shipping §9 MUST land with a hurl scenario on
`photos_resources.hurl` that pins the invariant:

1. Upload one JPEG into a drive's root folder → capture its
   `file_id` + `content_hash`.
2. Copy the file into a sibling folder (or re-upload the same
   fixture at a second path — content-addressing dedupes at the
   blob layer, each upload produces a distinct file row).
3. `GET /api/photos/resources?limit=50` as the owner.
4. Assert `jsonpath "$.items" count == 1` AND the surviving
   `$.items[0].resource.id` matches ONE of the two file ids
   (not both).

Plus a cross-drive sibling scenario once the within-vs-across
policy is pinned:

5. Upload the same fixture to two drives the caller sees
   (personal + a shared drive opted into the photo index).
6. `GET /api/photos/resources?limit=50` → assert
   `count == 2` under within-drive dedup (the two copies share
   content but live in different scopes) OR `count == 1` under
   fully-global dedup. Mismatch → policy regression.

### Why PROPOSAL

The user-facing need is confirmed. The design has one real open
question (cross-drive policy) that wants an explicit answer
before the SQL lands — picking it after shipping would risk a
visible behaviour change on the gallery. §9 lands on the
roadmap once that policy is chosen.

---

## Priority

0. **Hotfix** before anything else — the inline `accessible` CTE in
   `list_media_files` already produces duplicate rows on multi-grant
   users and that's what froze the Photos virtualized grid on 2026-10-04.
   Rewrite to `EXISTS` (one commit, one file) and ship. Everything
   below is proper engineering; this zero-th step is incident response.
1. **§1 + §4 + §5** together — the normalisation PLUS the shared
   accessible-drives function. §5 is a natural companion to §1
   because the migration touches `list_media_files` anyway, and
   doing both at once avoids a two-step rewrite.
2. **§2** — shared ETag engine. Fits any time after §1 lands; retrofits
   the four other `/resources` endpoints independently.
3. **§2b** — client-side `If-None-Match` cache. Lands AFTER §2 (needs
   server ETags on every target endpoint). Low-risk ~80 LoC wrapper
   around `apiFetch`; adopt one endpoint at a time starting with
   `/api/photos/resources` and `/api/folders/{id}/resources`. The
   browser's HTTP cache already covers the common case, so §2b is
   earned by (a) stale-while-revalidate UX on tab switch and
   (b) resilience against mobile browser cache evictions.
4. **§6** — `?kind=photo|video` filter. Cheap; one `WHERE` branch
   and a cursor field. Lands anytime after §1.
5. **§6b** — `?drive_id=<uuid>` filter. Reuses the existing
   `idx_files_media_timeline_by_drive` as-is; one `WHERE`
   predicate on the LATERAL probe plus a visibility check
   against the `accessible` CTE for anti-enum. Lands anytime
   after §1; composes cleanly with §6.
6. **§3** — sort axis. User-visible feature; land once a stakeholder
   actually asks for the "file arrival time" view.
7. **§7 (phase 1: `?person_id=`)** — consolidate the existing
   `person_photos` endpoint under this one. Mechanical; adds one
   filter arg to the cursor serializer. Later phases (keyword,
   location) land when their respective pipelines exist.
8. **§8 — DEFERRED.** Not scheduled. Captured for shape only; a
   future "watching listings go stale" demand across more than
   one `/resources` endpoint earns the message-bus axis.
9. **§9 — DONE** (2026-10-08). Within-drive dedup via
   `DISTINCT ON (fi.blob_hash)` on the lateral probe, cursor + outer
   LIMIT applied after the DISTINCT so a page boundary never
   promotes an older sibling of a surviving blob. Cross-drive dedup
   stays off — each drive's scope is isolated by the per-drive
   `CROSS JOIN LATERAL`. Regression test lives at
   `photos_resources.hurl` step 8d.
10. **§9b — DONE** (2026-10-09). Delete-UX follow-up to §9, shipped
    as the three-layer stack the design converged on:
    - Layer 0 (`3d5a2b1a`): split `GET /api/dedup/check/{hash}`
      into user-scoped (drops `ref_count`, 404 anti-enum,
      anonymous-caller guard) and admin-scoped
      (`GET /api/admin/dedup/check/{hash}`, always carries
      `ref_count`). Hurl +  webdav bash tests migrated;
      `trash.hurl` + `trash_resources.hurl` gained a
      `DELETE /api/trash/empty` preflight so prior-session
      residue can't flunk their `count==0` assertion.
    - Layer 1 (`c949215d`): `has_blob_siblings: bool` on
      `PhotoResourceItemDto`, computed by a per-row EXISTS probe
      that rides `idx_files_media_dedup_by_drive_blob`. The flag
      scope matches the gallery's own filter (`NOT is_trashed AND
      image/video`) — "siblings a user would actually SEE in the
      gallery", not "every ref anywhere". A trashed-only fan-out
      reads `false`; the FE must still call Layer 2 to be fully
      honest about bytes if that matters to the specific flow.
    - Layer 2 (`e9902b17`): the user-scoped check endpoint
      returns the caller's visible sibling file-rows —
      `{ file_id, name, drive_id, folder_id, is_trashed,
      can_delete, can_update, can_share }` per row, plus `count`
      + `truncated` (cap `MAX_SIBLINGS = 500`). Trashed rows are
      included: they still hold a blob ref until the trash entry
      is purged, so leaving them out would hide bytes the user
      needs to see. Authz filter runs through
      `check_files_read_batch` so the response never discloses
      files behind drive grants the caller doesn't hold — same
      anti-enum guarantee the handler leads with for its 404
      path. The modal renders a per-row checkbox list and
      batches ticked ids into the existing
      `POST /api/batch/trash`. No new bulk-trash endpoint — the
      caller explicitly names what to trash, so zero
      blast-radius risk.
