# Search

OxiCloud provides authenticated file and folder search with a
cursor-paginated response, filter/sort query parameters, recursive
traversal, and in-memory result caching.

## Endpoints

| Method | Endpoint | Description |
| --- | --- | --- |
| `GET` | `/api/search` | Cursor-paginated search |
| `GET` | `/api/search/suggest` | Lightweight autocomplete suggestions |
| `DELETE` | `/api/admin/search/cache` | Flush the shared search results cache (admin only) |

All search endpoints require authentication. The cache flush is
additionally restricted to administrators — see [Result Caching](#result-caching).

## Query Parameters

| Parameter | Description |
| --- | --- |
| `query` | Text to search in file/folder names (and file content when the Tantivy index is enabled) |
| `type` | Comma-separated file extensions (files only) |
| `folder_id` | Restrict search scope to one folder |
| `recursive` | Search subfolders, defaults to `true` |
| `created_after` / `created_before` | Filter by creation time (unix seconds) |
| `modified_after` / `modified_before` | Filter by modification time |
| `min_size` / `max_size` | Filter by file size in bytes |
| `resource_types` | Comma-separated: `file`, `folder` (both by default) |
| `order_by` | Sort dimension: `relevance` (default), `name`, `size`, `updated_at`, `created_at` |
| `reverse` | Reverse sort direction (no-op for `relevance`) |
| `limit` | Page size (1–200, default 50) |
| `cursor` | Opaque cursor returned by the previous page |

## Response Shape

`/api/search` returns the same envelope as every other `/*/resources`
listing (folders, favorites, recent, trash, shared) so a single
client component can render all of them:

```json
{
  "items": [
    {
      "resource_type": "file",
      "resource": { "id": "…", "name": "report.pdf", "size": 12345, "…": "…" },
      "meta": {
        "score": 0.82,
        "snippet": "…quarterly <mark>report</mark>…",
        "via": "content"
      }
    },
    {
      "resource_type": "folder",
      "resource": { "id": "…", "name": "Reports", "…": "…" },
      "meta": { "score": 0.31, "via": "name" }
    }
  ],
  "next_cursor": "eyJvZmZzZXQiOjUwLCJvcmRlcl9ieSI6InJlbGV2YW5jZSJ9",
  "query_time_ms": 12,
  "total": 137
}
```

- `resource_type`: `"file"` or `"folder"` — tells the client which
  variant of `resource` to render.
- `resource`: the same `FileDto` / `FolderDto` shape any other
  endpoint would emit.
- `meta.score`: relevance in `[0, 1]`.
- `meta.snippet`: optional HTML-safe excerpt from the content index
  (present only when the match came from file content).
- `meta.via`: `"name"`, `"content"`, or `"path"` — where the match
  fired.
- `next_cursor`: opaque; present iff another page exists. Pass it
  verbatim as `?cursor=…` for the next call.
- `total`: integer, approximate — reflects the caller-visible match
  count (permission-filtered). Omitted when unknown; never leaks a
  count for rows the caller cannot see.

### Example

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://oxicloud.example.com/api/search?query=report&type=pdf,docx&limit=20"
```

## Suggestions

Use `/api/search/suggest?query=rep&limit=10` for quick
autocomplete-style results. Suggestions can also be scoped to a
folder with `folder_id`.

## Result Caching

Search results are cached in memory using the search criteria and
user ID as the cache key.

- Cache TTL: 5 minutes
- Max entries: 1000
- Manual invalidation: `DELETE /api/admin/search/cache` — admin-only.
  The endpoint calls `invalidate_all()` on the shared moka cache, so
  one call cold-starts every subsequent search for every tenant;
  it's an operator debug lever, not a per-user affordance.

## Feature Flag

Search can be disabled with `OXICLOUD_ENABLE_SEARCH=false`.
