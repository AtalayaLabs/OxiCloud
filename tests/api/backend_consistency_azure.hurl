# =============================================================
# OxiCloud – backend_consistency against the Azure backend
#
# Runs against **Azurite**, the Azure Blob emulator started by
# `tests/common/spawn-db.sh`. It speaks the real Blob REST API, so this
# is the only way to exercise the Azure path without an account. The
# `azurite` storage entry is declared in `server.env` but never
# activated — the suite's active backend stays local, and this file
# reaches Azure explicitly through `?storage=azurite`.
#
# ## What it pins
#
# That Azure enumeration works against a real Blob REST implementation —
# SharedKey signing, prefix/marker paging, the 256-way shard walk and its
# termination. `AzureBlobBackend::list_blob_hashes` has unit tests for
# its name parser and ordering, but nothing else in the tree speaks the
# protocol.
#
# A failure surfaces as `ok: false`, because an enumeration error now
# fails the run. It used to degrade to a per-row probe — walk
# `storage.blobs`, ask "are these bytes there" — which found only the
# DB→backend direction and left orphans undetectable, since bytes no row
# claims are invisible to anything starting from the database. That
# fallback is gone; the module docs on `backend_consistency_service.rs`
# say why.
#
# ## What it deliberately does NOT assert
#
# Any finding count. The Azurite container starts empty and the run's
# grace window is one hour, so a freshly-uploaded blob is skipped in
# both directions by design — an audit here can only ever report zero.
# Asserting "zero findings" would pass whether enumeration worked or
# returned nothing at all.
#
# Real orphan/missing coverage needs blobs on the backend older than the
# grace window, which needs either a cutover into Azurite or a way to
# backdate `last_modified`. See "Why there is no cutover here" below.
#
# Prerequisites: setup.hurl must have run (admin user exists), and
# Azurite must be listening on 10000.
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/backend_consistency_azure.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login
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
# Step 2 – Upload a file so the registry is not empty.
#
# The bytes land on the ACTIVE (local) backend and never reach Azurite,
# so this does not feed step 3. It feeds step 4: a local sweep over an
# empty `storage.blobs` would satisfy every assertion there while
# comparing nothing, and the control is only a control if it had
# something to compare.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-azure-consistency"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 2b – Pre-flight: does the Azure backend work at all?
#
# Cheap, synchronous, and it isolates the failure. It separates "Azurite
# is missing, wedged, or misconfigured" from "enumeration is broken",
# which step 3 alone cannot: a dead container and a broken
# `list_blob_hashes` both surface there as the per-row fallback.
#
# `entry_name` resolves against OXICLOUD_STORAGE_ENTRIES, so this
# exercises the same entry step 3 audits rather than an ad-hoc config.
# It does a health check AND a write/read round-trip, and
# `phase_reached` names how far it got, so a failure points at a step
# rather than at the suite.
#
# Commonest cause of a failure here: the container does not exist.
# `AzureBlobBackend::initialize` verifies rather than creates, so
# `spawn-db.sh` provisions it with a hand-signed PUT.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/settings/storage/test
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "entry_name": "azurite"
}

HTTP 200
[Asserts]
jsonpath "$.connected" == true
jsonpath "$.roundtrip_passed" == true
jsonpath "$.bytes_written" != 0
jsonpath "$.bytes_read" != 0


