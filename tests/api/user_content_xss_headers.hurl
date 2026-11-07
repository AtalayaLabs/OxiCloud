# =============================================================
# OxiCloud — stored XSS via user-uploaded HTML / SVG (user report)
#
# A user reported that uploading an .html file and opening it inline
# executed its script. Confirmed:
#
#   GET /api/files/{id}?inline=true
#     → Content-Type: text/html
#     → Content-Disposition: inline
#     → NO Content-Security-Policy
#
# a fully-privileged same-origin document. Its inline script could
# call /api/* with the victim's cookies (HttpOnly does not help — the
# script never reads the cookie, the browser attaches it).
#
# ROOT CAUSE, and why this file pins headers rather than behaviour:
# the global CSP middleware in `main.rs` deliberately sends no CSP on
# `text/html` so the SPA's own hash-based <meta> policy governs the
# shell. That branch keys on Content-Type, which cannot distinguish
# our shell from a file a user uploaded. The fix adds a user-content
# branch BEFORE it, keyed on `Content-Disposition` — already this
# codebase's meaning of "this response is a file body" (the gzip
# predicate uses the same test), so every download handler present and
# future is covered without anyone maintaining a route list.
#
# Delivery paths that made it exploitable, for the record:
#   * the file viewer's "open in new tab" anchor (`?inline=true`)
#   * the global search box, which opened ANY picked file with
#     window.open(fileInlineUrl(id)) and no mime check
#   * a direct link from anyone who can drop a file where the victim
#     can read it — including an anonymous public-share visitor, since
#     /s/{token} is only a login method and the download itself goes
#     through these same /api/files routes
#
# What is pinned here:
#   1. an uploaded .html is sandboxed AND served as attachment
#   2. ?inline=true can no longer promote it to inline
#   3. an uploaded .svg gets the same treatment — it needed no
#      ?inline=true at all, since image/* matched the inline allowlist
#   4. a PNG still renders inline and is NOT sandboxed (the fix must
#      not break image preview — a gate that neutralised everything
#      would satisfy 1–3 just as well, which is why this case exists)
#   5. nosniff stays on everything
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/user_content_xss_headers.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Working folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-user-content-xss"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Upload the HTML payload. Content-Type is declared
# text/html on the multipart part, i.e. exactly what a browser
# sends when a user drags an .html file in. Nothing exotic is
# needed to reproduce this.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/xss-payload.html; text/html

HTTP 201
[Captures]
html_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — THE REGRESSION. The exact request from the report:
# plain inline fetch of the uploaded HTML.
#
# `sandbox` with no allow-* tokens is what makes the document
# inert: scripts disabled, opaque origin, so even if execution
# were somehow reached it has no access to app-origin cookies or
# storage. Asserted as a substring because the directive order
# in the header is not what matters — the presence of `sandbox`
# with no `allow-scripts` is.
#
# `allow-scripts` and `allow-same-origin` are asserted ABSENT
# individually: either one re-opens this, and `allow-scripts`
# together with `allow-same-origin` restores the vulnerability
# in full. A future "make the preview work again" patch that
# adds one is the single most likely way this regresses.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{html_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Security-Policy" contains "sandbox"
header "Content-Security-Policy" contains "default-src 'none'"
header "Content-Security-Policy" not contains "allow-scripts"
header "Content-Security-Policy" not contains "allow-same-origin"
# Second layer, independent of the CSP: ?inline=true must no longer
# promote a scriptable document to inline. A header can be dropped by
# a proxy or lost in a middleware refactor; the disposition cannot.
header "Content-Disposition" startsWith "attachment"
header "X-Content-Type-Options" == "nosniff"


# ─────────────────────────────────────────────────────────────
# Step 5 — Same file without the query param, since the default
# path must not be weaker than the explicit one.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{html_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Security-Policy" contains "sandbox"
header "Content-Disposition" startsWith "attachment"


# ─────────────────────────────────────────────────────────────
# Step 6 — Upload the SVG payload.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/xss-payload.svg; image/svg+xml

HTTP 201
[Captures]
svg_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 7 — SVG is the quieter half: `image/svg+xml` matched
# `starts_with("image/")`, so it was served INLINE with no
# ?inline=true needed. An SVG is a scriptable document, not a
# raster image.
#
# The SPA is unaffected by the attachment disposition here: it
# renders previews through <img src=…>, and Content-Disposition
# is ignored for subresource loads. Only a top-level navigation
# — the thing that executed the script — changes behaviour.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{svg_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Security-Policy" contains "sandbox"
header "Content-Security-Policy" not contains "allow-scripts"
header "Content-Disposition" startsWith "attachment"


# ─────────────────────────────────────────────────────────────
# Step 8 — THE OTHER HALF OF THE FIX: a real image must still
# render inline, and must NOT be sandboxed.
#
# Without this, a gate that neutralised every download would
# pass steps 4–7 and silently break image, video and PDF
# preview.
#
# It still carries a CSP — every user-content body does — just
# not the sandboxing one. The split is driven by the single
# `is_scriptable_document` classifier that also decides the
# disposition, so `inline` and "not sandboxed" can never
# disagree. `default-src` is asserted ABSENT because it is the
# one directive that could stop a natively-rendered body from
# displaying, and the built-in PDF viewer is the fragile case.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/blue-image.png; image/png

HTTP 201
[Captures]
png_id: jsonpath "$.id"

GET {{base_url}}/api/files/{{png_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Disposition" startsWith "inline"
header "Content-Security-Policy" not contains "sandbox"
header "Content-Security-Policy" not contains "default-src"
header "Content-Security-Policy" contains "frame-ancestors 'none'"
header "X-Content-Type-Options" == "nosniff"


# ─────────────────────────────────────────────────────────────
# Step 9 — Cleanup. Hurl files share one DB; leaving the folder
# behind would leak into the suite's end-of-run storage sweep.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{html_id}}
Authorization: Bearer {{token}}

HTTP *

DELETE {{base_url}}/api/files/{{svg_id}}
Authorization: Bearer {{token}}

HTTP *

DELETE {{base_url}}/api/files/{{png_id}}
Authorization: Bearer {{token}}

HTTP *

DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP *
