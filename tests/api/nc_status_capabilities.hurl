# =============================================================
# OxiCloud — Baseline: NC status + capabilities
# =============================================================
# Group A from BASELINE_TESTS_NC_WEBDAV.md (4 scenarios).
#
# These four endpoints are what every NC client probes first.
# If any of them returns the wrong shape, NC desktop refuses to
# even attempt sync, so the regression signal is loud and early.
#
# Anonymous (no auth) — these endpoints must be public.
# =============================================================


# ─────────────────────────────────────────────────────────────
# A1 — GET /status.php
#      Used by NC client for "is this server installed?".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/status.php

HTTP 200
[Asserts]
jsonpath "$.installed"     == true
jsonpath "$.maintenance"   == false
jsonpath "$.version"       exists
jsonpath "$.versionstring" exists
jsonpath "$.productname"   == "OxiCloud"


# ─────────────────────────────────────────────────────────────
# A2 — GET /index.php/204
#      NC mobile connectivity probe. Must be 204, empty body.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/index.php/204

HTTP 204
[Asserts]
bytes count == 0


# ─────────────────────────────────────────────────────────────
# A3 — GET /ocs/v1.php/cloud/capabilities
#      OCS v1 envelope must report statuscode 100.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ocs/v1.php/cloud/capabilities?format=json

HTTP 200
[Asserts]
jsonpath "$.ocs.meta.status"           == "ok"
jsonpath "$.ocs.meta.statuscode"       == 100
jsonpath "$.ocs.meta.message"          == "OK"
jsonpath "$.ocs.data.version.major"    exists
jsonpath "$.ocs.data.capabilities"     exists
jsonpath "$.ocs.data.capabilities.theming.name" == "OxiCloud"
# F5 (docs/plan/nc-sync-client-findings.md): the NC client sizes its chunks
# from this value; without it a lowered OXICLOUD_CHUNK_MAX_BYTES /
# OXICLOUD_DIRECT_PUT_MAX_BYTES turned every desktop upload into a 413.
# Test env sets both caps to 4 MiB, so the smaller of them is 4194304.
jsonpath "$.ocs.data.capabilities.files.chunked_upload.max_size" == 4194304


# ─────────────────────────────────────────────────────────────
# A4 — GET /ocs/v2.php/cloud/capabilities
#      OCS v2 envelope must report statuscode 200 (200 ≡ 100
#      semantically — different status enum across versions).
#      Payload shape otherwise identical to v1.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ocs/v2.php/cloud/capabilities?format=json

HTTP 200
[Asserts]
jsonpath "$.ocs.meta.status"           == "ok"
jsonpath "$.ocs.meta.statuscode"       == 200
jsonpath "$.ocs.meta.message"          == "OK"
jsonpath "$.ocs.data.version.major"    exists
jsonpath "$.ocs.data.capabilities"     exists
jsonpath "$.ocs.data.capabilities.theming.name" == "OxiCloud"
# F5 (docs/plan/nc-sync-client-findings.md): the NC client sizes its chunks
# from this value; without it a lowered OXICLOUD_CHUNK_MAX_BYTES /
# OXICLOUD_DIRECT_PUT_MAX_BYTES turned every desktop upload into a 413.
# Test env sets both caps to 4 MiB, so the smaller of them is 4194304.
jsonpath "$.ocs.data.capabilities.files.chunked_upload.max_size" == 4194304
