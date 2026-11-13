# =============================================================
# OxiCloud — GET /api/files/recent-changes
# =============================================================
# "What changed lately, and who changed it"
# (`docs/plan/recently-modified.md`). Wire-level pins for the parts a
# Rust test cannot see: the status codes, the envelope shape, and above
# all the ACCESS FILTER, which is three separate SQL branches and the
# one place a wrong simplification leaks another user's files.
#
# Self-contained: creates its own fixture users and folder, and deletes
# them at the end. It must be, because the API suite shares one database
# with every other file — no assertion here counts rows globally, each
# one names the fixture it owns.
#
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/recent_changes.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# 0 — Admin logs in, and creates two fixture users.
#
#     Two is the minimum that makes the access pins mean anything:
#     with one user every row is trivially visible and the filter
#     could be `WHERE true` and still pass.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "rc-ann",
  "password": "RcAnnPassword1!",
  "email":    "rc-ann@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
ann_id: jsonpath "$.user.id"


POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "rc-bo",
  "password": "RcBoPassword1!",
  "email":    "rc-bo@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
bo_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "rc-ann", "password": "RcAnnPassword1!" }

HTTP 200
[Captures]
ann_token: jsonpath "$.access_token"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "rc-bo", "password": "RcBoPassword1!" }

HTTP 200
[Captures]
bo_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Ann uploads a file into her own home folder.
#
#     The upload is what writes the content touch; nothing in this
#     file inserts one directly, so a write path that stopped
#     recording would fail here rather than in a unit test.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{ann_token}}

HTTP 200
[Captures]
ann_home: jsonpath "$[0].id"


POST {{base_url}}/api/folders
Authorization: Bearer {{ann_token}}
Content-Type: application/json
{ "name": "rc-ann-folder", "parent_id": "{{ann_home}}" }

HTTP 201
[Captures]
ann_folder: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{ann_token}}
[MultipartFormData]
folder_id: {{ann_folder}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
ann_file: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# 2 — Ann sees her own change, in both axes.
#
#     `modifier_id` is the uploader and `changed_at` is this row's
#     touch — not the file's latest change by anyone. The item also
#     carries the file under `resource`, shape-identical to every
#     other listing.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
# Hurl 8.x UNWRAPS a JSONPath filter that matches exactly once: the result
# is the scalar, not a one-element list. So `== "value"` is the assert, and
# `includes` / `count` / `nth 0` on a filter result are all wrong here —
# they fail with "expected list, got string" on the single-match case this
# fixture is built to produce.
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" == "{{ann_id}}"
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].changed_at" isString


GET {{base_url}}/api/files/recent-changes?order_by=modifier
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" == "{{ann_id}}"
# `lane_total` is the number of FILES this person changed in the window,
# counted inside the access filter. Present on the people axis only.
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].lane_total" isInteger


# ─────────────────────────────────────────────────────────────
# 3 — ACCESS: Bo cannot see Ann's file.
#
#     Bo has no grant of any kind and no access to Ann's drive. If
#     this ever returns the row, the access filter has collapsed —
#     which is exactly what an `A OR B OR C` predicate does when the
#     planner gives up and scans.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
# `not exists`, not `isEmpty`: a filter matching NOTHING yields null in
# Hurl 8.x rather than an empty list, and `isEmpty` on null is not the
# assert it looks like.
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" not exists


# ─────────────────────────────────────────────────────────────
# 4 — ACCESS branch 2: a grant on the FILE admits it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{ann_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bo_id}}" },
  "resource": { "type": "file", "id": "{{ann_file}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
file_grant_id: jsonpath "$.grants[0].id"


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" == "{{ann_id}}"


# Revoked again, so branch 3 below is tested on its own merits rather
# than passing because branch 2 is still open.
DELETE {{base_url}}/api/grants/{{file_grant_id}}
Authorization: Bearer {{ann_token}}

HTTP 204


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" not exists


# ─────────────────────────────────────────────────────────────
# 4b — An EXPIRED grant admits nothing.
#
#      The janitor sweeps expired grants on a cadence, not
#      synchronously, so between `expires_at` passing and the sweep
#      the row is STILL in `role_grants`. A visibility query without
#      the read-time `expires_at` filter therefore keeps serving a
#      grant that has lapsed — for however long the cadence is. Both
#      grant arms of the access filter carry the filter today; this
#      pin is what notices if a "cleanup" refactor drops one.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{ann_token}}
Content-Type: application/json
{
  "subject":    { "type": "user", "id": "{{bo_id}}" },
  "resource":   { "type": "file", "id": "{{ann_file}}" },
  "role":       "viewer",
  "expires_at": "2020-01-01T00:00:00Z"
}

HTTP 201
[Captures]
expired_grant_id: jsonpath "$.grants[0].id"


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" not exists


# Revoked, so branch 3 below is again tested on its own merits.
DELETE {{base_url}}/api/grants/{{expired_grant_id}}
Authorization: Bearer {{ann_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# 5 — ACCESS branch 3: a cascading grant on the FOLDER admits the
#     file inside it.
#
#     This is the branch most likely to be dropped as "an
#     optimisation" — it is the one that needs an ltree ancestor test
#     rather than an equality — and dropping it silently hides every
#     file anyone shared with you by folder.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{ann_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bo_id}}" },
  "resource": { "type": "folder", "id": "{{ann_folder}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
folder_grant_id: jsonpath "$.grants[0].id"


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file}}')].modifier_id" == "{{ann_id}}"


# `lane_total` must be counted AFTER the access filter. Bo sees one of
# Ann's files, so Ann's lane reads 1 for him — even if Ann has others he
# cannot see. A count taken before filtering would leak their existence.
GET {{base_url}}/api/files/recent-changes?order_by=modifier&person={{ann_id}}
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[0].modifier_id" == "{{ann_id}}"
jsonpath "$.items[0].lane_total" == 1


# ─────────────────────────────────────────────────────────────
# 5b — A PUBLIC LINK is not a person, and admits nobody.
#
#      `POST /api/shares` mints a `role_grants` row whose
#      `subject_type` is `token` (share_service.rs — the grant the
#      `trg_cleanup_grants_token` trigger later reaps). Those rows sit
#      in the same table the access filter joins, so a filter that
#      forgot `subject_type IN ('user','group')` would put EVERY
#      publicly-linked file in EVERY user's feed. Nothing else in this
#      suite would notice: the share itself keeps working.
#
#      The fixture lives in Ann's home root, NOT in `rc-ann-folder`,
#      so the still-open folder grant from step 5 cannot be what
#      admits or hides it — and Bo's visible-file count stays at the
#      two rows step 10 pages over.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{ann_token}}
[MultipartFormData]
folder_id: {{ann_home}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
ann_file_pub: jsonpath "$.id"


POST {{base_url}}/api/shares
Authorization: Bearer {{ann_token}}
Content-Type: application/json
{
  "item_id":   "{{ann_file_pub}}",
  "item_type": "file"
}

HTTP 201
[Captures]
pub_share_id: jsonpath "$.id"


# Ann sees her own file: proves the row EXISTS, so Bo's miss below is
# the access filter refusing it rather than the feed being empty.
GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file_pub}}')].modifier_id" == "{{ann_id}}"


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file_pub}}')].modifier_id" not exists


# ─────────────────────────────────────────────────────────────
# 5c — Trash hides a row; restore brings it back UNCHANGED.
#
#      Two invariants in one walk. The feed must not list a trashed
#      file (all three access branches carry `NOT f.is_trashed`, and
#      a feed is the last place you want a deleted file resurfacing).
#      And a restore must not look like a content change: it writes no
#      touch, so the row returns to the lane it was already in,
#      carrying its ORIGINAL timestamp and its ORIGINAL person. An
#      implementation that re-touched on restore would credit whoever
#      emptied the trash and re-date the change — plausible-looking,
#      and wrong.
#
#      Also in Ann's home root, for the step-10 reason above.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{ann_token}}
[MultipartFormData]
folder_id: {{ann_home}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 201
[Captures]
ann_file3: jsonpath "$.id"


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{ann_token}}

HTTP 200
[Captures]
file3_changed_at: jsonpath "$.items[?(@.resource.id == '{{ann_file3}}')].changed_at"
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file3}}')].modifier_id" == "{{ann_id}}"


DELETE {{base_url}}/api/files/{{ann_file3}}
Authorization: Bearer {{ann_token}}

HTTP 204


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file3}}')].modifier_id" not exists


# The trash entry id IS the resource id in the soft-delete model
# (`storage.trash_items` view; trash.hurl step 7 documents it), so the
# file id is what restore takes — no trash listing needed.
POST {{base_url}}/api/trash/{{ann_file3}}/restore
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.success" == true


GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file3}}')].modifier_id" == "{{ann_id}}"
jsonpath "$.items[?(@.resource.id == '{{ann_file3}}')].changed_at" == "{{file3_changed_at}}"


# ─────────────────────────────────────────────────────────────
# 5d — A CLIENT-ASSERTED MTIME DOES NOT HIDE THE UPLOAD.
#
#      Found on a sandbox, not by this suite: an uploaded photo never
#      appeared in the feed. `source_modified_at` (and `X-OC-Mtime`,
#      and the chunked and WebDAV equivalents) sets the file's content
#      clock, which is correct POSIX behaviour — and the touch used to
#      take that same value. So a holiday photo carrying a 2019 camera
#      date wrote a 2019 touch and fell outside the window the instant
#      it was uploaded. Every sync client asserts an mtime, so this was
#      the common path, not an edge case.
#
#      The two clocks must now DISAGREE, and that is the assertion:
#      `resource.modified_at` is the client's 2019, while `changed_at`
#      is when the server saw the upload. `days=7` is what makes it a
#      real pin — a 2019 touch cannot be inside a 7-day window, so the
#      row would simply be missing.
#
#      In Ann's home root, not `rc-ann-folder`, so Bo's visible-file
#      count stays at the two rows step 10 pages over.
# ─────────────────────────────────────────────────────────────
#      A text fixture with content of its OWN, deliberately. The bug
#      was found with a photo, but a JPEG would also kick off EXIF
#      extraction, which has its own opinion about dates
#      (`photos_sort_date_content_clock.hurl`) — and `hello-copy.txt`
#      is byte-identical to the `hello.txt` 5b already uploaded, so it
#      would ride the dedup path. Either way a regression elsewhere
#      would surface here looking like an mtime bug. This step tests
#      the mtime alone.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{ann_token}}
[MultipartFormData]
folder_id: {{ann_home}}
source_modified_at: 1546300800000
file: file,fixtures/cache_header_unique.txt; text/plain

HTTP 201
[Captures]
ann_file_old: jsonpath "$.id"
[Asserts]
# 1546300800000 ms = 2019-01-01T00:00:00Z. The content clock still
# obeys the client; `rest_upload_source_mtime.hurl` pins that contract
# in its own right, and this half is here so a regression that simply
# stopped honouring the mtime cannot make the rest of this step pass.
jsonpath "$.modified_at" == "2019-01-01T00:00:00Z"


GET {{base_url}}/api/files/recent-changes?days=7
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{ann_file_old}}')].modifier_id" == "{{ann_id}}"
jsonpath "$.items[?(@.resource.id == '{{ann_file_old}}')].resource.modified_at" == "2019-01-01T00:00:00Z"
# The touch is the server's clock, so it cannot be the camera's date.
jsonpath "$.items[?(@.resource.id == '{{ann_file_old}}')].changed_at" != "2019-01-01T00:00:00Z"


# ─────────────────────────────────────────────────────────────
# 6 — The `person` filter is a view, not a permission.
#
#     Asking about a person you share nothing with returns an empty
#     page, not a 403: the same anti-enumeration shape the photos
#     `drive_id` filter uses, so a caller cannot probe who exists.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/recent-changes?person={{bo_id}}
Authorization: Bearer {{ann_token}}

HTTP 200
[Asserts]
jsonpath "$.items" isEmpty


# ─────────────────────────────────────────────────────────────
# 7 — `days` is validated, never clamped.
#
#     A client asking for a year is told so. Clamping would quietly
#     serve 90 days and look correct — and the ceiling exists because
#     the lane aggregate's cost is proportional to the window.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/recent-changes?days=91
Authorization: Bearer {{ann_token}}

HTTP 400
[Asserts]
jsonpath "$.message" contains "90"


GET {{base_url}}/api/files/recent-changes?days=0
Authorization: Bearer {{ann_token}}

HTTP 400


