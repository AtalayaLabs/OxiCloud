# =============================================================
# OxiCloud — WOPI authorization at token redemption
# =============================================================
# Regression coverage for the WOPI verb-handler bypass documented in
# memory note `wopi-authz-bypass`. Two bugs closed:
#
#   1. Verb handlers (check_file_info, get_file, put_file,
#      file_operations, host_page) previously did NOT call
#      `AuthorizationEngine::require` at redemption. A grant
#      revoked between mint-time and request-time silently kept
#      working until the token TTL expired.
#
#   2. The mint helper decided `can_write` from the client's
#      `requested_action` string (`!= "view"` → write). A Viewer
#      clicking "Edit in Collabora" received a write-capable
#      token because the string was "edit".
#
# The fix wires `authz.require` on every verb and derives
# `can_write` from the caller's actual Update permission. This
# suite hits both paths through the real HTTP surface.
#
# Note on infra:
#   * `OXICLOUD_WOPI_ENABLED=true` in tests/common/server.env
#   * `OXICLOUD_WOPI_SECRET` pinned so the tokens the server mints
#     round-trip verify-able through the suite
#   * WOPI discovery served by `tests/common/wopi_mock_discovery.py`
#     started by run.sh — mock URL points at a black-hole editor
#     so we only assert on OxiCloud's own responses
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login as admin (owner) and capture home folder id
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
alice_user_id: jsonpath "$.user.full.user.id"


GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create a Bob user (Viewer under test) via admin API
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "wopi-bob",
  "password": "WopiBobPassword1!",
  "email": "wopi-bob@example.com",
  "role": "user"
}

HTTP 201
[Captures]
bob_user_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "wopi-bob", "password": "WopiBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 3 — Alice uploads a plain-text file the WOPI verbs will
#          target. `text/plain` is in the mock discovery XML so
#          `/api/wopi/editor-url` resolves to a real (black-hole)
#          editor URL — the endpoint returns 200 with an
#          access_token we can then poke at the verbs.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{alice_home_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.mime_type" == "text/plain"


# ─────────────────────────────────────────────────────────────
# Step 4 — Alice mints an editor-URL for her own file with
#          `action=edit`. Owner has Update → can_write=true.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/wopi/editor-url?file_id={{file_id}}&action=edit
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_edit_token: jsonpath "$.access_token"
[Asserts]
jsonpath "$.access_token" isString
jsonpath "$.editor_url" contains "edit"


# ─────────────────────────────────────────────────────────────
# Step 5 — CheckFileInfo with the owner's edit token. Verb
#          re-checks Read → allowed. `user_can_write=true`
#          reflects real Update.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/wopi/files/{{file_id}}?access_token={{alice_edit_token}}

HTTP 200
[Asserts]
jsonpath "$.UserId" == "{{alice_user_id}}"
jsonpath "$.UserCanWrite" == true
jsonpath "$.SupportsUpdate" == true


# ─────────────────────────────────────────────────────────────
# Step 6 — GetFile with the owner's edit token. Verb re-checks
#          Read → 200 with body.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{alice_edit_token}}

HTTP 200
[Asserts]
body contains "Hello"


# ─────────────────────────────────────────────────────────────
# Step 7 — PutFile with the owner's edit token. Verb re-checks
#          Update → 200. The owner overwrites her own file.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{alice_edit_token}}
Content-Type: application/octet-stream
```
owner overwrite via WOPI PutFile
```

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 8 — Alice explicitly requests view mode. Even the owner
#          gets `can_write=false` — the token respects the
#          client's downgrade so Collabora can open a doc
#          "read-only for co-browsing".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/wopi/editor-url?file_id={{file_id}}&action=view
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_view_token: jsonpath "$.access_token"


GET {{base_url}}/wopi/files/{{file_id}}?access_token={{alice_view_token}}

HTTP 200
[Asserts]
# Owner explicitly requested view — supports_update flips off.
jsonpath "$.UserCanWrite" == false
jsonpath "$.SupportsUpdate" == false


# View token trying to write → 401 (token's can_write bit says no
# before the authz.require ever runs).
POST {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{alice_view_token}}
Content-Type: application/octet-stream
```
owner trying to write with view token
```

HTTP 401


