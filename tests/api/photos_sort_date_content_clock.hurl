# =============================================================
# OxiCloud — the Photos timeline sorts on the CONTENT clock
# =============================================================
# Covers plan §3, which shipped without a test of its own.
#
# `storage.files.media_sort_date` is the materialised Photos sort key.
# Before §3 it was `COALESCE(captured_at, created_at)` — so an image
# with no EXIF date sorted by its UPLOAD time, permanently, however
# often its content changed. §3 repointed the fallback at
# `content_modified_at`:
#
#     media_sort_date = COALESCE(captured_at, content_modified_at)
#
# That backfill moved 4,665 of 4,916 rows on the dev instance, i.e. ~95%
# of the timeline reordered, and nothing asserted the new semantics.
#
# ── How the assertion distinguishes the two implementations ───────────
# The file is uploaded through the NextCloud surface with
# `X-OC-Mtime: 1700086400` (2023-11-15T22:13:20Z), which is the only
# way to set a content clock to a known PAST instant — the REST upload
# path has no mtime slot yet (plan §9).
#
#   after §3  → sort_date == 1700086400      (the content clock)
#   before §3 → sort_date ≈ now (~1.79e9)    (created_at, the upload)
#
# Three decades apart, so there is no timing sensitivity at all.
#
# ── Two deliberate choices in the fixture ────────────────────────────
# **A unique body, not a shared fixture file.** The media listing is
# `DISTINCT ON (blob_hash) … ORDER BY blob_hash, media_sort_date DESC`
# (see `20261103000004_files_media_dedup_by_drive_blob_index.sql`), so
# one row survives per blob per drive — the one with the NEWEST sort
# date. Re-using `fixtures/blue-image.png` would risk another test's
# copy of that blob, stamped "now", masking this row entirely and
# failing the assertion for a reason that has nothing to do with §3.
#
# **A `.png` name over a text body.** MIME detection is magic-bytes
# first then extension fallback (`common/mime_detect.rs`), so the
# content need not be a real PNG for the row to be stored as
# `image/png` and therefore to qualify for the timeline. Thumbnailing
# will fail on it asynchronously, which this test does not care about.
#
# Self-contained: deletes the probe and the app password in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT, plus an NC app password (X-OC-Mtime is NC-only).
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
{ "label": "photos_sort_date_content_clock" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Upload with a content clock three decades in the past.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/hurl-mediaclock-probe.png
Content-Type: image/png
X-OC-Mtime: 1700086400
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
hurl-mediaclock-probe unique body so this blob is its own: 7f3a9c21-mediaclock
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# THE assertion — the timeline reports the content clock.
#
# `order_by=captured_at` is the axis resolving to `media_sort_date`.
# `reverse=true` makes it OLDEST-first, which puts this file — dated
# 2023 while every other test's media is stamped "now" — at index 0.
#
# Positional access is deliberate: Hurl collapses a single-match
# `[?(...)]` filter to a bare scalar, and `nth`/`count` then error with
# `invalid filter input type`. Filters are unusable here.
#
# Asserting the NAME at index 0 alongside the date is what keeps the
# positional assumption honest: if some other old image ever sorts
# first, this fails on the name with an obvious message instead of
# silently checking the wrong row.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?limit=50&order_by=captured_at&reverse=true
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items[0].resource.name" == "hurl-mediaclock-probe.png"
# 1700086400 = 2023-11-15T22:13:20Z, the asserted X-OC-Mtime. A
# pre-§3 build reports the upload moment here instead (~1.79e9).
jsonpath "$.items[0].sort_date" == 1700086400
# Which COALESCE arm won, pinned explicitly. `captured_at` is
# `Option<i64>` with `skip_serializing_if = none`, so its ABSENCE is
# what proves the line above exercised the `content_modified_at`
# fallback rather than an EXIF date that merely happened to match. A
# fixture that ever carried real EXIF would fail here, loudly.
jsonpath "$.items[0].captured_at" not exists


# ─────────────────────────────────────────────────────────────
# A metadata mutation must not move the timeline position.
# Same rule as the file content clock: a rename is not a content
# change, so `content_modified_at` holds and `media_sort_date`
# with it.
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/remote.php/dav/files/{{username}}/hurl-mediaclock-probe.png
Destination: {{base_url}}/remote.php/dav/files/{{username}}/hurl-mediaclock-renamed.png
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


GET {{base_url}}/api/photos/resources?limit=50&order_by=captured_at&reverse=true
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.name" == "hurl-mediaclock-renamed.png"
jsonpath "$.items[0].sort_date" == 1700086400


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/remote.php/dav/files/{{username}}/hurl-mediaclock-renamed.png
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