# ─────────────────────────────────────────────────────────────
# Step 3 – Audit the Azure entry.
#
# `?storage=azurite` builds a backend for that named entry directly,
# bypassing the active-backend pointer — which is also the answer for
# auditing either side mid-migration.
#
# ## Why there is no cutover here — DECIDED, do not retry casually
#
# An earlier version ran `backend_migration ?storage=azurite` first, to
# put real bytes in the container. It hangs, on the first blob, and the
# cause is in `azure_core` 0.21 rather than in anything OxiCloud does.
#
# The chain, all verified in source:
#
#   backend_migration_service.rs      target.head_check(hash)
#     → EncryptedBlobBackend::head_check
#     → get_blob_range_stream(hash, 0, HEADER_SIZE)   ~40 bytes
#     → azure_core Range::as_headers  adds x-ms-range-get-content-crc64
#                                     to ANY range under 4 MiB
#     → Azurite answers 500
#     → azure_core classifies 500 retryable, response is deterministic,
#       so it retries forever — while migration_readonly refuses writes
#       application-wide.
#
# `head_check` is a pre-write format probe on the TARGET, so it fires
# before the first byte is copied. Nothing about the migration job is
# wrong; it works against real S3.
#
# **A workaround exists and was rejected** (2026-09-02): issue an
# unranged `get()` for small requests — its 16 MiB initial range clears
# the 4 MiB threshold, so the header is never sent — and truncate
# client-side. Correct against real Azure, but it pays for an emulator
# with production cost (a 40-byte probe becomes a whole-blob transfer)
# and puts new offset arithmetic on the read path, where a mistake
# serves wrong bytes silently instead of failing. See the note on
# `AzureBlobBackend::get_blob_range_stream`.
#
# So the cutover comes back with the official `azure_storage_blob` 1.x,
# where `range_get_content_crc64` is an explicit field to leave unset —
# and with it the orphan/missing assertions this file cannot make today.
# `docs/plan/jobs-handling-recoverable-error.md` covers the other half:
# the run should have paused with a reason instead of hanging, whatever
# the SDK does.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_consistency/trigger?storage=azurite
Authorization: Bearer {{token}}

HTTP 200
[Asserts]

# THIS is the assertion that matters, and it is a real one. Enumeration
# failure is no longer degraded into a completed run — any Err from
# `list_blob_hashes` now fails the run — so a broken Azure enumeration
# surfaces right here as `ok: false`, whatever went wrong: signing,
# paging, the shard walk, the cursor.
#
# Before the fallback was deleted this needed a proxy assert on
# `extra_stats.mode`, because a broken enumeration completed "cleanly"
# with half its coverage silently gone.
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"

# No unexpected notices. `backend_unenumerable` is gone with the
# fallback, so what this now guards against is a stray
# `unknown_backend_file` — a non-canonical name in the blob namespace,
# which on a container we provision ourselves means something wrote
# where it should not have.
jsonpath "$.outcome.extra.severity_counts.anomaly" not exists

# ── Known weakness, stated rather than hidden ────────────────────────
#
# A run that enumerates SUCCESSFULLY but returns nothing still passes.
# `scanned_count` would catch that, and it is deliberately not asserted
# here: the container is empty, so the job early-returns before its
# first checkpoint and 0 is the correct answer. Step 4 carries that
# assert instead, on the one entry that does hold blobs.
#
# Making it positive HERE needs the corpus on Azure, which needs the
# cutover — see the header. Planting blobs by hand is not a substitute:
# the DB side would then be walked against a backend that does not hold
# the corpus, and every local blob would report `blob_missing_from_backend`.


# ─────────────────────────────────────────────────────────────
# Step 4 – The same job against the LOCAL entry behaves identically.
#
# Azure and local now take the same code path, so this is no longer a
# contrast — it is the control. If a future change re-degrades Azure,
# this passing while step 3 fails localises the break to the Azure
# backend rather than to the job.
#
# It also carries the one POSITIVE assert this file can make. Local
# holds the blob step 2 uploaded, so the sweep gets past the empty-page
# early return and checkpoints — which is the only thing here that
# distinguishes "the merge-join compared something" from "the merge-join
# was handed nothing and completed".
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_consistency/trigger?storage=local_main
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"

# `scanned_count` accumulates via `checkpoint`, which the early return
# skips — so a non-zero value means blobs were enumerated AND paired
# against `storage.blobs`, not merely that the run ended cleanly.
jsonpath "$.outcome.extra.scanned_count" != 0


# ─────────────────────────────────────────────────────────────
# Step 5 – Teardown. Hurl files share one database, so a folder left
# behind changes what later scenarios see.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_id: jsonpath "$.items[?(@.resource.id == '{{folder_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_id}}
Authorization: Bearer {{token}}

HTTP 200