# ─────────────────────────────────────────────────────────────
# Step 9 — SECURITY: Bob has NO grant on Alice's file. Requests
#          an edit-URL. The mint helper's Read gate fires → 404
#          (anti-enum). This is the pre-fix behaviour holding
#          — mint-time Read was already enforced via
#          get_file_with_perms.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/wopi/editor-url?file_id={{file_id}}&action=edit
Authorization: Bearer {{bob_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 10 — Alice grants Bob the Viewer role on the file.
#           Capture the grant id off the POST response so Step
#           13's revoke doesn't need to LIST + filter (the LIST
#           endpoint returns a bare JSON array, not
#           `.grants[?...]`, and Hurl's single-match filter
#           capture behaviour is quirky — see memory note
#           `feedback_hurl_jsonpath_filter_empty`).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file", "id": "{{file_id}}" },
  "role": "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 11 — SECURITY: Bob (Viewer) requests an EDIT token. Fix
#           #12: mint helper derives `can_write` from real
#           Update permission, not from the requested_action
#           string. Bob has Read but not Update → token is
#           minted with `can_write=false` even though he asked
#           for "edit".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/wopi/editor-url?file_id={{file_id}}&action=edit
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_forged_edit_token: jsonpath "$.access_token"


# CheckFileInfo with Bob's "edit" token shows UserCanWrite=false
# because the token's can_write bit was scrubbed at mint. Prior
# to the fix this was `true` — a Viewer editing Alice's file.
GET {{base_url}}/wopi/files/{{file_id}}?access_token={{bob_forged_edit_token}}

HTTP 200
[Asserts]
jsonpath "$.UserId" == "{{bob_user_id}}"
jsonpath "$.UserCanWrite" == false
jsonpath "$.SupportsUpdate" == false


# Bob attempting PutFile with his "edit" token → 401. The
# token's own can_write=false is the outer gate; even if the
# token had somehow been forged with can_write=true, the
# redemption-time authz.require(Update) would return 404.
POST {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{bob_forged_edit_token}}
Content-Type: application/octet-stream
```
Bob trying to write as Viewer
```

HTTP 401


# Bob CAN read (his Read grant is real).
GET {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{bob_forged_edit_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 12 — SECURITY: promote Bob to Editor. Now he legitimately
#           holds Update, so an edit token becomes truly write-
#           capable.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file", "id": "{{file_id}}" },
  "role": "editor"
}

# The engine's `ON CONFLICT UPDATE` collapses one role row per
# (subject, resource), so this Editor grant REPLACES the Viewer
# grant from Step 10 rather than stacking. Bob now holds
# Editor alone; revoking it in Step 13 leaves him with no
# grants at all.
HTTP 201
[Captures]
bob_grant_id: jsonpath "$.grants[0].id"


GET {{base_url}}/api/wopi/editor-url?file_id={{file_id}}&action=edit
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_real_edit_token: jsonpath "$.access_token"


GET {{base_url}}/wopi/files/{{file_id}}?access_token={{bob_real_edit_token}}

HTTP 200
[Asserts]
# Bob is a real Editor now → can_write flips to true.
jsonpath "$.UserCanWrite" == true
jsonpath "$.SupportsUpdate" == true


POST {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{bob_real_edit_token}}
Content-Type: application/octet-stream
```
Bob as Editor legitimately writes
```

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 13 — SECURITY: revoke Bob's grant AFTER his edit token was
#           minted. The token stays cryptographically valid until
#           TTL, but every subsequent verb call must hit the
#           authorization engine and reject.
#
#           This is the CORE bug the memory note describes: prior
#           to the fix Bob's PutFile still succeeded here because
#           the verb handlers trusted the token in isolation.
#
#           The Editor grant from Step 12 REPLACED the Viewer
#           grant from Step 10 (engine's ON CONFLICT UPDATE —
#           one role row per subject/resource). So revoking the
#           Editor grant leaves Bob with no grants at all; every
#           verb — Read AND Update — must refuse.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{bob_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# CheckFileInfo — no Read → 404. Prior to the fix the verb
# handler trusted the token and returned 200 with the file's
# metadata.
GET {{base_url}}/wopi/files/{{file_id}}?access_token={{bob_real_edit_token}}

HTTP 404


# GetFile — no Read → 404. Prior to the fix Bob could still
# download the file content until the token TTL expired.
GET {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{bob_real_edit_token}}

HTTP 404


# PutFile — no Update → 404 (verb-side require_wopi_perm), OR
# 401 if the token's own `!claims.can_write` gate happened to
# fire first. The important assertion is "not 200" — a revoked
# grant must never let the caller through.
POST {{base_url}}/wopi/files/{{file_id}}/contents?access_token={{bob_real_edit_token}}
Content-Type: application/octet-stream
```
Bob post-revoke tries to write
```

HTTP 404


# ─────────────────────────────────────────────────────────────
# Cleanup — delete the test file so subsequent Hurl files don't
# see it. Bob user stays; other tests may reuse the `wopi-bob`
# username, but the grants that made this test meaningful are
# gone.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{alice_token}}

HTTP 204