GET {{base_url}}/api/files/recent-changes?days=90
Authorization: Bearer {{ann_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — A bad axis is refused at decode time.
#
#     The enum is typed precisely so an unknown value cannot fall
#     back to the default and silently reorder someone's page.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/recent-changes?order_by=whenever
Authorization: Bearer {{ann_token}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# 9 — A malformed cursor is a 400, not a reset to page one.
#
#     "Start from the top" is the dangerous answer: pagination would
#     drift silently and a client would re-show rows it had passed.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/recent-changes?cursor=not-a-cursor
Authorization: Bearer {{ann_token}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# 10 — Paging.
#
#      A second file in the granted folder, so Bo can see TWO rows.
#      This is load-bearing: a cursor is minted only when there are
#      MORE rows than the limit, so with one visible row `next_cursor`
#      is absent and there is nothing to page with — which is exactly
#      how the first cut of this test failed.
#
#      It also makes `lane_total` worth asserting twice: it read 1
#      above and must read 2 now, which pins that the count comes from
#      the live filtered set rather than from anything cached.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{ann_token}}
[MultipartFormData]
folder_id: {{ann_folder}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 201
[Captures]
ann_file2: jsonpath "$.id"


GET {{base_url}}/api/files/recent-changes?order_by=modifier&person={{ann_id}}
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[0].lane_total" == 2


# Page 1 of 1-per-page: a cursor now exists.
GET {{base_url}}/api/files/recent-changes?limit=1&days=30
Authorization: Bearer {{bo_token}}

HTTP 200
[Captures]
page1_cursor: jsonpath "$.next_cursor"
page1_id: jsonpath "$.items[0].resource.id"
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.next_cursor" isString


# Page 2 must be the OTHER row. This is the keyset doing its job: a
# predicate that compared the wrong tuple would re-serve page 1 (the
# classic off-by-one that looks like success until you read the ids) or
# skip straight past it.
GET {{base_url}}/api/files/recent-changes?limit=1&days=30&cursor={{page1_cursor}}
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" != "{{page1_id}}"


# A cursor cannot be carried across a changed window: `days` changes
# which rows exist, so continuing would skip or repeat silently.
GET {{base_url}}/api/files/recent-changes?limit=1&days=7&cursor={{page1_cursor}}
Authorization: Bearer {{bo_token}}

HTTP 400


# The people axis pages too, and its cursor carries the lane as well as
# the row — four keys deep, which is where an axis-blind predicate
# breaks.
GET {{base_url}}/api/files/recent-changes?order_by=modifier&limit=1
Authorization: Bearer {{bo_token}}

HTTP 200
[Captures]
lane_cursor: jsonpath "$.next_cursor"
lane_page1_id: jsonpath "$.items[0].resource.id"


GET {{base_url}}/api/files/recent-changes?order_by=modifier&limit=1&cursor={{lane_cursor}}
Authorization: Bearer {{bo_token}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.id" != "{{lane_page1_id}}"
jsonpath "$.items[0].modifier_id" == "{{ann_id}}"


# A cursor from one axis is refused on the other — the axis travels in
# the cursor precisely so a mid-scroll flip cannot reshuffle a page.
GET {{base_url}}/api/files/recent-changes?limit=1&cursor={{lane_cursor}}
Authorization: Bearer {{bo_token}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# 11 — Teardown. The API suite shares one database, so a fixture left
#      behind is a failure in somebody else's file later.
#
#      Deleting the users cascades their touches; the file and folder
#      go explicitly because `storage.files.created_by` is
#      `ON DELETE SET NULL`, not CASCADE, so those rows outlive their
#      owner (the exact leak that broke this feature's Rust pins on
#      their second run).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{folder_grant_id}}
Authorization: Bearer {{ann_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{ann_file}}
Authorization: Bearer {{ann_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{ann_file2}}
Authorization: Bearer {{ann_token}}

HTTP 204


# The share goes before its file: revoking it drops the `token` grant
# (trigger on `storage.shares`), which is the row step 5b is about.
DELETE {{base_url}}/api/shares/{{pub_share_id}}
Authorization: Bearer {{ann_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{ann_file_pub}}
Authorization: Bearer {{ann_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{ann_file3}}
Authorization: Bearer {{ann_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{ann_file_old}}
Authorization: Bearer {{ann_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{ann_folder}}
Authorization: Bearer {{ann_token}}

HTTP 204


# 200, not 204 — `DELETE /api/admin/users/{id}` returns a body
# (see `admin_user_ops.hurl` step 8). The asymmetry with the deletes
# above is the API's, not a typo here.
DELETE {{base_url}}/api/admin/users/{{ann_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


DELETE {{base_url}}/api/admin/users/{{bo_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
