# Nextcloud desktop client — sync findings

**Status: open for discussion (2026-10-22).** Found by the
`tests/nextcloudcmd/` suite, which runs the real Nextcloud sync engine
(`nextcloudcmd` 3.16.7) against OxiCloud while files change on both sides.
F2 and F3 are fixed with that suite; the rest are recorded here so they can
be decided deliberately instead of patched in passing. F1 (metadata
operations rewrote a file's mtime and ETag) and F6 (creates ignored
`X-OC-Mtime`) were fixed on main by `docs/plan/normalize-file-dates.md`
§1/§9 and are not repeated here.

| # | Finding | Status |
|---|---|---|
| F2 | Chunked-upload assembly answered without `OC-FileId` | fixed — `tests/api/nc_write_response_headers.hurl` |
| F3 | Writes never answered `X-OC-MTime: accepted` | fixed — same pin |
| F4 | Folder ETag bumps trail the change by up to one flush interval | by design; noted |
| F5 | Capabilities do not advertise `files.chunked_upload.max_size` | fixed — `tests/api/nc_status_capabilities.hurl` |

## F2 / F3 — fixed

The chunked-upload assembly MOVE returned 201 with `ETag`/`OC-ETag` and no
`OC-FileId`; the client reports "Missing File ID from server" and marks
**every chunked upload failed** even though the server committed it. No
write path answered `X-OC-MTime: accepted` either, so the client logged
"Server does not support X-OC-MTime" although the mtime was applied.

Fix: `webdav_handler::with_nc_write_headers`, shared by PUT, PATCH and
`uploads_handler::handle_assemble`. Pin:
`tests/api/nc_write_response_headers.hurl`. `accepted` is sent only when
the stored mtime equals the requested one.

## F4 — the folder-ETag window

Folder ETags are bumped asynchronously by `TreeEtagFlushService`
(`OXICLOUD_TREE_ETAG_FLUSH_MS`, default 500 ms) so user-facing writes take
no folder-row locks. A sync whose discovery lands inside that window sees
the new listing under the old ETag and can skip the subtree until its next
poll. Observed: a server-side folder move reached the client as "source
gone" without "destination present", so the client deleted its copy and
re-downloaded the folder one sync later. No data is lost and it converges
on the next poll (clients poll every ~30 s), but a large folder moved at
the wrong moment costs a full re-download. The suite waits out two flush
intervals before every sync, so it tests convergence rather than the
width of the window.

## F5 — chunk size not advertised

NC 28+ servers advertise `files.chunked_upload.max_size`, which the client
reads to size its chunks. OxiCloud does not. With the default
`OXICLOUD_DIRECT_PUT_MAX_BYTES` (1 GiB) this does not matter. An operator
who lowers it — or `OXICLOUD_CHUNK_MAX_BYTES` — below the client's default
chunk size makes desktop uploads fail with 413, because the client has no
way to learn the limit. (The test environment sets both to 4 MiB; the suite
forces 2 MB client chunks via `OWNCLOUD_CHUNK_SIZE` for that reason.)

**Fixed:** capabilities now advertise
`files.chunked_upload.max_size = min(OXICLOUD_CHUNK_MAX_BYTES,
OXICLOUD_DIRECT_PUT_MAX_BYTES)` — the smaller cap, because the client
PUTs a file whole when it fits in one chunk. Pinned in
`tests/api/nc_status_capabilities.hurl` (A3/A4) and the inline tests in
`src/interfaces/nextcloud/ocs_handler.rs`.
