# Recoverable errors in jobs — retry, then pause

**Status: implemented 2026-09-08**, except the online-migration
follow-up in [Where this should end up](#where-this-should-end-up),
which remains deliberately out of scope. Design settled 2026-08-31,
from a live diagnosis (see
[Motivating incident](#motivating-incident)).

Steps 1–4 are in, and §Testing is enforced by
`tests/api/backend_migration_blackhole.hurl`. Validated by hand against
a real S3 endpoint in both directions, with the network blackholed
mid-run: source-probe failure, target-upload failure and target-init
failure all reach `Paused` with the cause recorded and the cursor
positioned so nothing is skipped.

Two things the design did not anticipate, both found during
implementation and worth reading before touching this area:

* **Classification was not enough on its own.** A backend that *fails*
  was the case this plan modelled. A backend that never *answers* has
  no error to classify, so no amount of retry policy sees it. The S3
  client was built from a bare `config::Builder::new()` and carried no
  `TimeoutConfig` at all, which also meant `SdkError::TimeoutError` —
  an arm the classifier already handled — was unreachable in
  production. Fixed with `TimeoutBlobBackend` (innermost, below retry)
  plus the SDK's own connect/read timeouts and stalled-stream
  protection. See that module's docs for why the bound lives in the
  chain rather than being configured per SDK.

* **`NotFound` was returned for every read failure.** Nine sites across
  S3, Azure and local. `blob_exists` was among them, and it is the
  migration's FIRST probe of the source — so a transient outage read as
  "blob absent", took the permanent branch, advanced the cursor past
  the row and could reach `finish_completed` with the pointer flipped
  to a target missing everything the outage covered. A migration
  reporting success having silently dropped whatever was unreachable.
  That path is why the retry-then-pause policy alone would not have
  been sufficient.

Reporting bugs surfaced by the same testing, all fixed: a paused run
logged `outcome="ok"`, a resumed run inherited the previous attempt's
`error_message` through to `Completed`, and four of five migration
counters reset per segment while `scanned_count` alone was cumulative.

Known remaining, none of them blocking:

* The online-migration shape below (the gate is still held for the
  whole copy, so a paused migration freezes writes until an operator
  resumes or cancels).
* Per-backend SDK retry tuning — see [Scope](#scope-azure-and-s3-both).
  Two retry layers currently stack multiplicatively, so the configured
  retry count is not the effective one and detection takes ~2min rather
  than ~30s.
* `scanned_count` over-reported once (2522 against 2022 rows) on a run
  with several pause/resume cycles. Not reproduced in five runs since;
  now observable rather than inferable, because counters are persisted
  per batch.

A job that hits a failing backend today has two possible endings, and
neither is right for an outage: it fails the run (throwing away a
partially-complete migration, since `Failed` is terminal and only
`Paused` resumes), or it hangs forever inside an SDK retry loop with no
log line and no way to act on it.

This plan adds the third: **retry a bounded number of times, then pause
with the reason recorded**, so an operator resumes when the provider
recovers and the job continues from its cursor.

---

## Motivating incident

`backend_migration ?storage=azurite` hung indefinitely. Diagnosis, after
several wrong theories:

- The job issued a ranged GET carrying `x-ms-range-get-content-crc64`.
- Azurite answered **500** (real Azure supports CRC64 range validation;
  the emulator does not).
- `azure_core`'s retry policy classifies 500 as retryable and loops.
- The response was deterministic, so every retry failed identically.
- The job never advanced, never failed, and emitted no per-blob line —
  while holding `migration_readonly`, refusing writes **across the whole
  application**.

The exact chain was pinned down later (2026-09-02) and is worth having,
because it is not where you would look — the copy itself is innocent:

```
backend_migration_service.rs   target.head_check(hash)   ← pre-write probe
  → EncryptedBlobBackend::head_check
  → get_blob_range_stream(hash, 0, HEADER_SIZE)          ← ~40 bytes
  → azure_core Range::as_headers                         ← adds the CRC64
      (src/request_options/range.rs: any range < 4 MiB)     header, no opt-out
```

`copy_blob` reads from the SOURCE, which is local in a local→Azure
migration, so it never touches an Azure range. What hangs is the format
probe against the TARGET, on the first blob, before a byte is copied.

A workaround exists — unranged `get()` for small requests, truncate
client-side — and was **rejected 2026-09-02**: it pays for an emulator
with production read amplification and puts new offset arithmetic on the
read path. See the note on `AzureBlobBackend::get_blob_range_stream`.
The fix is the official SDK, where `range_get_content_crc64` is an
explicit field. **This plan is unaffected either way** — a bounded retry
would have turned the hang into a Paused run with a reason, which is the
point.

Two properties made it invisible: the 500 was only visible at
`azure_core=debug`, and nothing bounded the retry. The same shape would
occur against real Azure or S3 on any persistent 5xx; it is not an
emulator quirk. `AzureBlobBackend` configures no retry policy and no
timeout at all — `grep "retry\|timeout\|ClientOptions"` on
`azure_blob_backend.rs` returns nothing.

Full evidence chain, including the theories ruled out and what each
cost, is in the memory note `bug-azure-put-blob-hangs-no-timeout`.

---

## Why this belongs at the top level

Every recoverable job goes through `run_or_resume`, which already owns
the run lifecycle: it opens the row, persists and restores
[`JobRunArgs`](./job-registry.md) (`63820c3c`), dispatches the handler,
and writes the terminal state. Retry-and-pause is the same kind of
concern — policy about *how a run behaves*, not about what any one job
does.

Implemented there, `backend_rotate`, `transcode_import`, the thumbnail
imports and anything added later inherit it. Implemented per-job, it
gets written once per job and drifts.

It also survives the pending official-Azure-SDK migration untouched,
where per-SDK retry tuning would have to be redone.

**The blocker is that the engine cannot currently act on what it is
told.** A handler returns `RunOutcome::{Completed, Paused, Failed}`, so
a transient backend error is already flattened into `Failed` before the
engine sees it — "the provider is down" and "this data is wrong" are
indistinguishable. Closing that is what makes a top-level
implementation possible, and it is step 1.

---

## Step 1 — errors say whether they are retryable — **DONE**

Today both backends wrap SDK errors into
`DomainError::internal_error("Azure", format!("…{e}"))`, so the status
code survives only inside a formatted string. Recovering it means
string-matching, which is exactly the kind of fragility that turns into
a silent behaviour change when an SDK reformats its `Display`.

Carry the distinction on the type instead — a `retryable` flag, or a
kind the storage ports set deliberately.

**Retryable** (environment, may clear on its own):
- HTTP 5xx, 429 / `SlowDown` / throttling
- connect timeouts, connection resets, DNS failure

**Permanent** (will fail identically forever):
- 4xx other than 429 — 401/403 (credentials), 404 (missing container)
- decode failures, checksum mismatch
- `operation_not_supported`

> **The Azurite 500 is a permanent error wearing a retryable status
> code.** No classification by status alone gets this right, which is
> the case for a bounded cap rather than "retry until it works". The cap
> is the safety net for exactly the errors the taxonomy misjudges.

**Do not double-retry.** The AWS SDK already retries internally with its
own backoff, so a second layer above it multiplies. Check what the S3
backend inherits before adding anything, and consider making the
engine's cap the *outer* bound with SDK retries reduced or disabled.

## Step 2 — an outcome the engine can act on — **DONE**

`RunOutcome` grows a variant meaning "the environment failed, this is
worth trying again later":

```rust
RunOutcome::PausedRetryable { cursor: Vec<u8>, reason: String }
```

Distinct from all three existing outcomes, and the distinction is the
point:

| outcome | meaning | resumes? |
|---|---|---|
| `Failed` | the data or the request is wrong | no — terminal |
| `Paused` | an operator asked it to stop | yes |
| `PausedRetryable` | the environment failed | yes, and says why |

The row lands as `Paused` either way, so resume works unchanged. What
differs is `error_message`, which must let the panel — and an operator —
tell "I paused this" from "the provider went down". Without that
distinction a paused run is an unexplained one.

## Step 3 — the engine implements the policy — **DONE**

In `run_or_resume`:

- bounded exponential backoff, ~5 attempts
- **log each failed attempt at `warn` on our side.** The incident took
  several runs to diagnose because the 500 was visible only at
  `azure_core=debug`. One line per exhausted operation, naming status,
  target and attempt count.
- on exhaustion, write the row as `Paused` with the reason in
  `error_message`

Handlers then return the retryable outcome and get the policy for free.

---

## Step 4 — `migration_readonly`, the sharp edge — **DONE** (conservative option; gate still held while paused, confirmed as intended)

`backend_migration` holds a gate that refuses writes **application-wide**
until cutover. What happens to it on pause is a correctness question,
not a cosmetic one.

### Start conservative: keep the gate held while paused

Correct, because no writes during the pause means the cursor stays valid
and resume-from-cursor is sound.

It is also **strictly better than today**, which is the thing to
remember when the read-only window looks unattractive: the application
is *already* read-only while the job hangs — there is simply no way to
see why or act. Same lock, now with a reason and two operator actions.

**Cancel must clear the gate.** It ends the run with no swap, so the
source stays active and writes must return. The `failed > 0` path
already clears readonly on the reasoning that "users shouldn't be locked
out because of a partial run" — cancel is the escape hatch operators
will reach for during an outage, and it must work.

**Make the state loud.** Writes refused app-wide should be obvious in
the admin panel, not discovered by reading a run row. The progress
snapshot already feeds the header middleware; a paused migration wants
the same visibility, saying why and offering resume/cancel.

### Why NOT to release the gate on pause (yet)

Tempting — ops should not be locked out during a provider outage — and
unsafe as the job stands:

1. Release readonly; users write again. New blobs land on the source,
   which is still active, and are absent from the target.
2. Resume continues **from the cursor**, a position in a hash-ordered
   walk.
3. A blob written during the pause whose hash sorts *before* that cursor
   is never visited.
4. The run completes, flips the pointer, and reads for that hash 404
   against a target that never received it.

This is why the existing `failed > 0` path can safely clear readonly: it
**ends** the run, and an operator retrying starts a new run with a fresh
cursor, so everything is rescanned. Pause-and-resume is what
reintroduces the gap.

Releasing on pause becomes safe only alongside one of:

- **resume rescans from the beginning** rather than trusting the cursor
  — cheap, because the walk short-circuits on blobs already present in
  the target, so a second pass costs a lookup per blob, not a copy; or
- **a final catch-up pass under readonly before the swap**, with the
  pointer flipping only when a complete pass finds nothing new.

### Where this should end up

The second option is the standard online-migration shape, and it is
where this job wants to go regardless: copy the bulk **without** holding
the gate, engage it only for a short final catch-up plus the swap.

That removes what made the incident damaging — writes refused app-wide
for the entire duration of a long copy — and turns releasing-on-pause
into a free consequence rather than a correctness fix. Worth doing as a
follow-up, from the safer base this plan establishes.

---

## Scope: Azure and S3 both

The only reason S3 looks healthy is that the endpoint behaves. A
persistent 5xx from S3 hangs identically — the gap is the absence of a
bound, not anything Azure-specific.

Putting the policy above the `BlobStorageBackend` trait covers Azure,
S3, local and anything added later with one implementation. Per-backend
SDK retry tuning stays a separate, optional refinement — and for Azure
specifically it should wait for the official SDK, since `azure_core`
0.21 is archived and queued for replacement.

## Testing — **DONE**

Implemented as `tests/api/backend_migration_blackhole.hurl`, though not
the way this section anticipated. Two departures worth recording:

**The fixture is an unreachable address, not Azurite.** Azurite's
deterministic CRC64 500 is a *failure*, and failures were never the
hard case — they surface, get classified and retry. The case that hung
is a peer that never answers, so the entry points at `192.0.2.1`
(TEST-NET-1, RFC 5737, guaranteed unrouted): a SYN goes unanswered,
with no RST and no ICMP. Note the existing `s3_stub` entry
(`127.0.0.1:9999`) is NOT usable for this — nothing listens, so the
connection is refused instantly, and a test built on it would pass with
no timeout configured anywhere.

**The bound is a polling budget, not a request duration.**
`backend_migration` is a detached job: the trigger returns 202 in
milliseconds regardless of how long the backend hangs, so timing the
trigger proves nothing. The file polls `/runs` with `retry: 60,
retry-interval: 2000` — 120s, three orders of magnitude below the
unbounded socket's ~15min. That budget is the assertion; every other
assert in the file would eventually pass even unbounded.

It targets the **target-init** failure deliberately, because that path
pauses BEFORE `migration_readonly` is engaged and so cannot leave the
shared suite's server read-only. It also cancels its own run: a Paused
row left behind would be resumed by the next `backend_migration`
trigger in the suite.

The original notes follow.

The `backend_consistency_azure.hurl` scenario and its Azurite service
are already wired (`tests/common/docker-compose.test.yml`,
`spawn-db.sh`), which gives a backend that reliably produces the
failure: Azurite 500s on the CRC64 ranged GET every time. That makes it
a genuine fixture for this work rather than a flake —
**deterministically unretryable-but-retryable-looking**, which is the
hard case.

Note the scenario no longer triggers a migration — it audits Azurite
without cutting over, for the reason in its header. Reaching the fixture
means triggering `backend_migration ?storage=azurite` explicitly, which
is exactly the hang this plan is meant to convert into a Paused run.
Doing that inside the shared suite is what ordering it last was for; it
can go back once the outcome is bounded.

Assert the run reaches `Paused`, that `error_message` names the cause,
and that it does so in bounded time rather than hanging.

`POST /api/admin/settings/storage/test` with `entry_name` is the
pre-flight worth keeping in mind: synchronous, does a real write/read
round-trip, and isolates "the backend is misconfigured" from "the job
is broken". It passing while the migration hung is what ruled out
credentials, container and the write path during the incident.
