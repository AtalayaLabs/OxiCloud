# Plan — Failures that say what they are, and satellite lifecycles that cannot be skipped

**Status:** design captured 2026-10-01; **Part A phases 1–3 implemented
2026-10-02; phase 4 (surfacing), phase 5 (scheduling), phase 6 webhook
and email alerting 2026-10-03.** The detect → classify → run → surface →
act chain is closed for both out-of-band transports, and extended with a
second alert class the plan did not anticipate: a run that *stopped*
(`error_reason` set) alerts too, verified against a live backend outage.
**In-app is parked** — low priority, see § The shape. Part B's "needs a
schedule" item is already satisfied by phase 5.
**Part C is complete (2026-10-03/04)** — all three items. **Part B's
interim arm is done**; its strategic half (the FK + decrement trigger)
waits on fleet-wide CDC convergence, which is a release judgement rather
than code.
Parts B (satellites) and C (compensations) are untouched.
Follow-up to
`storage-consistency.md` (merged as PR #771), which fixed the *recording* half of
that plan's invariant and left the *discovery* half resting on detectors nothing
runs.

Three threads, deliberately in one plan because they share a root: **a correctness
property that depends on a code path firing at the right moment is not a
property.** The deletion queue turned "the unlink happened, probably" into a
durable intent. The same reasoning applies to how failures are classified and to
how satellite rows release their blobs.

Prior art to read first rather than re-derive: `derived-blobs.md` owns the
satellite *design* (keying rule, copy semantics, the two-table split — note its
status line is stale, the tables shipped), and
`jobs-handling-recoverable-error.md` owns the retry-then-pause engine this plan
leans on.

---

# Part A — Failure classification

## The incident that motivates it

`backend_consistency?deep=true` recorded `blob_unreadable` at severity
**`data_loss`** against a perfectly intact object. OVH was briefly degraded,
`StalledStreamProtectionConfig` aborted the slow read, and a re-run passed
cleanly. Nothing was wrong with the data.

That is not a cosmetic mislabel. `data_loss` is the severity an operator is
supposed to drop everything for, and a deep scan reads *every* object — so on a
large enough bucket the job reliably manufactures alarming findings about healthy
storage. A detector that cries wolf is worse than no detector, because it trains
the operator to ignore the one time it is right.

## Three compounding defects

1. **Mid-stream failures are never retried, and structurally cannot be by the
   current layer.** `RetryBlobBackend::get_blob_stream` retries *opening* the
   stream; once it returns `Ok(BlobStream)` the decorator is done. The failure
   happens while consuming the body (`encrypted_blob_backend.rs:1019`), outside
   every decorator. The AWS SDK cannot replay a body it already handed over
   either, and `stalled_stream_protection` actively *aborts* slow reads — so a
   degraded backend produces a hard error rather than a slow success.

2. **Transience is destroyed at the wrap.** That site builds
   `DomainError::internal_error(...)`, and `is_transient()` matches only
   `Timeout | TransientBackend`. So every streaming failure reads as **permanent**
   to every caller — defeating the classification that `backend_rechunk`,
   `backend_reclaim` and the consistency jobs all now depend on. They reason
   correctly about a value that is already wrong.

3. **The error chain is discarded.** The finding records `e.to_string()`, losing
   `DomainError::source`. The HTTP status, the SDK error code, and whether this was
   a stall abort or a connection reset are gone before anything is written down —
   which is why the incident produced `"stream read: streaming error"` and no way
   to tell what actually happened.

## The phases, in dependency order

### Phase 1 — Retry the whole read in the compute paths

**First, because it is the only phase that does not depend on solving another
one.** A successful re-read proves the object is intact without anyone
determining *why* the first attempt failed. It sidesteps Phase 2 entirely.

Targets: `verify_bytes` (deep mode), `backend_rechunk`, `backend_rotate` — the
paths that read in order to *compute*. Re-open, re-read, conclude only on the
second failure.

Explicitly NOT the download path. Past the first byte the client holds partial
data, so a transparent retry is impossible; HTTP Range plus client retry is the
answer there, and `get_blob_stream`'s existing decorator already covers failures
before the stream opens.

It also settles the one case static classification cannot: `UnexpectedEof` is
ambiguous between a truncated object and a dropped connection, but a truncated
object fails *identically every time at the same byte count*. One re-read converts
a judgement call into an observation.

Cost is negligible — deep mode already reads every object; this is one extra read
on the rare failure.

### Phase 2 — Classify at the source, not at the consumer

`BlobStream` is `Stream<Item = Result<Bytes, std::io::Error>>`, so the raw
`io::Error` does reach the wrap site and `kind()` is available today. But whether
it is *meaningful* differs by backend:

* **Local** — `tokio::fs` yields real kinds (`NotFound`, `PermissionDenied`,
  `UnexpectedEof`). Consumer-side classification works as-is.
* **S3** — `s3_blob_backend.rs` hands out `output.body.into_async_read()`, and that
  adapter flattens SDK errors into an `io::Error` that is very likely
  `ErrorKind::Other`. The discriminating detail survives only in `.get_ref()`, and
  only reachable by downcasting to the smithy byte-stream error type.

So the classification belongs **in the S3 backend**, which already has
`s3_domain_error` as its classifier for operation errors and simply does not apply
it to the body stream. Map the stream's errors there and every layer above gets a
meaningful `ErrorKind` uniformly.

**Confirm empirically before building this.** Log `e.kind()` alongside the message
at the wrap site and trigger one failure. If OVH already yields
`ConnectionAborted` or `TimedOut`, the cheap consumer-side fix suffices and the S3
work is unnecessary.

Mapping, once the kind is trustworthy:

| `io::ErrorKind` | classify as |
|---|---|
| `ConnectionReset`, `ConnectionAborted`, `BrokenPipe`, `TimedOut`, `Interrupted` | `transient_backend` |
| `UnexpectedEof` | ambiguous — resolved by Phase 1's re-read, not by the kind |
| everything else | keep `internal_error` |

### Phase 3 — Retry, then pause, in the `*_consistency` family

Retry alone is not enough: a degraded backend fails the next thousand blobs too, so
retry-record-continue walks the whole bucket emitting one `data_loss` finding per
object.

| outcome | action |
|---|---|
| read fails, retry succeeds | continue silently — the object is fine |
| retry fails, error **transient** | **PAUSE** at the cursor (`RunOutcome::from_domain_error`) |
| retry fails, error **permanent** | record the finding, **CONTINUE** |
| read succeeds, hash mismatches | record `blob_corrupted`, continue |

**Transient pauses; permanent does not** — a transient failure predicts the rest of
the scan, a permanent one concerns exactly one object, and stopping would let one
corrupt blob hide the bucket.

Two easy mistakes:

* **Pause BEFORE writing the finding**, or the run leaves a `data_loss` finding
  that outlives the outage and nothing retracts it.
* **`blob_unreadable` must stop meaning "possibly lost".** Reserve it for decrypt
  and permission failures; the transient case becomes the pause, which already says
  "backend unreachable" — true and actionable.

Same shape `backend_rechunk` ships: transient pauses at the cursor without consuming
the attempt budget, permanent records and continues, consecutive-failure cap as the
systemic backstop.

### Phase 4 — Schedule the detectors — and it MUST be last

Every consistency job is registered on-demand with `interval = None`:
`blobs_consistency`, `manifests_consistency`, `satellites_consistency`,
`files_consistency`, `folders_consistency`, `drives_consistency`,
`drive_policies_consistency`, `backend_consistency`. The only scheduled recoverable
jobs are `backend_reclaim` and `backend_cache_cleanup`, both from PR #771.

This matters because **"a sweep will find it" is the justification the codebase
leans on everywhere** — including `storage-consistency.md`'s own invariant — and it
is currently unfunded. It is also exactly how 29 orphaned blobs accumulated:
`dedup_gc` was `None, // on-demand`, nobody clicked, and bytes piled up for months.

**But scheduling comes last, not first.** Automating a job that converts a
provider's bad ten minutes into `data_loss` findings would industrialise the false
alarms — the operator would arrive to a drawer full of them and learn to dismiss
the lot. Phases 1-3 have to land before this one is a kindness rather than a
nuisance.

When it does: `blobs_consistency` and `satellites_consistency` are cheap (DB-only)
and can run often. `backend_consistency` walks the bucket and costs real money on
S3 — weekly with `deep=false` is the defensible default, with deep runs left to an
operator who has decided to pay for them.

**What shipped, and the two things review changed.**

The unit scheduled is **`consistency_batch` weekly**, not each detector. The batch
dispatches its children sequentially — deliberately, so the sweeps do not multiply
pressure on the shared maintenance pool — and it discovers them by name, so a
detector added later joins the sweep the day it is registered. Listing detectors
individually would leave each new one silently unscheduled, which is the exact
failure this phase exists to remove. Weekly rather than daily because the batch
includes the bucket walk; `deep` and `repair` both default to `false`, so a
scheduled run reads metadata and reports.

Cadence is configuration, not code: `OXICLOUD_SCHEDULED_JOBS`, sibling of
`OXICLOUD_STARTUP_JOBS` (that one runs a job once at boot, this one gives it a
cadence). `off` un-schedules without disabling. Issue #774 — which asks for the
two DB-only detectors on a schedule and explicitly wants the bucket walk kept out
— is therefore one line:
`consistency_batch=off,blobs_consistency=24h,satellites_consistency=24h`.

Two refusals are load-bearing rather than tidy:

* **`repair` cannot be scheduled.** A tick that repairs deletes on a cadence with
  nobody consenting after the first time. `OXICLOUD_STARTUP_JOBS` already is the
  deliberate-operator-act spelling, and it is a one-shot.
* **A duplicate job name is fatal.** `backend_consistency=24h,backend_consistency=720h?deep=true`
  reads as "shallow daily plus deep monthly", but a job has one run row
  (`one_active_run_per_job`) and one cursor — the deep run's pause cursor would be
  picked up by the next shallow tick. Two cadences need two registered jobs.

**The resume hole this opened, and the guard.** Scheduling made a latent hazard
reachable: `open_or_start` resumes a `Paused` run, and `persist_or_restore_args`
restores the args it started with, *ignoring the resuming caller*. So a tick would
have continued an operator's `?repair=true` run — deleting, unattended, on the
strength of a consent given once. The invariant in `config.rs` ("scheduled ticks
deliberately never pass `repair`") was true about what the tick *passes* and false
about what the resumed run *does*.

Fixed by stamping `params.unattended_resume` at fresh-run start and declining the
resume when an unattended caller meets a run that does not permit it. Three
details decide whether it is correct:

* **The default keys on the `repair` consent, not on `Mutates`.** Mutation is the
  wrong signal — `backend_reclaim` and `backend_rechunk` are both `Mutates::Always`
  and both *must* continue unattended; blocking the drain is how 29 orphans
  accumulated. Neither declares `repair`, so both are allowed with no code of
  their own.
* **The flag is a permission, and absent reads as `false`.** A run paused by an
  older release waits for a human. Naming it as a prohibition would have inverted
  that and auto-resumed every pre-existing paused repair run on upgrade.
* **It is stored, not declared**, so `from_declared` rejects it from the wire and
  a caller cannot grant itself the consent.

A human clicking Resume still works — that click *is* the consent — and a decline
is audited, because a paused run nothing picks up otherwise looks exactly like a
job that is not due.

---

# Part B — Satellite lifecycles

## What a satellite is, and why there are two kinds

A satellite is a blob that belongs to *something else* rather than to a file of its
own: a thumbnail, a transcode, an uploaded preview, a subtitle track. Both tables
point at `storage.blobs` content; neither is reachable as a file.

The two differ by **provenance**, and the keying follows from it rather than from
convenience:

| | `content_derived_blobs` | `file_attached_blobs` |
|---|---|---|
| contents | thumbnail, transcode | preview, subtitle, cover_art |
| origin | **server-computed** from content | **user-supplied** |
| keyed by | `(source_hash, kind, variant)` | `(file_id, kind, variant)` |
| dedups across files | yes, inherently | no |
| survives a file copy | free — same content, same key | needs an explicit copy |
| if lost | **recomputable** | **irreplaceable** |

That asymmetry is correct and should not be "unified". A derived artifact is a pure
function of its source, so content-keying makes it shared, free to copy and safe to
delete. An uploaded preview is not derivable from anything, so it must be bound to
the file that owns it and copied explicitly — which is why
`storage.copy_file_satellites` exists and why `attached_thumbnail_copy.hurl` guards
it.

## Creation: the same provenance split decides the failure strategy

Provenance does not only determine the keying — it determines what to do when
creation fails, and the two cases want opposite answers. Neither needs the deletion
queue, and the reasons differ.

### User-uploaded (attached): fail the request

Identical to a classic upload, and for the identical reason from
`storage-consistency.md` §2e: **the client is still on the wire and still holds the
payload.** A 5xx puts the retry where the bytes are, which is strictly better than
any server-side queue.

Queueing it would mean durably holding the payload until the backend returns — and
the only durable store for a payload is the backend that is down. That is a
write-behind cache, i.e. the parked `register_file_deferred` path, acknowledging a
write that is durable nowhere it claims.

The partial-state case is already handled: `store_derived_blob`/`store_attached_blob`
write bytes first and release the reference when the row cannot be written, so a
failed insert does not strand a reference.

### Server-derived: discard, and let the next fetch re-trigger it

Not the queue either — and the reason is exactly the admission test the deletion
queue imposes on itself: **can the intent be re-derived from current state?**

For a deletion it cannot. Once the row is gone, nothing knows the hash, which is why
the intent must be recorded durably. For a *generation* it can: the file exists, its
content hash is known, and the renderer is deterministic. **The file row IS the
standing request to have a thumbnail.** Writing a queue entry would record something
already implied by data we are keeping anyway.

Lazy regeneration is also better than eager retry on its own merits:

* **Self-limiting.** Artifacts nobody requests are never generated. A generation
  queue would dutifully re-render thumbnails for files no one will open.
* **Naturally prioritised.** The next fetch is, by definition, the moment the
  artifact is actually wanted.
* **Free of its own failure modes.** No backlog to monitor, park or drain.

So on failure: release the reference, write no mapping, log, and return. The next
request regenerates. Which is what the code already does — this plan records *why*
it is right, so nobody later "fixes" it by adding a queue.

### The wrinkle: a deterministic failure retries forever

A permanently-undecodable source (corrupt JPEG, unsupported codec) is re-attempted
on every fetch, forever. The answer already exists for transcodes and should be the
rule for both: a **negative entry** recording "this cannot be derived" — the
`.skip` markers, which `transcode_import` turns into negative rows.

Creation policy:

| | attached (user-supplied) | derived (server-computed) |
|---|---|---|
| who is waiting | the client, synchronously | nobody |
| transient failure | **fail the request** — client retries, it holds the payload | **discard**; the next fetch regenerates |
| permanent failure | fail the request; the user sees why | **record a negative entry** so it is not retried forever |
| partial state | release the reference, write no row | release the reference, write no row |
| needs the queue? | no — the intent IS the payload | no — the intent is re-derivable from the file |

**Verified gap: the thumbnail negative verdict is not durable.** Both services have
the concept, but at different lifetimes:

| | negative verdict stored where | survives restart |
|---|---|---|
| transcode | "memory sentinel **+ disk marker**" (`image_transcode_service.rs:13`), imported into negative rows | **yes** |
| thumbnail | empty `Bytes` as moka's zero-weight sentinel (`thumbnail_service.rs:650`) | **no** |

So the verdict is lost on restart or on cache eviction (moka is size-bounded), and
nothing counts failed renders — the cost shows up as CPU with no attribution.

Fix: persist it as a negative row in `content_derived_blobs`, keyed
`(source_hash, 'thumbnail', variant)`. Content-keyed, so one undecodable image is
diagnosed once for every file sharing its bytes. Reuse whatever sentinel the
transcode path already uses for "derived, deliberately absent" — `blob_hash` is
`NOT NULL`, so one encoding exists; do not invent a second.

## The real problem on the reclamation side: one is structural, the other is not

| | releases its blob reference via |
|---|---|
| `file_attached_blobs` | **the database** — `file_id REFERENCES storage.files ON DELETE CASCADE`, plus `trg_file_attached_blobs_decrement_blob_ref AFTER DELETE` |
| `content_derived_blobs` | **application code** — `purge_derived_blobs`, which only runs when the source is reaped |

The attached side cannot leak: deleting the file cascades to the row, and the
trigger decrements the reference. No code path has to remember.

The derived side has no FK and no trigger, so its correctness depends on
`purge_derived_blobs` being reached at the right moment — and there is a documented
window where it is not. `store_derived_blob`'s own comment says it plainly: a
mapping written *after* its source is reaped is unreachable forever, because
nothing will ever reap that hash a second time, and the orphaned row holds its
derived blob at `ref_count = 1` which GC is then correct to refuse. Permanent leak,
three rows per image. Not hypothetical — thumbnail generation is spawned and
unawaited, so an upload deleted promptly has its render finish after the reap.

The guard in that INSERT (`WHERE EXISTS (manifest OR blob)`) closes the common case
atomically, but it cannot close the reverse order: insert commits, source is reaped
immediately after, purge has already run.

## The strategic fix, and what unlocks it

Give `content_derived_blobs` what the attached table has: an FK plus a decrement
trigger.

The obstacle was that `source_hash` could name either `chunk_manifests.file_hash`
(CDC) or `storage.blobs.hash` (legacy) — two tables, no single FK. **CDC convergence
removes it**: post-convergence every `source_hash` names a manifest, and
`chunk_manifests.file_hash` is a PRIMARY KEY.

```sql
ALTER TABLE storage.content_derived_blobs
  ADD CONSTRAINT fk_derived_source
  FOREIGN KEY (source_hash) REFERENCES storage.chunk_manifests(file_hash)
  ON DELETE CASCADE;

-- plus the mirror of trg_file_attached_blobs_decrement_blob_ref
```

Both failure modes become impossible: the mapping cannot outlive its source, and the
reference is released by the database rather than by a function someone has to
reach. This is §1's payoff beyond deleting legacy read paths.

Prerequisites, in order:

1. `backend_rechunk` reports zero **fleet-wide**, not just on one instance. The
   reporting instance already does (verified 2026-09-28); this is a release-gate
   judgement, not a per-deployment one.
2. A migration that deletes any pre-existing orphan rows, because the FK will
   refuse to be added while they exist. That deletion must release their blob
   references — the same decrement the trigger will do from then on.
3. Only then the FK and trigger.

## Interim, until the FK is possible

`satellites_consistency` currently finds `derived_orphan_mapping`,
`derived_dangling_blob` and `attached_dangling_blob` — and has **no `mutates()`
override, no repair arm and no parameters**. It can name an orphan mapping and do
nothing whatsoever about it, and nothing schedules it.

So it needs the treatment `backend_consistency` got in §2d:

* **A `repair=true` arm that is `Mutates::OnRepairOnly`**, deleting an orphan
  mapping and **releasing its blob reference** — which is the half that matters,
  since the row is only the symptom and the pinned reference is the leak.
* **Route the freed blob through the deletion queue**, not a direct unlink. The
  base-blob paths all do this since PR #771, and a satellite's blob is an ordinary
  blob — this is the same argument as §2d: the drain re-verifies under a row lock,
  so an object that became referenced again is never deleted.
* **A schedule**, after Part A. **Already satisfied** (2026-10-03): phase 5
  schedules `consistency_batch` weekly by default and it fans out to every job
  whose name ends in `_consistency`, so `satellites_consistency` is swept
  without an entry of its own.

A `derived_dangling_blob` (the blob is gone, the mapping remains) wants the
opposite remedy and should be stated separately: the artifact is recomputable, so
the correct repair is to delete the mapping and let it be regenerated on next
request — *not* to report `data_loss`, which is what it does today
(`satellites_consistency_service.rs:324`). That severity is
right for `attached_dangling_blob`, where the bytes were user-supplied and are
genuinely unrecoverable.

Note the severity correction now has a second consequence it did not have when
this was written: out-of-band alerting is gated on severity, and the shipped
default floor is `data_loss`. So today a regenerable thumbnail whose blob went
missing pages an operator exactly as loudly as missing user bytes — which is the
false-alarm end of the same mislabelling, and the fastest way to get a channel
muted.

## One open bug to fold in — **already fixed, verified 2026-10-03**

`bug_attached_blob_same_content_leaks_ref` — storing an attached blob whose content
is unchanged increments the reference while the guard skips the release, so each
re-store leaks one. Exactly the shape of §4's `swap_blob_hash` defect in a different
table, and worth fixing in the same pass now that the pattern is understood: the
caller's increment and the release have to be paired at one site, not left for two
functions to agree about.

**It already is.** Both attached write paths pair them within one function, which is
exactly the remedy this item asks for:

* `store_attached_blob` reads the superseded hash *before* upserting and then
  branches — different content releases the old reference, **same** content cancels
  the phantom increment `store_from_stream` took unconditionally.
* `store_attached_blob_if_absent` releases on `inserted == 0`, where the row that
  would justify the reference belongs to a concurrent winner.

Left in place rather than deleted because the reasoning is the same one Part B's FK
rests on, and because "fold this in" would otherwise read as outstanding work to
whoever picks the part up next.

---

---

# Part C — The compensation paths, which fail during the same outage

`storage-consistency.md` §2e concluded that creation-side residue is crash-only
because the upload path compensates on error. That holds, with one case it did not
chase down: **the compensation itself makes backend calls, so during an outage it
fails too** — and one of those failures has a data-loss tail rather than a leak.

## `IngestGuard`'s rollback registers chunks it could not sync

The rollback does two things in order: `backend.sync_blobs(&hashes)`, then an INSERT
of those chunks at `ref_count = 0` with `orphaned_at` set, so the GC can reclaim
them — *"a backend file with no PG row would be invisible to it"*, which is correct
and the whole reason the registration exists.

**The INSERT is not gated on the sync result** (`blob_handler.rs:323`) — the sync
failure is a `warn!` and the rows land regardless.

Harmless where `sync_blobs` is a no-op (S3, Azure — a PUT is durable on return).
On **Local** it is a real fsync, and the chain is:

1. rows claim the chunks exist; durability was never confirmed
2. a later identical upload's `pin_claimable_chunks` finds them, treats them as
   present, and skips the write — *"chunks the store already has are dropped from
   RAM without any disk I/O"*
3. that file now references bytes nobody confirmed hit disk; a crash loses them

Needs Local + failed fsync + identical re-upload + crash. Narrow, but the only
compensation here that errs toward loss.

Fix: **insert only the hashes that synced.** The rest become row-less objects —
orphans, which `backend_consistency` finds and `backend_reclaim` reclaims. Leak, not
loss.

**DONE 2026-10-03.** `sync_blobs` failing now returns before the INSERT, so none of
the batch is registered. `sync_blobs` is all-or-nothing, so "only the ones that
synced" and "none, on failure" are the same set here; per-hash granularity would
need the backend trait to report per-object results and would buy nothing, since the
rows exist only so the GC can find bytes that `backend_consistency` enumerates
anyway. The failure is an audit line naming which job will find the residue. The
cost, worth stating: reclamation now waits for a bucket walk rather than the next
cheap DB sweep.

Not covered by a test — proving the gate needs backend fault injection, and the
`FaultyBlobBackend` harness this plan's § Verification calls for does not exist yet
(only a comment in `cached_blob_backend.rs` anticipating it). Building it is its own
piece and serves Verification item 1 too; deliberately not half-built to make one
gate look covered.

## Chunked-upload cleanup: right answer, fragile reasons

The audit filed the six `let _ = fs::remove_*` calls in `chunked_upload_service` as
"local scratch with an owning cleanup path" — benign. That conclusion holds, but it
was asserted rather than verified, and checking it turned up two things worth
fixing.

### The expiry loop is a detached task, not a job

`cleanup_loop` (`:447`) is spawned with `tokio::spawn` at construction (`:255`) and
ticks hourly, expiring sessions after 24 h. It therefore has no admin trigger, no
run history, no findings and no visible progress — the exact situation §1 of the
previous plan existed to correct for `spawn_legacy_rechunk`, and the argument
transfers without modification.

It arguably matters more here. The failure this guards is the one
`storage_cleanup_check.sh` names outright — *"which under sustained sync workloads
is the classic 'disk fills up over the weekend' failure mode"* — and today an
operator watching that happen has no way to ask how many sessions were reaped, how
many unlinks failed, or whether the loop is running at all. A job would also give
the orphan-scan count somewhere to land.

Promotion is mechanical now that `backend_rechunk` is the worked example:
`Mutates::Always`, a real interval rather than a hand-rolled `tokio::time::interval`,
and the per-session failures as findings instead of `warn!` lines.

**DONE 2026-10-04** — `uploads_cleanup`, reporting `sessions_expired`,
`orphan_dirs_removed`, `failures` and `sessions_live` on every run. Named for the
`trash_cleanup` / `notifications_cleanup` / `job_runs_cleanup` family.

A plain `JobHandler`, not recoverable, and the difference from `backend_rechunk` is
the reason: that one resumes because it walks a DB cursor that survives a restart,
while this walks an in-memory `DashMap` and a directory listing. No cursor to
persist, and a half-finished pass is redone an hour later at no cost.

**Deviation from the paragraph above:** the failures are an audited event with a
stable `reason` plus a count on the run, not findings. Findings live in
`jobs.run_findings`, which only the recoverable engine writes — so "as findings"
would mean making the job recoverable for the sake of a row. Same information,
reachable the same two ways.

### `sessions.remove()` happens before the unlink

```rust
sessions.remove(&id);                       // the record is gone first
if let Err(e) = fs::remove_dir_all(&temp_dir).await {
    tracing::warn!("Failed to cleanup expired upload {}: {}", id, e);
}
```

Same shape as the blob-deletion defect: drop the record, best-effort delete, log the
failure. Once `sessions.remove()` runs, the map-driven pass can never see that
directory again.

Saved by a second mechanism — the same loop also walks `read_dir(&temp_base_dir)`
and removes anything whose **mtime** exceeds `SESSION_EXPIRATION`, regardless of map
membership. That disk walk is what the blob path lacked, its sweep being DB-driven.

Latent trap, not active bug. Fix — backend first, record second, absent counts as
done:

```
match fs::remove_dir_all(&temp_dir) {
    Ok(_) | Err(NotFound) => sessions.remove(&id),   // settled
    Err(e)                => keep the entry, retry next pass, record the error,
}
```

Same rule `backend_reclaim` states: deleting something already absent is success.

**Invert when the record is private; use a queue when its disappearance is
semantics.** The session map is internal, so inverting suffices. `storage.blobs`
could not be inverted — the row's removal is what makes the delete visible — so the
intent had to live elsewhere, which is `storage.pending_actions`.

Both items are small. They are in this plan because the pattern is now recognisable
rather than because the symptoms are urgent.

**DONE 2026-10-04**, exactly as written above. Three tests on the extracted
`cleanup_once`, which needs no database: the happy path, `NotFound` settling the
session, and a failed unlink retaining it. The failure is injected by pointing
`temp_dir` at a regular **file** so `remove_dir_all` returns `NotADirectory` — the
obvious permissions trick is useless in CI, which often runs as root where `chmod`
does not prevent a removal.

## The rule for compensations

Best-effort must mean **the failure errs toward leak, never loss**. Logging is not
the bar.

| compensation | on its own failure | direction |
|---|---|---|
| `remove_reference` after a failed row write | over-count — blob pinned | leak ✓ |
| `IngestGuard` pin release | over-count — chunk pinned | leak ✓ |
| chunked-upload expiry unlink | spool dir stranded | leak ✓ (via the mtime walk) |
| `IngestGuard` chunk registration after failed sync | row claims unsynced bytes | **LOSS** ✗ |

Two corollaries:

* **"Discoverable" presumes a job that runs.** Three rows above are acceptable only
  because something sweeps them; `blobs_consistency` is `interval = None`. Hence
  Part A Phase 4.
* **Enumerate the side that still holds the evidence.** The chunked-upload backstop
  walks the filesystem and works; the blob sweep walked the database, where a
  row-less object is invisible by construction.

---

# Part D — Surfacing: a finding nobody sees is not a detection

The chain this plan family depends on is **detect → classify → run → surface →
act**, and review caught that the first draft covered only the middle. Scheduling
detectors that nobody is told about produces audit records, not safety.

## What is already true

The data is not missing. Every run returns `severity_counts` alongside
`finding_count`, computed from `finding_severity_counts(run_id)` — so a run that
recorded a `data_loss` finding says so in its outcome.

Two things lose it before it reaches a human:

1. **The admin panel reports the run as "Ok" regardless** — known as
   `bug_admin_jobs_ok_despite_findings`. `outcome == "ok"` means *the run walked its
   whole subject*, which is not what the word suggests to someone scanning a list.
   A run that completed and found data loss is, to the eye, identical to a clean one.

   **Half of this was already fixed, and the half that remained was the one that
   mattered.** The panel has flipped its pill to amber "issues" on a non-empty
   `severity_counts` since 2026-07-29 — but it read those counts from
   `last_outcome`, which is *in-memory, written when a dispatch completes*. A
   restart empties it, so the amber pill survived only until the next restart and
   the row fell back to a neutral "—". That is precisely the case this plan is
   about: a **scheduled** detector runs with nobody watching the dispatch, so the
   in-memory copy is the one nobody ever sees. Findings are durable rows; the
   signal drawn from them has to be too.
2. **There is no out-of-band path at all.** Learning about a finding requires
   opening the right run and expanding a drawer, on purpose, having already
   suspected something.

So the cheapest, highest-leverage fix in this entire plan is (1): make a run with a
non-empty `severity_counts` not say "Ok". It is a frontend change, it needs no new
machinery, and without it every other phase here is theatre.

## Notifications: what exists, and what it is not shaped for

`NotificationApplicationService` writes a `notif.notifications` row and publishes a
thin bus event, with a principle already stated in its own docs — *"the DB row is
the truth, the bus is best-effort"* — which is exactly the right shape and should
be preserved rather than reinvented.

But that table is `user_id UUID NOT NULL REFERENCES auth.users(id) ON DELETE
CASCADE`: it is **per-user and in-app**. A job finding is addressed to whoever
operates the instance, who may have no account, may not be logged in, and must be
reachable when the instance is unhealthy. That mismatch is what the deferred
"`NotificationSink` refactor first" note was pointing at, and it is a real
prerequisite rather than a preference — fanning findings out to every admin's
in-app inbox would be a workaround that still fails the case that matters.

**Email is the only out-of-band transport that exists**, and it is template-driven:
`smtp_email_sender.rs` plus askama templates under `templates/`, already used by
`magic_link_invite_service` and `recipient_notification_service`. Webhook — and
therefore Slack — is entirely new.

Two consequences for an operator email that the existing machinery would otherwise
push the wrong way:

* **It needs a template, not a formatted string.** Reusing the established pattern
  is right, but the templates today are user-facing transactional mail (an invite,
  a share). An operator alert is a different genre: it wants the finding kind, the
  severity, the counts, the run id and a direct link to the run — closer to a
  report than a message.
* **Locale resolution differs.** User mail resolves `preferred_locale` from the
  recipient's account. An operator alert may have **no user** behind it, so it must
  fall back to the instance locale rather than to a recipient who does not exist.
  That is a small branch, but it is the kind that is discovered at 3am in the wrong
  language if nobody states it.

## The shape

One port, several transports, and **Slack is not a third integration** — it is a
webhook with a payload shape. Discord and Teams likewise. So:

```
finding (durable, in jobs.run_findings)
   └── NotificationSink  (port; best-effort fan-out)
         ├── in-app      → NotificationApplicationService   [exists, user-scoped]
         ├── email       → smtp_email_sender + askama       [exists, needs an operator template]
         └── webhook     → POST JSON to a configured URL    [NEW]
               └── formatters: generic | slack | discord | teams
```

That keeps the licence constraint clean too (`AGENTS.md`): a webhook plus a payload
template needs no vendor SDK, where a Slack client library would add a dependency
and a vendor coupling to a project that must stay self-hostable.

### In-app: parked 2026-10-03, and the blocker turns out to be dissolvable

Low priority — both out-of-band transports ship, so an operator is already
reachable. Recorded because the investigation reached a conclusion worth not
redoing.

**The right AuthZ class already exists.** `Topic::Job(name)` (`job:<name>`) is
`AuthzCheck::RoleAdmin` — non-admins get `topic_forbidden`, indistinguishable
from an unknown topic — and already carries `JobRunStarted` / `JobRunProgress` /
`JobRunEnded`, published from `engine.rs`. The gap is **granularity, not
permission**: it is one topic per job name, with no wildcard in `Topic::parse`,
so an ops feed would need a subscription per job and would still miss one the
panel is not watching.

**The per-user mismatch above dissolves if in-app is a bus event rather than a
row.** The objection to `notif.notifications` is its `user_id UUID NOT NULL`,
which forces a recipient a finding does not have. A transient admin-only bus
event has no recipient column, so the question never arises — and it preserves
this plan's own principle exactly: the row in `jobs.run_findings` is the truth,
the bus is best-effort.

So the shape, when picked up: one new admin-only topic (`Topic::Ops`, same
`AuthzCheck::RoleAdmin`) plus a third `NotificationSink` that publishes to it.
Making it a sink rather than a bespoke call is the point — it inherits the
severity gate, the transition diff and the per-run de-duplication, so in-app
cannot drift from what email and the webhook say. Keep `job:<name>` for
progress (high-frequency, per-job) and put alerts on the new topic (rare,
instance-wide).

The same erring-direction rule from Part C applies to delivery: **the finding row
is the truth and the sinks are best-effort.** A webhook that 500s must not fail the
job run or lose the finding — but it also must not be silent, so a failed delivery
is itself worth a counter on the run.

**What shipped, and the four things review changed.**

* **The webhook is a transport, not a jobs feature.** `OXICLOUD_WEBHOOK_URL` /
  `_FORMAT` / `_TARGET` define a destination the way `OXICLOUD_SMTP_*` does, and
  `OXICLOUD_JOBS_NOTIFY_MIN_SEVERITY` is the jobs-side policy about what reaches it.
  One `Arc<dyn NotificationSink>` on `AppState`, shared by the job notifier and the
  admin test endpoint, so the thing an operator tests is the thing that delivers.
* **Six formats, two of which carry their recipient in the body.** Slack, Discord
  and Teams are the same POST with the text under a different key. Telegram
  (`chat_id`) and ntfy (`topic`) need a `target`, refused at construction rather
  than at the first alert — a channel discovered to be misconfigured when something
  breaks is silent exactly when it matters. ntfy also gets a 1–5 priority so a
  resolution does not buzz a phone at 3am.
* **Retry, but only what can recover.** Five attempts, 1s doubling, on a transport
  failure / `429` / `5xx`. A `4xx` returns immediately: a wrong URL or revoked token
  will not fix itself, and the job's terminal path is waiting. The admin test takes
  the opposite trade — one attempt, no retry — and reports the receiver's status
  **and body**, because a bare 403 does not distinguish a revoked token from a
  disabled channel while the body says `invalid_token`.
* **No alert-state table.** The transition baseline is the previous *completed* run's
  findings, which the history already records. Completed-only matters: a paused run
  holds partial findings, so diffing one would announce everything it had not reached
  yet as resolved — an all-clear for work that never ran.

Still open from this phase: email (needs an operator template and an instance-locale
fallback, since an operator alert may have no user behind it) and in-app, which waits
on the per-user/`user_id NOT NULL` mismatch rather than working around it.

## The three rules that decide whether this is useful or hated

1. **Severity gates delivery.** `data_loss` notifies; `inconsistent` is
   configurable; `info` never does. `orphan_blob_queued` and
   `legacy_blob_rechunked` are `info` precisely so that a healthy instance doing
   its job is silent.
2. **Notify on TRANSITION, not per run.** Once the detectors are scheduled, a
   persistent finding recurs every run — twenty scheduled runs must not send twenty
   alerts. Notify when a finding kind first appears for a subject, and again when it
   clears. This needs a small amount of state (last-notified per kind), and without
   it scheduling plus notification is an alarm that rings until it is disconnected.
3. **Classification is a hard prerequisite.** Part A phases 1–3 must land first.
   Notifications *amplify* false positives — the OVH incident would have paged
   someone at 3am about data loss that never happened, and the second time that
   happens the channel is muted and the feature is worse than nothing.

## Revised phase ordering

Surfacing changes the order given earlier in this plan:

| # | phase | state |
|---|---|---|
| 1 | Retry (Part A ph. 1) | **DONE** — `verify_bytes` re-reads once |
| 2 | Classify (Part A ph. 2) | **DONE** — `classify_stream_read_error` + source chain |
| 3 | Pause (Part A ph. 3) | **DONE** — `backend_consistency`, `backend_rotate`; `backend_rechunk` already had it |
| 4 | **Panel shows findings** | **DONE** — counts read from `jobs.run_findings`, not from memory |
| 5 | Schedule the detectors (Part A ph. 4) | **DONE** — `consistency_batch=168h` by default, `OXICLOUD_SCHEDULED_JOBS` to change it |
| 6 | `NotificationSink` + transports | **DONE (webhook + email)** — transition-diffed, severity-gated; in-app parked, see below |
| 7 | Alert when a run *stopped* | **DONE** — not in the original plan. Keyed off `error_reason` on the run row; an operator pause sets neither error column and stays silent |

**Implementation note that simplified phase 1.** The plan called for a re-read in
`verify_bytes`, `backend_rechunk` and `backend_rotate`. Only `verify_bytes` needs
one: it writes a per-object verdict and must conclude before moving on. The other
two are **resumable**, so pausing at the cursor *is* the retry — the engine resumes
and re-reads. So phase 1 collapsed into phase 3 for the migration jobs, and
`backend_rechunk` needed no change at all: it already paused on transient, and the
classification fix is what finally makes that branch reachable for a streaming
failure.

`backend_rotate` did need it — it had no transience check anywhere, and recorded
`rotation_failed` at `data_loss` for read AND write failures alike.

Phases 1–2 before 3–5 is the load-bearing ordering. Everything else can move.

## Verification

1. **Phase 1 is provable with fault injection.** The `FaultyBlobBackend` pattern
   from PR #771 extends to failing a read mid-body: assert that a first-attempt
   failure followed by a success records *no* finding, and that two identical
   failures record one.
2. **Phase 3 needs the pause asserted, not just the finding absent.** A transient
   read failure must leave the run `Paused` with a cursor and **zero** findings —
   the bug being guarded is a finding written before the pause.
3. **The FK migration needs the no-op test** `storage-consistency.md` used for its
   prune step: on a snapshot taken before it, assert every derived row's effective
   reachability is unchanged, and that the orphan deletion released exactly as many
   references as it removed rows.
4. **Satellite repair needs the queue assertion**, mirroring
   `refcount_same_content_rewrite.hurl`: after repair the mapping is gone, the blob
   is queued, the bytes are still present, and only after `backend_reclaim` are they
   unlinked.
5. `just check`, `just test`, `just test-integration`, `just api-test`. Note the
   satellite paths DO have API coverage (`derived_blob_copy.hurl`,
   `attached_thumbnail_copy.hurl`, `thumb_import_check.sh`), unlike the cache job.

## Not in scope

* **Unifying the two satellite tables.** The asymmetry is provenance, not
  accident; collapsing them would either make uploaded previews shareable across
  files (wrong) or stop thumbnails deduping (wasteful).
* **Retrying downloads.** Past the first byte it is impossible; Range plus client
  retry is the design.
* **A `time_to_live` on the blob cache.** `backend_cache_cleanup` handles staleness
  and moka handles capacity; a TTL would evict hot entries for no benefit.
