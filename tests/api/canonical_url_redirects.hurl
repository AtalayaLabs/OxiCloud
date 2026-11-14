# =============================================================
# OxiCloud – canonical-URL redirects
#
# Pins the alias table in `interfaces/web/mod.rs::CANONICAL_REDIRECTS`,
# wired as a sub-router by `canonical_redirects_router()`. Each alias
# is registered as its own GET route emitting a 301 to the canonical
# form, so the Axum trie matches and short-circuits before the SPA
# shell fallback catches anything.
#
# Matrix:
#   /mails     -> /mail
#   /mailbox   -> /mail
#   /calendar  -> /calendars
#   /contact   -> /contacts
#   /file      -> /files
#   /photo     -> /photos
#   /group     -> /groups
#   /favorite  -> /favorites
#   /inbox     -> /mail       (Gmail/NC synonym)
#   /me        -> /profile    (Google/Twitter synonym)
#   /drive     -> /files      (Google Drive synonym)
#
# Also pinned:
#   * trailing-slash tolerance (`/mails/` → `/mail` same as `/mails`)
#   * query-string preservation (`/mails?foo=bar` → `/mail?foo=bar`)
#   * unmatched paths fall through (`/mailto` NOT redirected)
#
# The destinations may 404 today (`/mail` and `/calendars` SPA pages
# ship on `feat/shell-tabbed`). The test asserts the REDIRECT shape,
# not what the destination serves — a 301 to a 404 is still a correct
# 301.
#
# No auth required: the middleware runs on the public web layer.
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        tests/api/canonical_url_redirects.hurl
# =============================================================

# ─────────────────────────────────────────────────────────────
# 1. Every alias returns 301 with the expected Location.
# ─────────────────────────────────────────────────────────────

GET {{base_url}}/mails
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?mail$"


GET {{base_url}}/mailbox
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?mail$"


GET {{base_url}}/calendar
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?calendars$"


GET {{base_url}}/contact
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?contacts$"


GET {{base_url}}/file
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?files$"


GET {{base_url}}/photo
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?photos$"


GET {{base_url}}/group
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?groups$"


GET {{base_url}}/favorite
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?favorites$"


GET {{base_url}}/inbox
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?mail$"


GET {{base_url}}/me
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?profile$"


GET {{base_url}}/drive
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?files$"


# ─────────────────────────────────────────────────────────────
# 2. Trailing slash is tolerated — `/mails/` redirects the same.
#
# The middleware trims a trailing slash before matching so that a
# browser's auto-slash on an empty-last-segment URL doesn't miss the
# alias and fall through to the SPA shell as if nothing matched.
# ─────────────────────────────────────────────────────────────

GET {{base_url}}/mails/
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?mail$"


# ─────────────────────────────────────────────────────────────
# 3. Query string is preserved verbatim.
#
# A share link or a utm-tagged typo shouldn't lose its query on the
# redirect. Pin the shape so a future refactor that rebuilds the URL
# can't quietly drop it.
# ─────────────────────────────────────────────────────────────

GET {{base_url}}/mails?utm_source=example&from=inbox
[Options]
location: false
HTTP 301
[Asserts]
header "Location" matches "^(.*/)?mail\\?utm_source=example&from=inbox$"


# ─────────────────────────────────────────────────────────────
# 4. Unmatched paths fall through — the middleware is path-exact and
#    does NOT fuzzy-match.
#
# `/mailto` shares a prefix with `/mail` but is a distinct string, so
# the middleware ignores it and the request falls through to the SPA
# shell fallback (which serves the shell HTML for any unmatched client
# route — not a 301, not a 404). The asserted `Content-Type` is the
# only reliable signal that the middleware did NOT intercept.
# ─────────────────────────────────────────────────────────────

GET {{base_url}}/mailto
[Options]
location: false
HTTP *
[Asserts]
status != 301
