# =============================================================
# OxiCloud — X-OC-Mtime lands in content_modified_at, and stays
#            there across metadata mutations
# =============================================================
# Covers plan §1's trust bounds and §2's repoint
# (`docs/plan/normalize-file-dates.md`). `X-OC-Mtime` is a client
# asserting the source file's POSIX mtime. It is accepted ONLY on the
# two NextCloud surfaces:
#
#   nextcloud/webdav_handler.rs::handle_put   — plain PUT
#   nextcloud/uploads_handler.rs::handle_assemble — chunked assemble
#
# Neither the REST upload endpoint nor the native `/webdav/` surface
# accepts it, so this file stays on `/remote.php/dav/`.
#
# What is actually being pinned here, and why it is not covered
# elsewhere: the existing DAV date tests
# (`nc_multidrive_move_regression.hurl`, `webdav_patch_consistency.hurl`)
# assert the *shape* of `D:getlastmodified` — that it is an RFC 1123
# HTTP-date rather than RFC 3339. None of them assert its *value*. So a
# regression that emitted a perfectly-shaped but wrong instant — the
# upload moment instead of the client's mtime, or the rename moment
# instead of the content moment — passes the whole existing suite.
#
# Five parts:
#
#   A. PUT with X-OC-Mtime → getlastmodified is that exact instant, and
#      creationdate is NOT (the two clocks must be distinguishable).
#      A3 also pins `nc:upload_time` to RECEIPT rather than content (§0).
#   B. Rename the file → getlastmodified UNCHANGED. This is the single
#      regression the dates plan exists to fix: before §2, a rename
#      bumped `updated_at`, which every mtime read resolved to, so
#      renaming a file told every sync client its content had changed.
#   C. The floor instant (2000-01-01T00:00:00Z) is ACCEPTED. One second
#      earlier is rejected. This pins the app gate and the DB CHECK to
#      the same comparison — they were `<` and `>` at one point, so the
#      single second at exactly the floor passed validation and then
#      violated the constraint, turning a 400 into a 500.
#   D. Implausible values are refused with 400, never clamped. A wrong
#      client clock must be visible to whoever can fix it.
#   E. The CHUNKED assemble surface honours it too — the path a real NC
#      desktop client uses for anything large, and the one this header
#      claimed to cover while only Part A tested it.
#
# **Ordering is load-bearing.** Part B renames the probe file, so every
# step after it must use `nc-mtime-renamed.txt`. A new assertion about
# the uploaded file belongs in Part A, before that MOVE — adding one
# after it 404s. Part E sidesteps this by using its own fresh path.
#
# Depends on migration 20261103000006 (adds content_modified_at).
#
# Self-contained: mints its own app password and deletes every probe
# file plus the app password in teardown, so it can run alongside the
# rest of the suite.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT login, then mint an NC app password (NC DAV uses
#         Basic Auth, not the JWT bearer token).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_webdav_client_mtime" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ═════════════════════════════════════════════════════════════
# Part A — PUT with X-OC-Mtime: the asserted instant is what
#          D:getlastmodified reports
# ═════════════════════════════════════════════════════════════
# 1700086400 = 2023-11-15T22:13:20Z. Chosen to be unambiguously in
# the past (so it cannot collide with the upload moment) and to have a
# two-digit day, so the RFC 1123 rendering has no zero-padding
# ambiguity.
PUT {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-probe.txt
Content-Type: text/plain
X-OC-Mtime: 1700086400
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
content with a client-asserted mtime
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# A2 — PROPFIND: getlastmodified is the asserted mtime exactly.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-probe.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
    <D:creationdate/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# THE assertion of this file. 1700086400 → Wed, 15 Nov 2023 22:13:20Z.
# The suffix alternation tolerates the ` GMT` / ` +0000` convention
# (both are accepted by every modern sync client, and the two DAV
# surfaces have historically differed); the instant itself is pinned.
xpath "string(//*[local-name()='getlastmodified'])" matches "^Wed, 15 Nov 2023 22:13:20 (GMT|\\+0000)$"
# `creationdate` is `created_at` — the upload moment, i.e. now — so it
# must NOT be the 2023 instant. If this ever matches, the two clocks
# have been collapsed into one column again and Part B below would
# silently stop testing anything.
xpath "string(//*[local-name()='creationdate'])" not matches "^2023-11-15T22:13:20"


# ═════════════════════════════════════════════════════════════
# Part A3 — `nc:upload_time` reports RECEIPT, not content (§0)
# ═════════════════════════════════════════════════════════════
# This file is the ideal fixture for it: its content clock is pinned to
# 2023 by the X-OC-Mtime above, while it was actually uploaded moments
# ago. The two are three years apart, so the assertion needs no timing
# care.
#
# §0 repointed this slot from `modified_at` to `created_at`. Before
# that it echoed the content clock, so a file edited years after upload
# claimed to have been uploaded years after it was — here it would have
# reported the 2023 instant.
#
# `nc:creation_time` is deliberately NOT asserted: it still reads
# `created_at` and is still wrong (it should be
# COALESCE(captured_at, content_modified_at)), because `captured_at` is
# not available at that emission site. Asserting it now would pin the
# wrong behaviour. See §0 in docs/plan/normalize-file-dates.md.
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-probe.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <D:prop>
    <nc:upload_time/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# Integer Unix seconds — NC clients parse with `(int)$value`, so an RFC
# 3339 string silently coerces to 0 and the file reads as never uploaded.
xpath "string(//*[local-name()='upload_time'])" matches "^[0-9]+$"
# NOT the asserted content mtime. 1700086400 is the X-OC-Mtime from
# Part A; receipt happened just now, so this must be some other value.
xpath "string(//*[local-name()='upload_time'])" != "1700086400"


# ═════════════════════════════════════════════════════════════
# Part B — A rename does NOT move the content clock
# ═════════════════════════════════════════════════════════════
# The regression the whole dates plan exists to fix. Before §2, every
# mtime read resolved to `updated_at`, which a rename bumps — so
# renaming a file announced a content change to every sync client and
# triggered a needless re-download.
#
# Hurl gotcha: headers MUST precede section blocks like `[BasicAuth]`,
# or `Destination:` is parsed as a new request's method line.
MOVE {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-probe.txt
Destination: {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-renamed.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-renamed.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# Identical to A2. The rename bumped `updated_at`; `content_modified_at`
# must not have moved.
xpath "string(//*[local-name()='getlastmodified'])" matches "^Wed, 15 Nov 2023 22:13:20 (GMT|\\+0000)$"


# ═════════════════════════════════════════════════════════════
# Part B2 — a CONTENT edit does move the clock
# ═════════════════════════════════════════════════════════════
# Part B proved a rename leaves the content clock alone. That is only
# the right behaviour if a real content change still moves it —
# otherwise a clock that never moves would pass Part B too.
#
# Asserted deterministically rather than against the wall clock: the
# file currently reports the 2023 instant from Part A, so overwriting
# it with NO `X-OC-Mtime` must leave it reporting something else. No
# sleep, no same-second race — the only way this passes is if the
# overwrite actually rewrote the column.
PUT {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-renamed.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
a genuine content change, with no client-asserted mtime
```

HTTP 204


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-renamed.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# No longer the Part A instant — the bytes changed, so mtime moved.
xpath "string(//*[local-name()='getlastmodified'])" not matches "^Wed, 15 Nov 2023 22:13:20"


# ═════════════════════════════════════════════════════════════
# Part C — The floor instant is accepted, one second earlier is not
# ═════════════════════════════════════════════════════════════
# 946684800 = 2000-01-01T00:00:00Z, exactly `CLIENT_MTIME_FLOOR_SECS`.
# `validate_client_mtime` rejects `secs < FLOOR`, so the floor itself is
# valid input — which is why the DB constraint is `>=` and not `>`.
# A `>` constraint let this exact value pass the app gate and then fail
# the insert, producing a 500 where the client deserved either a 201 or
# a clean 400. Pinning the accepted boundary is what catches that class
# of drift; pinning only the rejected side would not.
PUT {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-floor.txt
Content-Type: text/plain
X-OC-Mtime: 946684800
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
at the floor
```

HTTP 201


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-floor.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# 2000-01-01 was a Saturday. `0?1` tolerates either day-padding
# convention — chrono's RFC 2822 output is unpadded, but the two DAV
# surfaces use different emitters and only the instant is load-bearing.
xpath "string(//*[local-name()='getlastmodified'])" matches "^Sat, 0?1 Jan 2000 00:00:00 (GMT|\\+0000)$"


# ═════════════════════════════════════════════════════════════
# Part D — Implausible mtimes are refused, never clamped
# ═════════════════════════════════════════════════════════════
# D1 — 946684799 = 1999-12-31T23:59:59Z, one second below the floor.
# Catches zero, negative and uninitialised clocks, which would
# otherwise sort to the start of every timeline forever.
PUT {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-too-old.txt
Content-Type: text/plain
X-OC-Mtime: 946684799
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
should be refused
```

HTTP 400


# D2 — 4102444800 = 2100-01-01T00:00:00Z, far beyond the one-hour
# future slack. A fixed far-future constant rather than a computed
# `now + 2h`, which Hurl cannot express; the assertion is unaffected by
# when the suite runs. Catches a badly-set client clock, which would
# otherwise pin the file to the top of every newest-first listing
# permanently.
PUT {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-future.txt
Content-Type: text/plain
X-OC-Mtime: 4102444800
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
should be refused
```

HTTP 400


# D3 — A refused PUT must not have created the file. The handler
# validates before the write, so a 400 leaves no trace; a regression
# that validated afterwards would leave a file with a garbage mtime
# AND return 400, which is the worst of both.
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-too-old.txt
Depth: 0
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 404


# D4 — A malformed (non-integer) header is currently IGNORED, not
# refused: the parse is `.and_then(|v| v.parse::<i64>().ok())`, so an
# unparseable value collapses to `None` — indistinguishable from the
# header being absent — and the PUT succeeds with the server clock.
#
# Decided 2026-10-18, and the asymmetry with D1/D2 is deliberate: an
# implausible-but-parseable value is a 400 because the client told us
# something specific and wrong, and clamping it would bake a bad
# timestamp into stored data. An unparseable value carries no
# information at all, so on a NextCloud-compatibility surface the
# lenient reading wins — refusing a whole request over a header we can
# simply disregard would break clients that otherwise work.
#
# The broken client is not invisible, though: `parse_client_mtime`
# emits `<surface>.mtime_ignored` at WARN with the raw value. That log
# line is the half of this decision Hurl cannot assert — if the warn is
# ever dropped this test still passes, so the two belong together.
PUT {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-garbage.txt
Content-Type: text/plain
X-OC-Mtime: not-a-number
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
malformed header is ignored, not refused
```

HTTP 201


# ═════════════════════════════════════════════════════════════
# Part E — the CHUNKED surface honours it too
# ═════════════════════════════════════════════════════════════
# This file's header says `X-OC-Mtime` is accepted on two NC surfaces —
# `webdav_handler::handle_put` and `uploads_handler::handle_assemble` —
# and until now only tested the first. The second is the one a real
# NextCloud desktop client actually uses for anything large, so the
# untested path was the more important of the two.
#
# §1 lists "chunked upload completion" as one of the four content
# paths that must set the clock. It shares `parse_client_mtime` and
# `update_file_streaming_with_perms` with the plain PUT, so this is a
# wiring assertion rather than a second implementation — but wiring is
# exactly what was missing on the CREATE path, and that defect survived
# until a test looked.
#
# Note the REST chunked endpoint (`/api/uploads/{id}/complete`) cannot
# carry a mtime at all: it routes through
# `upload_file_streaming_with_perms`, which takes no such parameter.
# Only the NC assemble path can, which is why this exercises that one.
#
# Zero chunks on purpose: the assemble step writes an empty file, which
# is enough to pin the clock and avoids the protocol noise of a real
# chunk sequence. Mirrors `nc_multidrive_move_regression.hurl`, which
# established that this shape works.
MKCOL {{base_url}}/remote.php/dav/uploads/{{username}}/nc-mtime-chunk-session
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


# Hurl gotcha, same as Part B: headers MUST precede `[BasicAuth]`, or
# `Destination:` is parsed as a new request's method line.
MOVE {{base_url}}/remote.php/dav/uploads/{{username}}/nc-mtime-chunk-session/.file
Destination: {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-chunked.txt
X-OC-Mtime: 1700086400
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-chunked.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# Same instant as Part A, reached through the other surface. A build
# that parsed the header and then dropped it on assemble reports the
# upload moment here and passes every other assertion in this file.
xpath "string(//*[local-name()='getlastmodified'])" matches "^Wed, 15 Nov 2023 22:13:20 (GMT|\\+0000)$"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-chunked.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-renamed.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-floor.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


# D4's probe DID get created (the malformed header was ignored, not
# refused), so unlike D1/D2 it needs cleaning up.
DELETE {{base_url}}/remote.php/dav/files/{{username}}/nc-mtime-garbage.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
