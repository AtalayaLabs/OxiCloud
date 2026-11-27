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
# Step 12 — HOST PAGE SECURITY HEADERS.
#
# `GET /wopi/edit/{id}` is the server-rendered page that performs
# the editor handshake. It exists so the cross-origin form POST
# happens where the editor's origin is KNOWN at request time:
# the SPA's own policy is a build-time <meta> that cannot name an
# operator's editor host, so letting the SPA post meant allowing
# a whole scheme (`form-action https:`) — which excluded a
# cleartext editor outright and let ANY https origin receive a
# token-bearing form.
#
# These assertions are the delivery half. Unit tests
# (`origin_of_tests`) pin how the origin is computed; only a live
# request proves the header actually ships, and two of the values
# below are cross-layer and invisible to any unit test:
#
#   * `X-Frame-Options: SAMEORIGIN` — the global security-header
#     middleware defaults this to DENY and now skips responses
#     that set their own. Regress that and the editor silently
#     becomes unframeable: the SPA iframes this page, the browser
#     refuses, and the user sees an empty modal with only a
#     console message. The same clobber is why FileViewer
#     round-trips PDFs through a `blob:`.
#   * the CSP survives at all — the middleware's non-HTML branch
#     used to `insert` the app policy unconditionally, replacing
#     any policy a handler had set.
#
# `http://mock-editor.invalid` is the `urlsrc` origin from
# tests/common/wopi_mock_discovery.js — NOT the origin of
# OXICLOUD_WOPI_DISCOVERY_URL (127.0.0.1:9100). The mock keeps
# those two deliberately different, which is the point: only
# `urlsrc` is where the POST goes, and an implementation that
# reached for the discovery URL's origin instead would pass every
# unit test and fail here.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/wopi/editor-url?file_id={{file_id}}&action=edit
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
host_page_token: jsonpath "$.access_token"


GET {{base_url}}/wopi/edit/{{file_id}}?access_token={{host_page_token}}

HTTP 200
[Asserts]
header "Content-Type" contains "text/html"
# The one origin the handshake may post to, and the only origin
# the page may frame. A wildcard or a scheme here means someone
# widened the policy instead of naming the host.
#
# `matches` with a `[^;]*` scope, not `contains`: a CSP source list
# is UNORDERED, so any substring spanning two tokens only catches
# one spelling of the same policy.
header "Content-Security-Policy" matches "form-action[^;]*http://mock-editor[.]invalid"
header "Content-Security-Policy" matches "frame-src[^;]*http://mock-editor[.]invalid"
header "Content-Security-Policy" not matches "form-action[^;]*[*]"
header "Content-Security-Policy" not matches "frame-src[^;]*[*]"
header "Content-Security-Policy" not matches "form-action[^;]*https:"
# Closed by default: nothing but the nonced <script>/<style> runs.
header "Content-Security-Policy" contains "default-src 'none'"
header "Content-Security-Policy" contains "script-src 'nonce-"
header "Content-Security-Policy" not contains "unsafe-inline"
# The SPA frames this page; `'self'` is what permits that, and
# the explicit XFO below is what stops the global DENY default
# from overruling it.
header "Content-Security-Policy" contains "frame-ancestors 'self'"
header "X-Frame-Options" == "SAMEORIGIN"
# The body embeds a freshly minted access token — never cache it.
header "Cache-Control" contains "no-store"
# The auto-submit script must carry a nonce, or the page's own CSP
# blocks it and the handshake never fires. Pins that the nonce is
# threaded into the markup, not just into the header.
body contains "<script nonce="
body contains "action=\"http://mock-editor.invalid/edit?"


# A bad token must not render the page at all — the handshake
# embeds a re-minted token, so an unauthenticated caller reaching
# this page would be handed one.
GET {{base_url}}/wopi/edit/{{file_id}}?access_token=not-a-real-token

HTTP 401


# ─────────────────────────────────────────────────────────────
# Step 13 — View-only legacy `.doc` fallback + user-confirmed
#           convert + PutRelativeFile (OnlyOffice workflow).
#
# Regression pin: OnlyOffice discovery advertises `view` and
# `convert` (targetext="docx") for `.doc`, with NO `edit` action.
# Previously `action=edit` returned 422 on open.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{alice_home_id}}
file: file,fixtures/legacy.doc; application/msword

HTTP 201
[Captures]
doc_file_id: jsonpath "$.id"


# Owner requests `action=edit` on `.doc` → resolves to `view`
# with `convert_targets: ["docx"]` instead of 422.
GET {{base_url}}/api/wopi/editor-url?file_id={{doc_file_id}}&action=edit
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
doc_view_token: jsonpath "$.access_token"
[Asserts]
jsonpath "$.action" == "view"
jsonpath "$.editor_url" contains "/view?"
jsonpath "$.convert_targets[0]" == "docx"


# View fallback must never mint a write-capable token.
GET {{base_url}}/wopi/files/{{doc_file_id}}?access_token={{doc_view_token}}

HTTP 200
[Asserts]
jsonpath "$.UserCanWrite" == false
jsonpath "$.SupportsUpdate" == false
jsonpath "$.UserCanNotWriteRelative" == true


GET {{base_url}}/wopi/edit/{{doc_file_id}}?access_token={{doc_view_token}}

HTTP 200
[Asserts]
body contains "action=\"http://mock-editor.invalid/view?"


POST {{base_url}}/wopi/files/{{doc_file_id}}/contents?access_token={{doc_view_token}}
Content-Type: application/octet-stream
```
must fail on view token
```

HTTP 401


