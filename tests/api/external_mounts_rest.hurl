# =============================================================
# OxiCloud — External file mounts over the REST API (the SPA)
# =============================================================
# Regression pins for #793 (reported 2026-10-06): REST prechecks
# parsed `ext:<mount>:<node>` ids as UUIDs and answered 404 before
# the handlers' mount branches could run. Only the mount root (a
# real UUID) was usable from the web UI; every subfolder and every
# file inside a mount 404'd — browse, download, preview, metadata,
# upload into a subfolder.
#
# Shape: writable local_fs mount on /tmp, mkdir a subfolder and
# upload a file through the mount root (always worked), then pin
# every SPA call on the `ext:` ids. Thumbnails of mount files are
# the documented 204 ("no thumbnail") instead of 404; metadata is
# the "no EXIF" body. A stranger is denied as 404 (anti-enum).
# Cleanup removes the created entries (mounts have no trash).
#
# Requires OXICLOUD_ENABLE_EXTERNAL_MOUNTS=true (server.env).
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# Capture admin's user id for the #837 effective_grants assertion
# below — the filter needs to pin which SUBJECT holds the owner
# grant, not just that *some* owner grant exists. `$.full.user.id`
# (not `$.id`): `/api/auth/me` returns the Self view composed over
# Full over Public (user_dto.rs), and the layers are NOT
# serde-flattened — the nesting is on the wire.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{token}}

HTTP 200
[Captures]
admin_user_id: jsonpath "$.full.user.id"


GET {{base_url}}/api/drives
Authorization: Bearer {{token}}

HTTP 200
[Captures]
drive_id: jsonpath "$[0].id"


POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-rest-mount", "host_path": "{{mount_rest_root}}", "read_only": false, "drive_id": "{{drive_id}}" }

HTTP 201
[Captures]
mount_id: jsonpath "$.mount_folder_id"


# ─────────────────────────────────────────────────────────────
# content_modified_at over an external mount — REST surface.
#
# `hello-mtime.txt` is seeded by `run.sh::seed_mount_fixtures` with a
# FIXED mtime of 2021-01-01T12:00:00Z (`TZ=UTC touch -t`). It cannot be
# produced through the API: the local provider only READS
# `meta.modified()` (`local_fs_mount_provider.rs:163,258`) and has no
# write path for file times, so not even an upload carrying `X-OC-Mtime`
# can set it.
#
# What this pins: the provider's `modified_at` (unix seconds from the
# host filesystem) survives the trip through `mount_entry_file_dto` and
# `epoch_to_datetime` to the wire as RFC 3339 UTC — so a mount file's
# date is the FILE's date, not the date the server happened to see it.
# The WebDAV half of the same fixture is in `external_mounts_webdav.hurl`,
# and the two asserting the same instant on different surfaces is the
# point: they must not drift.
#
# Filtered jsonpath + `==` rather than `[0]`: the mount root holds both
# this fixture and the subfolder created below, so index order is not
# guaranteed. Interpolating a variable inside the filter is the same
# shape `grants.hurl` uses for `@.subject.id`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{mount_id}}/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.name=='{{mount_mtime_file}}')].resource.modified_at" == "2021-01-01T12:00:00Z"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-rest-sub", "parent_id": "{{mount_id}}" }

HTTP 201
[Captures]
sub_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"


# ─────────────────────────────────────────────────────────────
# #793 — upload INTO the `ext:` subfolder (precheck on the folder).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{sub_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"
jsonpath "$.folder_id" == "{{sub_id}}"


