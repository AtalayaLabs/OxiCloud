# =============================================================
# OxiCloud — NC multi-drive MOVE destination-prefix regressions
# =============================================================
# Regression coverage for two sibling bugs discovered 2026-07-12
# when the multi-drive `admin~{drive-uuid}` credential shape was
# rolled through the NC `/remote.php/dav/*` surface but two MOVE
# handlers were missed:
#
#   uploads_handler::handle_assemble (chunked-upload MOVE)
#   trashbin_handler   (restore MOVE with a Destination header)
#
# Both handlers were stripping the destination-URL prefix with
# `&user.username` (bare `admin`) instead of
# `&session.raw_username` (composite `admin~{uuid}`). NC clients
# on a non-home drive send:
#     Destination: /remote.php/dav/files/admin~{uuid}/<path>
# The bare-username strip left `~{uuid}/<path>` glued to the
# leading path segment; downstream lookups then targeted a
# fabricated `<drive-root>/~{uuid}/…` path and 500'd (assemble
# path) or silently missed collisions (trash path).
#
# webdav_handler::handle_move (the standard `/dav/files/…` MOVE)
# was ALREADY correct — it uses `url_user = &session.raw_username`.
# The uploads + trashbin siblings were coverage gaps: no Hurl
# tests hit them with a composite credential.
#
# Hurl gotcha: the `[BasicAuth]` block parses the username as a
# single token terminated by `:`. A raw composite like
# `{{nc_username}}~{{drive_id}}` fails to parse because Hurl
# sees the `~` between two templates and expects a line
# terminator. Workaround: alias the composite into
# `nc_basic_user` via `[Options] variable:` on a bootstrap
# request, then use `{{nc_basic_user}}` in every subsequent
# BasicAuth block.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup 1 — JWT login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt:           jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Setup 2 — Fetch admin's home drive root folder id.
#
# The composite `{user}~{marker}` shape sends the marker
# through `basic_auth_middleware.rs`, which resolves it as a
# **folder id** (not a drive id) via
# `folder_service.get_folder_with_perms(folder_id, user_id)` —
# the auth boundary refuses if the caller lacks Read on that
# folder.
#
# For a regression test we don't need a SECONDARY drive —
# we need any folder id the caller has Read on so the composite
# credential authenticates cleanly. Admin's own home folder is
# the trivially-authorized choice; the tilde-parsing bug in
# `handle_assemble` / trashbin restore fires the same way
# regardless of which folder id the marker points at.
#
# For the real multi-drive scenario Ed hit in production, the
# marker after `~` was the folder id of a shared drive's root
# where admin had explicit Read via role_grants. That code path
# is identical to the one exercised here — the bug is in the
# destination-URL parsing, not in what the folder id points to.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Setup 3 — Mint an app password. `username` in the response is
# just `admin`; we splice the folder id onto it in Setup 4
# below to get the composite `admin~{uuid}` shape.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_multidrive_move_regression hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Setup 4 — Bootstrap the composite BasicAuth username.
#
# `[Options] variable:` sets a variable whose VALUE is a
# template expanded against the current bindings, then the
# result is available to all subsequent requests. `nc_username`
# and `home_folder_id` are already captured; concatenating them
# here hides the `~` from the strict `[BasicAuth]` parser
# (which would otherwise reject `{{nc_username}}~{{home_folder_id}}`
# mid-username).
#
# `/ready` is a cheap unauthenticated 200 that gives us a
# request to hang the option on. No side effects.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ready
[Options]
variable: nc_basic_user={{nc_username}}~{{home_folder_id}}

HTTP 200


# =============================================================
# A. Chunked-upload MOVE assemble regression
# =============================================================
# `handle_assemble` in `uploads_handler.rs` was calling
# `extract_files_subpath(&destination, &user.username)`. With a
# composite Destination it treated `~{drive_uuid}/<path>` as the
# target subpath, then tried `nc_to_internal_path(chroot, …)`
# → `<drive-root>/~{drive_uuid}/<path>`. Downstream parent-folder
# lookup → 500.
#
# Fixed by binding on `&session.raw_username`. Test shape:
#   A1 — MKCOL: create the chunked-upload session directory.
#   A2 — MOVE `.file` (empty session → zero chunks → assemble
#        writes an empty file at Destination). Pre-fix: 500 with
#        "Failed to get folder at path: /<drive-root>/~<uuid>".
#        Post-fix: 201 + file exists at the real Destination.
#   A3 — PROPFIND on the destination path to confirm the file
#        landed under the drive's root (NOT under `~<uuid>/`).
# =============================================================