# Viewer granted Read on Alice's `.doc` sees `view` with NO
# `convert_targets`, and `action=convert` is refused with 403
# (lacks Create on Alice's parent folder).
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file", "id": "{{doc_file_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
bob_doc_grant_id: jsonpath "$.grants[0].id"


GET {{base_url}}/api/wopi/editor-url?file_id={{doc_file_id}}&action=edit
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.action" == "view"
jsonpath "$.convert_targets" not exists


GET {{base_url}}/api/wopi/editor-url?file_id={{doc_file_id}}&action=convert&target_ext=docx
Authorization: Bearer {{bob_token}}

HTTP 403


DELETE {{base_url}}/api/grants/{{bob_doc_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# Owner confirms conversion to `.docx` → `action=convert` resolves
# discovery's `convert` URL, CheckFileInfo advertises relative write
# support, PutFile on the source `.doc` stays blocked (401), and
# PUT_RELATIVE creates `legacy.docx` (then `legacy (1).docx` on
# collision) while leaving the original `.doc` intact.
GET {{base_url}}/api/wopi/editor-url?file_id={{doc_file_id}}&action=convert&target_ext=docx
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
doc_convert_token: jsonpath "$.access_token"
[Asserts]
jsonpath "$.action" == "convert"
jsonpath "$.editor_url" contains "/convert?"


GET {{base_url}}/wopi/edit/{{doc_file_id}}?access_token={{doc_convert_token}}

HTTP 200
[Asserts]
body contains "action=\"http://mock-editor.invalid/convert?"


GET {{base_url}}/wopi/files/{{doc_file_id}}?access_token={{doc_convert_token}}

HTTP 200
[Asserts]
jsonpath "$.UserCanWrite" == true
jsonpath "$.SupportsUpdate" == true
jsonpath "$.UserCanNotWriteRelative" == false


POST {{base_url}}/wopi/files/{{doc_file_id}}/contents?access_token={{doc_convert_token}}
Content-Type: application/octet-stream
```
convert token must never overwrite original .doc
```

HTTP 401


POST {{base_url}}/wopi/files/{{doc_file_id}}?access_token={{doc_convert_token}}
X-WOPI-Override: PUT_RELATIVE
X-WOPI-SuggestedTarget: .docx
X-WOPI-FileConversion: True
Content-Type: application/octet-stream
```
converted docx bytes v1
```

HTTP 200
[Captures]
converted_docx_id: jsonpath "$.HostEditUrl" regex "file=([^&]+)"
converted_docx_wopi_url: jsonpath "$.Url"
[Asserts]
jsonpath "$.Name" == "legacy.docx"
jsonpath "$.Url" contains "/wopi/files/"
jsonpath "$.HostEditUrl" contains "?file="


GET {{converted_docx_wopi_url}}

HTTP 200
[Asserts]
jsonpath "$.BaseFileName" == "legacy.docx"
jsonpath "$.UserCanWrite" == true


GET {{base_url}}/api/files/{{converted_docx_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
body contains "converted docx bytes v1"


# Second SuggestedTarget conversion avoids colliding with legacy.docx.
POST {{base_url}}/wopi/files/{{doc_file_id}}?access_token={{doc_convert_token}}
X-WOPI-Override: PUT_RELATIVE
X-WOPI-SuggestedTarget: .docx
X-WOPI-FileConversion: True
Content-Type: application/octet-stream
```
converted docx bytes v2
```

HTTP 200
[Captures]
converted_docx_copy_id: jsonpath "$.HostEditUrl" regex "file=([^&]+)"
[Asserts]
jsonpath "$.Name" == "legacy (1).docx"


DELETE {{base_url}}/api/files/{{doc_file_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{converted_docx_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{converted_docx_copy_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 14 — Collabora edit-only `.xlsx` requested in `view` mode.
#
# Regression pin: `host_page` used to require an exact `"view"`
# discovery entry and returned 500 when discovery only listed
# `"edit"`. Now it falls back to `"edit"` URL while keeping
# `UserCanWrite=false`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{alice_home_id}}
file: file,fixtures/doc-date.xlsx; application/vnd.openxmlformats-officedocument.spreadsheetml.sheet

HTTP 201
[Captures]
xlsx_file_id: jsonpath "$.id"


GET {{base_url}}/api/wopi/editor-url?file_id={{xlsx_file_id}}&action=view
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
xlsx_view_token: jsonpath "$.access_token"
[Asserts]
jsonpath "$.action" == "view"
jsonpath "$.editor_url" contains "/edit?"


GET {{base_url}}/wopi/files/{{xlsx_file_id}}?access_token={{xlsx_view_token}}

HTTP 200
[Asserts]
jsonpath "$.UserCanWrite" == false
jsonpath "$.SupportsUpdate" == false


GET {{base_url}}/wopi/edit/{{xlsx_file_id}}?access_token={{xlsx_view_token}}

HTTP 200
[Captures]
xlsx_host_token: body regex "name=\"access_token\" value=\"([^\"]+)\""
[Asserts]
body contains "action=\"http://mock-editor.invalid/edit?"


# Regression pin: host_page must preserve view mode and not re-elevate an
# Owner's explicit action=view session on an edit-only discovery format back
# to writable when re-minting the token.
GET {{base_url}}/wopi/files/{{xlsx_file_id}}?access_token={{xlsx_host_token}}

HTTP 200
[Asserts]
jsonpath "$.UserCanWrite" == false
jsonpath "$.SupportsUpdate" == false


DELETE {{base_url}}/api/files/{{xlsx_file_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Cleanup — delete the test file so subsequent Hurl files don't
# see it. Bob user stays; other tests may reuse the `wopi-bob`
# username, but the grants that made this test meaningful are
# gone.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{alice_token}}

HTTP 204