# Folder metadata + listing of the `ext:` subfolder.
GET {{base_url}}/api/folders/{{sub_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{sub_id}}"
jsonpath "$.name" == "hurl-rest-sub"
jsonpath "$.parent_id" == "{{mount_id}}"


GET {{base_url}}/api/folders/{{sub_id}}/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
# Why not `$.items[*].resource.name includes "hello.txt"` (the original):
# Hurl collapses a single-element jsonpath result to a SCALAR, so with one
# file in the subfolder that expression evaluates to the string
# "hello.txt" and `includes` — a collection predicate — fails with "types
# between actual and expected are not consistent". The server returned the
# right thing; only the operator was wrong.
#
# Why not `$.items[?(@.resource.name=='hello.txt')] count == 1` either:
# a filter yielding one match is just as collapse-prone, and nothing else
# in this suite asserts `count` on a filtered expression, so its behaviour
# here is unverified. The forms below use only primitives with heavy prior
# art — `count` on a plain array field, and `==` on a single indexed
# element (which collapses to a scalar, where `==` is exactly right).
#
# Deliberately asserts name and not mime_type: mime is the subject of the
# content-sniffing work and would make this pin fail for an unrelated
# reason.
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.name" == "hello.txt"


# ─────────────────────────────────────────────────────────────
# #837 — breadcrumb for a mount subfolder.
#
# `FolderService::get_ancestors_with_perms` gated on
# `folder_resource(leaf_id)` which `Uuid::parse_str`'d the id and
# mapped failures to NotFound — so every `ext:` id (every subfolder
# below a mount root) returned 404, breaking the web UI's "open
# subfolder" path even though WebDAV and `/resources` worked.
#
# Pins (as the mount owner):
#   * 200 (not 404).
#   * The returned chain has at least two entries — the mount root
#     and the leaf below it. The in-mount hierarchy may lift the
#     chain into the mount's native ancestors (drive root, etc.);
#     at minimum the two we created live in it.
#   * The LAST entry (leaf) is the subfolder we created.
#   * The mount root appears somewhere in the chain.
#
# The `.id` field type is NOT asserted here — the fix extends it
# to accept both native UUIDs and `ext:` ids; a Hurl assertion on
# the exact wire shape would couple this pin to the DTO rename
# unnecessarily. The two name/length pins catch the regression
# without that coupling.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{sub_id}}/ancestors
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ancestors" count >= 2
jsonpath "$.ancestors[-1].name" == "hurl-rest-sub"
# Hurl collapses a single-element filter match to a scalar, so a
# `count == 1` assertion fails with "invalid filter input type"
# (the input is the matched object, not a list). Project onto
# `.name` and compare the scalar instead; a zero-match would
# fail parsing, a multi-match wouldn't collapse and would also
# fail the `==` — covers "exists exactly once" without the
# collection-count call.
jsonpath "$.ancestors[?(@.name=='hurl-rest-mount')].name" == "hurl-rest-mount"


# ─────────────────────────────────────────────────────────────
# #837 — nested subfolder (two segments deep).
#
# `node_id` for the provider is the POSIX path relative to the
# mount root, so `hurl-rest-sub/nested-leaf` is split into TWO
# segments by `get_ancestors_with_perms`'s synthetic-tail loop.
# A regression that only walked the LAST segment (producing
# `[mount, nested-leaf]` instead of `[mount, hurl-rest-sub,
# nested-leaf]`) would still pass the single-level pin above —
# this one catches it.
#
# Pins:
#   * `hurl-rest-sub` AND `nested-leaf` BOTH appear in the chain.
#   * `nested-leaf` sits strictly after `hurl-rest-sub` (chain is
#     root-first, leaf-last). Asserted by checking the leaf is
#     the last entry and the intermediate is anywhere in it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "nested-leaf", "parent_id": "{{sub_id}}" }

HTTP 201
[Captures]
nested_sub_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"
jsonpath "$.parent_id" == "{{sub_id}}"


GET {{base_url}}/api/folders/{{nested_sub_id}}/ancestors
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ancestors" count >= 3
jsonpath "$.ancestors[-1].name" == "nested-leaf"
# Scalar-projection trick to avoid Hurl's collapse-on-single-match
# (see earlier note on the one-segment case).
jsonpath "$.ancestors[?(@.name=='hurl-rest-sub')].name" == "hurl-rest-sub"
jsonpath "$.ancestors[?(@.name=='hurl-rest-mount')].name" == "hurl-rest-mount"


# ─────────────────────────────────────────────────────────────
# #837 — effective_grants on a mount-child leaf.
#
# `?include_grants=true` RAISES the required permission to Share
# (the gate right below the Read one in `get_ancestors_with_perms`).
# The grants collector runs over the NATIVE rows only (mount root
# and its native ancestors) — synthetic mount-child entries have
# no `role_grants` row of their own. The expected invariant:
# `effective_grants` for a leaf INSIDE a mount is the same set
# as `effective_grants` for the mount root itself.
#
# Three failure modes this pin catches:
#   1. "Grants collection skipped on the mount-child path" →
#      `effective_grants: []`. Caught by the `>= 1` assertion.
#   2. "Grants duplicated per synthetic segment" (one copy per
#      path depth) → the equivalence check with the mount root's
#      count would see 2× or 3×. Caught by `count ==` on the
#      second call.
#   3. "Reading from the wrong drive scope" → subjects wouldn't
#      include the admin. Caught by the subject-filter assertion.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{nested_sub_id}}/ancestors?include_grants=true
Authorization: Bearer {{token}}

HTTP 200
[Captures]
mount_child_grants_count: jsonpath "$.effective_grants" count
[Asserts]
jsonpath "$.effective_grants" exists
jsonpath "$.effective_grants" count >= 1
# Admin's drive-owner grant on their personal drive must be in
# there — the mount lives in admin's personal drive and the
# `role_grants` row seeded at user creation is `(admin, drive,
# <personal-drive>, owner)`.
#
# Compound filter on `subject.id` AND `role` so a regression that
# leaked a DIFFERENT user's owner grant (e.g. by reading from the
# wrong drive scope) is caught. `SubjectDto` has no `name` field —
# `{type, id}` only — so the admin match goes through the captured
# `admin_user_id`. Scalar projection on `.role` to work around
# Hurl's single-element collapse (the filter yields the matched
# object, not a one-element list).
jsonpath "$.effective_grants[?(@.subject.id=='{{admin_user_id}}' && @.role=='owner')].role" == "owner"


# Equivalence check: querying the mount root with the same flag
# must return the SAME set. If the fix duplicated grants per
# synthetic segment, this count would mismatch.
GET {{base_url}}/api/folders/{{mount_id}}/ancestors?include_grants=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.effective_grants" count == {{mount_child_grants_count}}


# Download, preview, metadata of the `ext:` file.
GET {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Length" == "32"
body == "Hello from OxiCloud Hurl tests.\n"


GET {{base_url}}/api/files/{{file_id}}?metadata=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.name" == "hello.txt"
jsonpath "$.size" == 32


GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/files/{{file_id}}/metadata
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.message" == "No EXIF metadata available"


# ─────────────────────────────────────────────────────────────
# Stranger: every one of the calls above is a 404 (anti-enum).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{ "username": "mount_rest_eve", "email": "mount_rest_eve@example.com", "password": "MountRestEve1!" }

# 200 (anti-enum) or 201 depending on the registration policy; the login
# below is what this scenario relies on.
HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mount_rest_eve", "password": "MountRestEve1!" }

HTTP 200
[Captures]
eve_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders/{{sub_id}}/resources
Authorization: Bearer {{eve_token}}

HTTP 404


# #837 — ancestor walk denies as 404 for an outsider, matching the
# anti-enumeration shape every other mount-subfolder call uses here.
# The fix must keep this: Resource::Folder(mount_id) is the AuthZ gate
# the ancestor method shares with /resources above, so a regression
# that leaks "folder exists but you can't see its chain" via 403
# shows up as a mismatch between these two calls.
#
# Covers BOTH depth levels (one-segment `sub_id`, two-segment
# `nested_sub_id`) because the gate collapses every `ext:` id onto
# `Resource::Folder(mount_id)` — if that collapse ever drifted and
# one depth leaked while another denied, the asymmetry shows here.
GET {{base_url}}/api/folders/{{sub_id}}/ancestors
Authorization: Bearer {{eve_token}}

HTTP 404


GET {{base_url}}/api/folders/{{nested_sub_id}}/ancestors
Authorization: Bearer {{eve_token}}

HTTP 404


GET {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{eve_token}}

HTTP 404


GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{eve_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Cleanup — permanent inside a mount.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP *


# Delete nested-leaf before its parent — mount deletes are
# non-recursive at this layer (the provider's rmdir rejects
# non-empty directories), so the leaf-first order matters.
DELETE {{base_url}}/api/folders/{{nested_sub_id}}
Authorization: Bearer {{token}}

HTTP *


DELETE {{base_url}}/api/folders/{{sub_id}}
Authorization: Bearer {{token}}

HTTP *


DELETE {{base_url}}/api/admin/external-mounts/{{mount_id}}
Authorization: Bearer {{token}}

HTTP 204