# A1 — MKCOL upload session.
MKCOL {{base_url}}/remote.php/dav/uploads/{{nc_basic_user}}/regression-upload-session
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 201


# A2 — MOVE `.file` with a composite Destination header. Empty
# session, so the assemble step writes a zero-byte file at the
# destination path — that's fine, we're pinning the destination-
# parsing behaviour, not the byte-copying.
#
# Hurl gotcha: headers MUST come before section blocks like
# `[BasicAuth]`. `Destination:` after `[BasicAuth]` gets parsed
# as a new request's method line.
MOVE {{base_url}}/remote.php/dav/uploads/{{nc_basic_user}}/regression-upload-session/.file
Destination: {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-assembled.txt
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

# The regression: pre-fix this returned 500 with a
# "~<drive-uuid>" fragment in the error message; post-fix it
# writes the empty file successfully. Any 2xx status proves the
# destination-parsing path is intact.
HTTP 201


# A3 — Confirm the file exists at the real path inside the
# chroot. HTTP 207 alone is the load-bearing assertion: pre-fix,
# MOVE would have 500'd (so we'd never reach here); and even if
# it had somehow written, the file would have landed at the
# fabricated `<chroot>/~<folder_id>/…` path rather than
# `<chroot>/regression-assembled.txt` — this PROPFIND would
# then 404 rather than 207.
#
# `body not contains "~{folder_id}/..."` would be redundant AND
# wrong here: NC's PROPFIND echoes the client's request URL in
# `<d:href>`, so the composite `admin~<folder_id>` legitimately
# appears in the returned href — that's the URL prefix, not a
# leak. The empty-file-size assertion below is the concrete
# positive check: MOVE with zero chunks assembles a 0-byte
# file, so we pin that shape.
PROPFIND {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-assembled.txt
Depth: 0
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getcontentlength'])" == "0"


# Cleanup — remove the assembled file so a re-run starts clean.
DELETE {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-assembled.txt
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 204


# =============================================================
# B. Trashbin restore MOVE — sibling handler with the same bug
# =============================================================
# `trashbin_handler.rs` line 143 has the same shape:
#     extract_nc_subpath_from_dest(&dest_header, &user.username)
# The trash MOVE uses the Destination header for a collision
# pre-check, not for relocation (restore always lands at the
# original path). A buggy prefix strip therefore doesn't 500 —
# it silently miscomputes the collision path (`<drive>/~<uuid>/…`
# instead of `<drive>/<real-path>`), letting a real collision
# slip past. The response is 2xx either way.
#
# So a "5xx vs 201" assertion won't catch it. What DOES catch it:
# stage a genuine collision, restore with a Destination that
# points at it. Pre-fix: no 412 (bug misses the collision).
# Post-fix: 412 Precondition Failed.
#
# Sequence:
#   B1 — Upload `regression-collision.txt` to the drive.
#   B2 — DELETE it (soft-trash).
#   B3 — Re-upload `regression-collision.txt` (new file at the
#        same path) to stage the collision.
#   B4 — Enumerate the trashbin to find the trashed item's id.
#   B5 — MOVE the trash item back with Destination pointing at
#        the re-created file. Pre-fix: 201/204 (collision missed).
#        Post-fix: 412 Precondition Failed.
# =============================================================

# B1 — Stage the file the client will trash.
PUT {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-collision.txt
Content-Type: text/plain
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}
```
first version
```

HTTP 201


# B2 — Soft-trash it.
DELETE {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-collision.txt
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 204


# B3 — Re-upload at the same path to stage the collision.
PUT {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-collision.txt
Content-Type: text/plain
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}
```
second version
```

HTTP 201


# B4 — Enumerate trashbin to find the trashed item's numeric id
# (NC identifies trash items with `oc:trashbin-filename` etc.).
# Using PROPFIND at Depth 1 on the trashbin root.
PROPFIND {{base_url}}/remote.php/dav/trashbin/{{nc_basic_user}}/trash
Depth: 1
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 207
[Captures]
# Grab the href of the first trashed child. Fragile against
# multi-item trash but this test creates exactly one before
# reading — safe here. Local-name xpath so we don't have to
# thread the DAV namespace prefix.
trash_item_href: xpath "string((//*[local-name()='response']/*[local-name()='href'])[2])"


# B5 — MOVE the trash item back with a composite Destination.
# Pre-fix: collision check runs against a fake `<drive>/~<uuid>/…`
# path, misses the real collision, restore succeeds (201/204).
# Post-fix: collision check hits the real path, request refused
# with 412.
MOVE {{base_url}}{{trash_item_href}}
Destination: {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-collision.txt
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

# Post-fix expectation: 412 (collision detected). If a future
# change makes trashbin restore honour Destination for
# relocation, this assertion changes — but the collision-check
# semantics should stay collision-refusing.
HTTP 412


# Cleanup — permanently delete the trashed item so a re-run
# starts clean, and drop the live file.
DELETE {{base_url}}{{trash_item_href}}
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/remote.php/dav/files/{{nc_basic_user}}/regression-collision.txt
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 204


# =============================================================
# C. Chunked-upload PROPFIND href regression
# =============================================================
# `handle_propfind_session` in `uploads_handler.rs` was emitting
# `<d:href>` values with `user.username` (bare `admin`) instead of
# `session.raw_username` (composite `admin~<uuid>`). Same shape
# as the trashbin PROPFIND bug: NC clients doing chunked-upload
# resume PROPFIND the session, then MOVE/DELETE against the
# returned hrefs. With the bare form, every follow-up 403s at
# the `NcSession` extractor (URL `{user}` segment mismatches
# `raw_username`).
#
# Positive test: after PROPFIND-ing an upload session with a
# composite credential, the emitted hrefs MUST contain `~<uuid>`.
# Pre-fix: `<d:href>/remote.php/dav/uploads/admin/…</d:href>`.
# Post-fix: `<d:href>/remote.php/dav/uploads/admin~<uuid>/…</d:href>`.
# =============================================================

# C1 — MKCOL a fresh session.
MKCOL {{base_url}}/remote.php/dav/uploads/{{nc_basic_user}}/regression-propfind-session
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 201


# C2 — PUT one chunk so PROPFIND has something to enumerate
# alongside the session collection itself.
PUT {{base_url}}/remote.php/dav/uploads/{{nc_basic_user}}/regression-propfind-session/00000001
Content-Type: application/octet-stream
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}
```
chunk-body
```

HTTP 201


# C3 — PROPFIND the session with the composite credential and
# assert every emitted href carries the composite user segment.
# `contains "~{{home_folder_id}}/"` is the exact byte marker
# introduced by the fix — the bug would produce
# `/dav/uploads/admin/…` with no `~` between the surface and the
# session id.
PROPFIND {{base_url}}/remote.php/dav/uploads/{{nc_basic_user}}/regression-propfind-session
Depth: 1
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 207
[Asserts]
# Every href for this upload surface must echo the composite user.
# Two responses expected: session collection + one chunk. Both
# hrefs share the same `/remote.php/dav/uploads/<user>/<session>/…`
# prefix, so one substring check on the body body is sufficient
# and immune to XML formatting drift.
body contains "/remote.php/dav/uploads/{{nc_basic_user}}/regression-propfind-session/"
# Belt-and-suspenders: assert the bare form is absent. The
# composite basic-user is `admin~<uuid>`; the bare form would
# render as `/dav/uploads/admin/regression-…` (no `~`).
# `not contains` here would still permit that byte sequence to
# appear inside the composite, so we anchor on the trailing `/`
# after the user segment to disambiguate: `/admin/regression-…`
# is the bug shape; the fix never produces `/admin/regression-…`
# because the composite always separates admin from the session
# with `~<uuid>`.
body not contains "/remote.php/dav/uploads/{{nc_username}}/regression-propfind-session"


# C4 — DELETE the session with a composite href. Pre-fix (bare
# href returned by C3 that the client would have followed) this
# would have been a wire-level 403 at the extractor; post-fix
# the composite href works end-to-end.
DELETE {{base_url}}/remote.php/dav/uploads/{{nc_basic_user}}/regression-propfind-session
[BasicAuth]
{{nc_basic_user}}: {{nc_password}}

HTTP 204


# =============================================================
# Teardown — revoke the app password.
# =============================================================
DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
