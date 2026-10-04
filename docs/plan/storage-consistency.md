# Plan — Storage consistency: keeping the registry and the backend in agreement

**Status:** design captured 2026-09-27 from a sandbox investigation; review
closed and **implementation started 2026-09-27**. Triggered by 29
`orphan_blob` findings and two `manifest_refcount_mismatch` findings on a live
S3 + encryption + cache deployment, plus a deliberate network-outage test.

Progress, kept current as work lands — each item carries its own marker at its
heading:

| item | state |
|---|---|
| §1a `backend_rechunk` job | **DONE** — unit tests + API check green |
| §1b retire `OXICLOUD_LEGACY_RECHUNK` | **DONE** — deprecation warning; removal next major |
| §1c convergence visible + `LEGACY-WHOLE-FILE-BLOB` markers | **DONE** — finding + 7 tagged sites |
| §1 **delete the legacy path** | **DEFERRED TO THE NEXT MAJOR** (decided 2026-10-04) — two gates, both outside this plan: the migration must have terminated fleet-wide, *and* the release must be a major. See §1c. |
| §1d share the walk with `backend_rotate` | **CLOSED** — premise was wrong; rechunk is DB-filtered to legacy blobs |
| §2a schema + atomic enqueue | **DONE** — `storage.pending_actions`, reap-and-enqueue in one statement |
| §2b `backend_reclaim` drain | **DONE** — scheduled every 300s, per-object `FOR UPDATE SKIP LOCKED` |
| §2c backlog visibility | **DONE** — depth, oldest-age and parked count on the dedup stats |
| §2d `backend_consistency` repair arm | **DONE** — `?repair=true` ENQUEUES; it never deletes |
| §2e creation side | design note only, no code by design |
| §2f resurrection race | **DONE** — drain re-verifies under lock; chunk settle cancels before writing |
| §3 Backoff, and the nested SDK retry | **DONE** — per-document flush backoff; SDK retry made explicit |
| §4 Same-content refcount leak | **DONE** — unit + API suite green |
| §5 Boot: unreachable vs misconfigured | **DONE** — 60 s bounded retry on transient only |
| §6 Cache plaintext — eviction ordering | **DONE** — unit test green |
| §6 Cache plaintext — `backend_cache_cleanup` job | **DONE** — hourly, remote backends only |

§4 and §6's ordering fix were taken first deliberately: both are surgical, both
are independent of the queue, and both are live defects on the reporting instance
rather than design work — §4 is the direct cause of the
`manifest_refcount_mismatch` findings, and §6's ordering bug is why the deleted
content was still readable in plaintext.

**§2a, §2b and §2f had to ship together**, which was not obvious from the plan's
numbering. The queue alone makes the resurrection race *worse* than the
best-effort unlink it replaces: the window between "row reaped" and "bytes
unlinked" grows from sub-second to the drain interval, so an upload landing inside
it would adopt bytes about to be deleted. §2f is not a follow-up to §2b, it is the
other half of the same change.

**Every base-blob unlink path now goes through the queue**, which took two more
sites than §2a covered. A note here previously called this gap "derived and
attached blob deletions still unlink directly" — that was wrong in an instructive
way, and the correction reduces the work rather than adding to it:

**Derived and attached artifacts have no unlink path of their own.**
`purge_derived_blobs` deletes the mapping rows and calls `remove_reference` on
each `blob_hash` — they are ordinary content-addressed blobs in `storage.blobs`,
so their physical deletion has always flowed through the base-blob paths. That is
exactly what the `object` column's rationale says: it records the ORIGIN of the
request, not a separate code path. So there was never a derived/attached deletion
path to convert; there were two base-blob paths still to convert, and doing so
covers derived and attached completely.

The full set, for anyone auditing it later:

| path | state |
|---|---|
| `dedup_gc` reap loop | queues (§2a) |
| `rechunk_one_legacy_blob` | queues on unlink failure |
| `remove_legacy_reference` | queues — was delete-row-then-best-effort-unlink |
| `cleanup_if_orphaned` | queues — same shape |
| `storage_settings_service` ×6 | not applicable: a backend connectivity self-test writing a synthetic probe blob with no PG row, whose cleanup result is reported to the admin |

The consequence for tests: anything asserting that bytes are gone from disk now
needs BOTH phases — a `dedup_gc` pass and a `backend_reclaim` drain. Previously
most deletes unlinked synchronously, so a GC pass was enough.

And one flake this creates by construction: `backend_consistency` counts
`orphan_blob`, which is precisely what the queue holds between a reap and its
drain. Since the drain runs on a schedule, that count legitimately DROPS during a
suite. Baseline comparisons against it must therefore be an upper bound (`<=`),
never equality — an equality assertion fails on a background job doing its job.

A correction to an earlier note here, which claimed `backend_consistency`'s repair
arm "still deletes directly": it had **no** repair arm at all — it was
discovery-only. §2d therefore added one, and the shape matters. It does not delete;
it **enqueues**, and `backend_reclaim` does the deleting. Two reasons, and the
first is the one that makes this better than a direct repair would have been:

* The scan holds a **stale listing** — enumeration began before the comparison
  reached any given hash — so deleting from inside it acts on a view that may
  already be wrong. The drain re-verifies under a row lock immediately before its
  unlink, so an object referenced again in the meantime is never deleted; its
  queued intent is discarded instead.
* Repair inherits the retry, backoff and parking the queue already provides,
  instead of being one more best-effort delete of exactly the kind this plan
  exists to remove.

Findings distinguish the two outcomes: `orphan_blob` (`inconsistent`, nothing
done) versus `orphan_blob_queued` (`info`, handed to the drain), following the
`refcount_repaired` convention that a handled finding is not an anomaly.

Named for the guarantee rather than one of its mechanisms. It began as
"blob reclamation" — fixing deletion — but the same window exists on
creation, and the fix for both is one set of machinery: idempotent
operations, a durable outbox, and a reconciling sweep. Reclamation is what
the machinery *does*; consistency between the two stores is what it is
*for*.

Sibling of `derived-blobs.md` (which owns the *tiers* and what a blob IS)
and `storage-multi-entry.md` (which owns *which* backend a blob lives on).
This plan owns one question those two leave open: **what happens when
bytes should stop existing.**

## The defect, in one paragraph

`DedupService::garbage_collect_with_grace` deletes the registry rows for
reclaimable blobs in a single statement, and *then* unlinks the backing
objects with a best-effort loop whose failure path is `tracing::warn!`
(`dedup_service.rs:3601-3613`). The intent to delete is therefore recorded
nowhere durable. When the unlink fails — routine on a remote backend — the
row is already gone, so `dedup_gc` can never see those bytes again: it
reclaims *from the database*. They become permanently orphaned, discoverable
only by `backend_consistency` walking the bucket, and reclaimable by nothing.

Observed directly: a `dedup_gc` run, forced and unforced, left all 29
orphans untouched. `force` only sets `grace_secs = 0`, and grace was never
the obstacle.

## Why it shows up under collaborative editing first

Not a collab bug. Collab is the workload that mints the most short-lived
blobs: every debounced flush writes the document's *current* text as a new
content hash, so typing `aaaaaa` → `# test` → `it works` produces a chain of
versions, each superseded and dereferenced within seconds. Any delete path
hits the same window; collab just arrives there a hundred times faster.

Confirmed by reading the orphaned content out of `.blob-cache` — it was
hand-typed markdown test content, not fixtures.

## What we are actually building, and the invariant it buys

This is a transactional filesystem across two systems that cannot enlist in
one transaction: PostgreSQL and an object store. Neither can join the other's
commit, S3 offers no ordering and no rollback, and the only primitives on
offer are idempotent `PUT`/`DELETE` plus read-after-write. There are exactly
three tools for that situation, and the plan converges on all three — which
is a sign the design is conventional rather than clever:

1. **Idempotent operations** — content addressing already gives this. A
   repeated `PUT` of the same hash is a no-op; a `DELETE` of an absent key is
   success.
2. **Durable intent** — `storage.pending_actions` is a transactional outbox.
   Worth calling it that: the pattern's known pitfalls are at-least-once
   delivery, an idempotent drain, and poison handling, all of which this plan
   already carries.
3. **Reconciliation** — `backend_consistency` is the fsck.

**Exactly-once across that boundary is not available.** Every operation
therefore picks which side to err on, and both existing choices are the right
way round:

| operation | order | errs toward | rather than |
|---|---|---|---|
| creation | bytes, then row | orphaned bytes (`inconsistent`) | a row without bytes (`data_loss`) |
| deletion | row, then unlink | bytes outliving their row (`inconsistent`) | deleting live bytes (`data_loss`) |

The system is deliberately biased to **leak space rather than lose data**.
That is correct, and it has a consequence worth stating plainly: given that
bias, reconciliation is not optional maintenance bolted on the side — it is
the other half of the guarantee. A design that errs toward leaking and then
never sweeps has simply chosen to leak.

So the invariant to hold, and to test against, is not "no orphans" — which
is unachievable while a process can be killed between two systems. It is:

> At any instant the registry and the backend may disagree. Every
> disagreement is either (a) recorded in `storage.pending_actions`, or
> (b) discoverable by `backend_consistency`. Both are bounded and
> observable.

Where filesystems get to be genuinely transactional, it is because they own
the device and can order writes with barriers. We own neither, so the bar is
not "never inconsistent" but "converges, and the gap is visible while it
lasts". Every item below serves that sentence.

## Settled decisions

- **Durable intent, in a side table.** `storage.pending_actions`,
  written in the *same statement* as the row delete. Not a state column on
  `storage.blobs`.

  The filesystem precedent is the argument: ext3/4's orphan inode list is a
  separate on-disk structure, not a bit on the inode, because the intent is a
  work item rather than a property of the object.

  It does **not** avoid the resurrection race — an earlier draft of this plan
  claimed it did, and that was wrong. Both designs need the same
  serialisation; see §2f, which owns the race and its resolution. The side
  table wins on a narrower point: a marked row is still a row, so a state
  column would force every existing `NOT EXISTS` guard and consistency
  predicate to distinguish "live" from "marked", while the side table leaves
  all existing paths behaving exactly as today and confines the new logic to
  the drain and chunk settle.

- **Deletion becomes a drainable backlog, not a step inside a sweep.**
  Filesystems tried inline discard and moved to periodic batched `fstrim`
  for the same reason: issuing unlinks on the critical path couples
  foreground latency to backend behaviour. The network-outage test showed
  exactly that — one sweep became a long retry storm.

- **`backend_consistency` stays the fsck, and feeds the journal.** Its
  repair arm ENQUEUES what it finds rather than deleting directly. One
  deletion path, one set of retry and poison semantics, and the pre-existing
  orphans become reclaimable by the same machinery. Today the bucket walk is
  the *only* way to discover stranded bytes, which on S3 means paying for a
  full LIST to answer a question the system should already know.

- **Poisoning is mandatory, and this is where the filesystem analogy
  breaks.** Freeing a block locally cannot really fail. An S3 delete can
  fail *permanently* — credentials rotated, bucket gone, object under a
  retention lock. Without parking after N attempts, one unlovable object
  makes the drain retry forever and the backlog never converges.

- **Bug fixes do not wait for the CDC unification.** Unifying on
  manifests+chunks (§1 below) is the largest item and makes everything after
  it simpler, but §2 and §3 are stopping active bleeding. Sequencing them
  behind a migration would leave a known data-retention defect open for the
  duration.

## Work items

### 1. Finish the CDC migration — one content model, not two

`remove_reference` still branches: CDC manifest path, then "legacy
whole-file blob path" (`dedup_service.rs:2499-2514`). Every deletion,
refcount and consistency concern is therefore written twice, and the two
refcount surfaces (`storage.blobs.ref_count` and
`storage.chunk_manifests.ref_count`) can disagree — a divergence already
tracked separately.

Collapsing to manifests-with-chunks everywhere removes a class of dual-path
bugs rather than fixing instances of it.

**The migration already exists.** `DedupService::spawn_legacy_rechunk()`,
launched from `di.rs:494` behind `OXICLOUD_LEGACY_RECHUNK` (default `true`),
idempotent and incremental, a no-op via one `COUNT` once converged. So this
item is not "write a migration" — it is **confirm convergence, then delete
the legacy path.**

Note it re-reads and re-chunks rather than wrapping. A metadata-only wrap is
possible and tempting — `file_hash` is `blake3(content)` (`file_hasher`
accumulates the raw data, `dedup_service.rs:2085`), so a legacy blob `H`
could become `chunk_manifests(file_hash = H, chunk_hashes = [H])` with no
file row rewritten and no bytes moved. It would unify the code model for
free. It is still the wrong choice: a single-chunk manifest leaves Range
reads and partial decrypts paying for the whole blob, which is the reason the
existing task does the expensive thing. Recorded so nobody "optimises" the
migration into uselessness later.

**1a — Convert the background task into a job: `backend_rechunk`.** A spawned
task has no admin trigger, no run history, no findings, and no cursor anyone
can see — so "has this converged on my instance?" is answerable only by
reading boot logs. As a `RecoverableJobHandler` in the `thumb_*_import` mould
it gains all of those, plus crash recovery and pause/resume from the
recoverable-runs engine.

Naming: `backend_rechunk`, matching `backend_rotate`'s verb form. Not
`backend_chunk_migration` — `backend_migration` already means "move to
another storage entry", so that reads as migrating chunks to another backend.
The `backend_*` prefix is right because the job genuinely reads and rewrites
backend objects.

**1b — `OXICLOUD_LEGACY_RECHUNK` goes away.** Not because its stated reason
was wrong — "disable on metered remote backends where the one-time re-read of
every legacy blob should be scheduled deliberately" is a real operational
need, and on S3 with encryption it is the common case. It goes away because
converting to a job makes it **redundant**: an admin-triggerable job is
schedulable by definition, and `OXICLOUD_STARTUP_JOBS` already expresses
"run this at boot or not". Two knobs for one decision is the defect; the work
itself becomes mandatory because deleting the legacy path depends on it.

Follow the house deprecation path rather than removing it outright — warn at
boot when the variable is still set, pointing at `OXICLOUD_STARTUP_JOBS`, and
drop it next major. Silently ignoring a set variable that used to prevent a
full re-read of every blob on a metered backend would be an unpleasant
surprise.

**1c — Make convergence visible, then delete.** A consistency finding while
any legacy whole-file blob remains, so "is it safe to remove the legacy
path?" is answerable from the admin panel instead of by grepping. The legacy
code is only removable once that finding is empty across the installs that
matter, which is a release-timing decision, not a code one.

Mark the sites now with a greppable tag so removal is mechanical rather than
archaeological:

```rust
// LEGACY-WHOLE-FILE-BLOB: removable once `backend_rechunk` has converged
// everywhere; see docs/plan/storage-consistency.md §1.
```

Known sites from a first pass: the `remove_reference` legacy branch
(`dedup_service.rs:2499-2514`), `blob_reference_sources.rs:40`,
`files_consistency_service.rs:111`, and three in `encrypted_blob_backend.rs`
where the unbounded-read case exists *only* for legacy blobs — that last
group is the one that actually costs something, since it is why the decrypt
path carries an unbounded buffer at all.

**Decided 2026-10-04: removal is a NEXT-MAJOR change.** Two gates, and both
have to be open:

1. **The migration has terminated fleet-wide.** `backend_rechunk` reporting
   zero on one instance (verified 2026-09-28 on the reference deployment) is
   one data point, not a fleet.
2. **The release is a major.** Not a per-deployment judgement — removing the
   legacy *read* path makes any surviving legacy blob unreadable, and "your
   files are unreadable unless you finished a migration" is only an
   acceptable precondition at a major boundary, where an operator expects to
   read upgrade notes.

So this is **not** a boot gate that refuses to start, and **not** "one more
release". It is the same deprecation path §1b already puts
`OXICLOUD_LEGACY_RECHUNK` on — which means the variable and the code it
guards come out together, in one coherent removal, rather than across two
releases that each half-explain themselves. Current version is 0.9.x, so the
target is 1.0.

Until then the tags are the whole deliverable: when the gates open, removal
is a grep rather than an excavation.

**As implemented (1a–1c)** — three things the plan had not specified, each from a
review question worth recording:

* **Per-conversion audit line and finding.** *"Does the rechunk job log the files
  it changed? It could help Ops."* — it did not: the only per-blob line was
  `debug`, and it named the hash rather than the files. Now it emits
  `event="storage.blob_rechunked"` on the **audit** channel with `file_ids`, plus
  an `info`-severity `legacy_blob_rechunked` finding per blob, following the
  `refcount_repaired` convention (`blobs_consistency_service.rs:531`) that a
  successful mutation is not an anomaly. The audit channel is load-bearing rather
  than cosmetic: `tests/common/server.env` sets `RUST_LOG="warn,audit=info,…"`, so
  a plain `tracing::info!` is filtered out entirely. And the file ids are the part
  that cannot be reconstructed afterwards — the whole point of the conversion is
  that the file stops referencing that hash directly.
* **Transient failures pause the run instead of counting as failures.** Otherwise
  an outage marches through every remaining blob recording a finding for each,
  and a run that converted nothing still reports having examined the instance. The
  consecutive-failure cap therefore only sees permanent faults, where a streak
  really does indicate something systemic — a wrong key failing every hash check.
* **Keyset paging replaced the in-memory failed-hash exclusion list.** A failed
  hash is skipped because the cursor advanced past it, which also survives a
  pause; the list could not.

Verification landed as `tests/api/rechunk_legacy_check.sh`, and two of its
assertions are worth naming because they are what makes it a real test rather
than a smoke test: the fixture must produce **more than one chunk** (a
single-chunk file re-chunks to one chunk and would let a broken migration pass),
and `backend_consistency?deep=true` **re-hashes every chunk** afterwards. The
deep pass is the strongest available check here — `blobs_consistency` is DB-only
by design and never opens a blob, and the byte-identical download assertion can
be satisfied by a warm plaintext cache without the backend holding correct bytes.

**1d — Consider one walk for two jobs.** `backend_rotate` cannot do this work
— it is object-*preserving* by design, rewriting the same key via
`put_blob_from_bytes_replace` so that the hash and key never change, whereas
re-chunking is object-*multiplying* (one object becomes N chunk objects plus
a manifest row). But both pay the same dominant cost: **a full read of every
blob**, which on a remote backend is the egress bill and the wall-clock, not
the CPU. They differ only in what they do with the already-decrypted bytes.

**CLOSED — will not do. The premise above is wrong**, and the correction is
worth keeping because it is the kind of mistake that makes an optimisation look
attractive.

"Both read every blob" is true only on a fully pre-CDC instance. The two jobs
select completely different candidate sets:

| job | candidates |
|---|---|
| `backend_rechunk` | `WHERE NOT EXISTS (manifest) AND EXISTS (file)` — **legacy blobs only** |
| `backend_rotate` | `SELECT hash FROM storage.blobs` — **every blob** |

`backend_rechunk` filters in the DATABASE before reading anything, so it only ever
touches the legacy subset. Their read sets therefore overlap on that subset alone,
and a shared walk could save at most its size — never the whole store.

Which makes the optimisation self-defeating: the legacy set is precisely what
`backend_rechunk` consumes, so the available saving shrinks to zero exactly as the
migration it would accelerate completes. On a converged instance the candidate
query returns empty and the job reads nothing, so there is no double-read left to
remove.

Against that: a shared walk would couple two independently pausable jobs onto one
cursor, findings stream and lifecycle — and observability is the reason these are
jobs at all. It is also now a retrofit rather than a design choice.

The residue worth keeping is one sentence of operator guidance, not code: let
`backend_rechunk` converge before scheduling a rotation, so the legacy blobs are
read once as chunks rather than twice.

### 2. Durable deletion intent + a drain job

**2a — Schema and atomic enqueue (one commit).** A migration that adds an
unused table is a half-step; the enqueue lands with it, and at that point
nothing is ever stranded again even before a drain exists.

```sql
CREATE TABLE storage.pending_actions (
    hash            TEXT NOT NULL,
    action          TEXT NOT NULL DEFAULT 'deletion'
                         CHECK (action IN ('deletion')),
    object          TEXT NOT NULL
                         CHECK (object IN ('blob', 'derived', 'attached')),
    entry_name      TEXT,
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    requested_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TIMESTAMPTZ,
    last_error      TEXT,
    parked_at       TIMESTAMPTZ,
    PRIMARY KEY (hash, action)
);
```

Enqueue is therefore `ON CONFLICT (hash, action)`, and every read that means
"is this object queued for deletion" must say
`WHERE hash = $1 AND action = 'deletion'` rather than matching on `hash`
alone — a bare-`hash` predicate is correct today and silently wrong the day a
second action exists, which is the kind of latent break worth spending a
composite key to avoid.

No FK to `storage.blobs` — the row being gone is the entire point.

**Generalised name and the two discriminators — decided, with the reasoning
recorded so it is not revisited blindly.** `action` carries one value today,
and deletion is structurally the only per-blob operation that *needs* a
queue: rotation (`storage-key-rotation.md:260`) and `backend_migration`
retry by re-walking `storage.blobs`, which works because the row survives
the operation and idempotency makes a second pass free. Deletion is the case
whose completion destroys the record of the work, so the work list has to
live outside the table it empties.

The column is kept anyway because renaming a table later costs a migration
with data while naming it right now costs nothing, and because being wrong
about a name is cheaper than being wrong about the columns. The `CHECK`
constraint keeps it honest: a second action has to be added deliberately, and
whoever adds it is forced to look at whether the drain's semantics actually
transfer — deleting live bytes and losing the only copy mid-rewrite are not
the same hazard, and a shared drain loop is where that distinction would get
lost.

`object` is the axis that will actually vary. Every plausible addition is
still a *deletion*, of something other than a base blob: the derived tier
(`purge_derived_blobs`, which strands bytes the same way), the attached tier,
and the objects left on a decommissioned entry after a cutover.

**The PK is `(hash, action)` — and NOT `(hash, object)`.** Two separate
decisions, each with its own reason.

*`action` is in the key* because otherwise the column is future-proofing that
does not work. With `hash` alone the table holds exactly one action per
object, so the first time a second action type exists, enqueueing it for a
hash that already has a pending deletion would either clobber that deletion
(`ON CONFLICT DO UPDATE`) or be silently dropped (`DO NOTHING`) — a
multi-action table with a single-action key. Cheap to get right now,
expensive later.

*`object` is NOT in the key* because the tiers dedup to shared blobs: the same
bytes referenced as a base blob and as a derived artifact are ONE object in
the backend with one hash. Keying on it would create two rows for one object
and therefore two unlinks, the second a no-op — harmless but wasteful, and it
would break the useful property that one unlink settles the hash however many
tiers released it. So `object` records the ORIGIN of the request — useful for
findings and for draining one tier at a time — and is not part of identity.

The consequence for §2f: chunk settle cancels with
`DELETE … WHERE hash = $1 AND action = 'deletion'`, and takes its
`FOR UPDATE` on the same predicate. It must cancel *the deletion* specifically
rather than every pending action for that hash — a future action on the same
object is not something an upload has any business discarding.

Note this is orthogonal to the resurrection race. That race is a queued
deletion against a **live synchronous upload**, not two queued rows; creation
is not and should not become a queued action (durability-first, then the row —
see §2e). The key shape neither causes nor prevents it.

#### The ordering invariant: this is a dirty set, not a log

`delete H` then `create H` and `create H` then `delete H` have opposite correct
end states, so it is worth being explicit about why the queue needs no ordering
to get both right — and about what would break that.

**The queue holds hints, not state transitions.** A row does not mean "delete
these bytes"; it means *"H was last observed at refcount 0 — go check, and if it
still is, unlink."* The authority on whether H should exist is never the queue,
it is `storage.blobs` / `storage.chunk_manifests`. Three consequences:

* **Order cannot matter, because nothing is being replayed.** The drain re-reads
  the truth under the lock (the mandatory re-verify, §2b), so whatever sequence
  of events actually occurred, it acts on the *current* refcount. `create` then
  `delete` leaves refcount 0 and the unlink proceeds; `delete` then `create`
  leaves a live reference and the row is discarded. Same queue state, opposite
  outcomes, both correct — and derived from the DB, not from row order.
* **A falsified hint is discarded, not deferred.** This is why §2f cancels the
  row rather than sequencing behind it: once a reference exists, the intent is
  not "not yet", it is *wrong*, and executing it later in any order would delete
  live bytes.
* **Content-addressing is what buys this.** H's bytes are a pure function of H,
  so `create H` and `delete H` are not writes of competing values — they are
  refcount transitions on one immutable identity. There is no lost-update
  question, hence no ordering question. A mutable store would not have this
  luxury.

So the table is a **dirty set** — the same shape as `storage.search_index_dirty`
and `storage.tree_etag_dirty`, which converge rather than replay — and not a
write-ahead log.

One caveat, so the claim is not overstated: `entry_name` is the single field that
is *not* re-derivable at drain time, since nothing records which backend a hash
was reaped from. That is precisely why it is nullable and why the cutover
procedure drains the queue first (see its column note) — the field is a
best-effort breadcrumb for an operator, not an instruction the drain depends on.
It is a small, known exception to "everything is re-verified", and the reason the
cutover step is an operational requirement rather than a nicety. That is also why `requested_at` is load-bearing for *fairness*
only (oldest first, so nothing starves; a cursor bound, which §2b establishes
carries no correctness) and must never be promoted to an ordering key:
PostgreSQL's `now()` is transaction-start time, so it is neither commit order
nor causal order, and a row can surface *behind* a point the drain has already
passed. `clock_timestamp()` does not fix it, and the repo already carries an open
defect on app-clock vs DB-clock skew in job timestamps.

**The admission test for any future action**, which is the real reason to write
this down: *can it be re-verified against DB truth at drain time?* If yes it is
a hint, it is order-free, and it fits this table as-is. If no — a `rewrite`,
`reencrypt`, or `move to entry X` describes work that is not implied by any
current row, so it is a log entry, and then order becomes load-bearing and this
design is **insufficient**: it would need a monotonic sequence (not a
timestamp), per-object total ordering, commit-order gap handling, and defined
squash semantics for non-commuting pairs. That is a design, not a column. The
`CHECK (action IN ('deletion'))` constraint exists to force whoever wants that
to notice they are signing up for it.

Column rationale, since a migration is the hardest thing to revise:

| column | why it earns its place |
|---|---|
| `entry_name` | **The one that is easy to miss.** GC unlinks via the *active* backend. If a storage cutover happens between enqueue and drain, a queued hash names an object on the OLD entry; draining against the active one either fails or deletes from the wrong bucket. **Nullable**, and deliberately so — see below: NULL means "drain via the active backend", because nothing can report the entry's name at enqueue time. `NOT NULL` is reachable only after that prerequisite. |
| `attempts`, `last_attempt_at` | per-item backoff, and the input to parking |
| `last_error` | without it a parked entry is a mystery at 3am |
| `parked_at` | the permanent-failure case above |
| `size_bytes` | lets the backlog report *reclaimable bytes*; the sweep already holds it |
| `requested_at` | drain ordering, and oldest-age is the health signal |

The enqueue folds into the existing delete, so atomicity costs nothing:

```sql
WITH reaped AS (
  DELETE FROM storage.blobs WHERE ctid = ANY(…) RETURNING hash, size
)
INSERT INTO storage.pending_actions (hash, action, object, size_bytes, entry_name)
SELECT hash, 'deletion', 'blob', size, $3 FROM reaped
ON CONFLICT (hash, action) DO NOTHING;
```

**On conflict, keep the existing row — `DO NOTHING`, not a reset.** The
intent is idempotent ("H is at refcount 0, unlink it"), so a second enqueue
carries no information the row does not already hold, while resetting
`attempts` / `last_error` / `parked_at` actively destroys state that matters:

* It **defeats the backoff, by resetting it** — and does so precisely under
  churn, which is when backoff is needed. A hash re-reaped repeatedly would have
  its attempt count zeroed each time and retry at full rate forever. That is the
  same shape as the §3 defect (retries accelerating instead of backing off)
  reintroduced inside the new table.
* It **silently un-parks**, which is the house rule inverted: parked means a
  human decided nothing more should be attempted automatically, and clearing it
  as a side effect of unrelated churn is exactly the silent auto-repair this
  repo avoids elsewhere. Un-parking belongs in an explicit operator action.
* It **nulls `last_error`**, the column whose whole justification is that
  "without it a parked entry is a mystery at 3am".

And the clean-slate case that seemed to justify the reset — *a hash parked, then
re-uploaded, then dereferenced again* — **already gets a clean slate without it.**
A re-upload cancels by *deleting* the row (§2f), so the later re-reap inserts a
genuinely new row with `attempts = 0` by construction. The only path that should
clear the bookkeeping is the only path that already does. Nothing else has any
business clearing it.

**`entry_name` is nullable — decided.** The plan originally specified
`NOT NULL`, which cannot be implemented as written: `DedupService` holds a
hot-swappable backend stack (`AppState.blob_backend_hot_swap`) that a cutover
replaces at runtime, and neither the stack nor its wrapper reports the entry
it was built for, so there is no honest value to write at enqueue time.
Making it `NOT NULL` would mean writing a guess.

NULL therefore means **"drain through whatever backend is active"**, which is
precisely today's behaviour and so introduces no regression. The column
exists now because adding it later is cheap while backfilling it is not — an
existing row's entry would be unknowable.

**What makes NULL safe, and when to revisit.** The ambiguity only bites if a
cutover lands *while the queue is non-empty*: a hash enqueued against entry A
would then be drained against entry B, either failing or — worse — deleting
from the wrong bucket. That is avoidable operationally rather than
structurally: **drain the queue to empty as part of the cutover procedure**,
which fits where `backend_migration` already freezes writes (see
`storage-multi-entry.md` on readonly/engage semantics). A drained queue has
no ambiguous rows, so NULL stays correct indefinitely.

So the trigger to revisit is specific: either cutovers start happening
without a drain step, or the backlog is routinely deep enough that draining
it first is impractical. Until one of those is true, populating the column
would require a prerequisite (teach the backend stack to report its current
entry) whose only payoff is removing a step from a procedure that should have
it anyway.

The existing best-effort unlink then stays exactly as written — it simply
deletes the queue row on success instead of shrugging on failure.

**2b — The drain job, `backend_reclaim`.** Scheduled, budget-bounded per tick, per-item
exponential backoff keyed on `attempts`, parking past a limit. Idempotent
and incremental, which is what makes it *safe to schedule* — unlike a full
sweep. Deleting an object that is already absent is success, not an error.

The name puts the subject first like its siblings (`backend_rotate`,
`backend_migration`, `backend_consistency`) — the subject is the storage
backend, because backend I/O is all this job does. `reclaim` over `purge` or
`unlink`: it describes the outcome an operator cares about (bytes come back)
rather than the syscall, and it stays distinct from `dedup_gc`, which is the
DB-side half. The division of labour is worth stating plainly, since two jobs
now participate in one deletion: **`dedup_gc` decides what is unreferenced and
enqueues; `backend_reclaim` unlinks bytes and clears the row.** Neither does
the other's work, and `dedup_gc` stops touching the backend entirely — which
is what turns today's best-effort unlink into something with a retry.

**Run shape — a cursor taken at launch, new arrivals wait for the next run.**
The run records a boundary when it starts, then pops rows at or below it
(oldest `requested_at` first, `parked_at IS NULL`, bounded by the tick budget),
deleting each as it settles. Anything the engine pushes mid-run sits above the
cursor and is invisible to this run; when the run terminates it may re-arm
immediately if work remains, rather than waiting for the next tick.

A **cursor, not a materialised batch** — the distinction matters for more than
style. A batch selected into memory at launch is lost the moment the run
pauses, whereas a cursor is exactly what `RecoverableJobHandler` already
checkpoints, so pause/resume and the cancel poll work with no extra machinery
and the run never holds the candidate set in memory. This is the same shape
every other recoverable job here uses.

Two reasons this is the right default. The work per run is bounded and
predictable, which is what a budgeted job needs; and the drain never chases a
moving target, so a busy instance cannot starve the run into never finishing —
**termination is what the cursor buys.**

**The cursor is a fairness device, not a correctness device**, and that is the
property that makes it safe to be sloppy about. Because a settled row is
*deleted*, nothing marks a row as "seen", so a row this run misses — committed
after the scan passed its position, or with a `requested_at` from a
transaction that started before the cursor was taken and committed after (`now()`
is transaction-start time, so timestamp order is not commit order) — is simply
still there next run, under a fresh and larger boundary. The worst case for a
mis-ordered row is a delayed reclaim, never a permanent skip. Contrast a
keyset cursor over an append-only table, where passing a row means losing it.

**Squashing within an action happens at enqueue, by the key.** With
`PRIMARY KEY (hash, action)` there is at most one row per object per action, so
two tiers releasing the same hash collapse into one row on insert and one
unlink settles both — structural, not a step the run performs.

**Squashing across actions is a different problem, and the key does not solve
it.** Two rows for one object (`(H, 'deletion')` and a future `(H, 'rewrite')`)
are separate rows, so nothing collapses them and nothing serialises them either
— see the per-object locking note below, which is where that has to be handled.
It costs nothing today because there is one action; it is not merely an
optimisation the day there are two.

**A stop is a re-plan opportunity, and that is where the optimisations live.**
A run that pauses, is cancelled, or is killed resumes by taking a *fresh*
boundary and re-planning over whatever the queue now holds, rather than
restoring the old one. Safe by the paragraph above — the cursor carries no
correctness — and it is what makes three optimisations available that a fixed
in-memory batch would foreclose:

* **Batch the backend calls.** S3 `DeleteObjects` takes up to 1000 keys per
  request and Azure has its own batch form, so a re-plan can coalesce the
  popped rows into bulk calls. On a remote backend this is the dominant cost by
  a wide margin — a thousand round trips collapsing into one — and it is
  reachable only if the run is free to group rows at plan time instead of
  walking a pre-committed list one at a time.

  The port cannot express this today — `blob_storage_ports.rs:203` has only
  single-hash `delete_blob`. So it needs a `delete_blobs(&[&str])` alongside it,
  as a **default trait method that loops over `delete_blob`** per the house rule
  (`AGENTS.md` § Code duplication: shared port behaviour → default method). Every
  backend then keeps working unchanged and S3 overrides it with `DeleteObjects`.
  Worth treating as optional for the first cut: the queue is already a
  correctness fix without it, and a bulk delete whose partial-failure reporting
  is mishandled would undo the retry guarantee the queue exists to provide —
  `DeleteObjects` returns per-key errors in a 200 response, so "the call
  succeeded" is not "the keys are gone".
* **Bulk pre-filter before paying for locks.** One anti-join against
  `storage.blobs` and `storage.chunk_manifests` drops every hash that has since
  become referenced again, so the run does not take a row lock and a re-verify
  per obviously-live entry. Cheaper the longer the queue has been waiting,
  which is exactly the backlog case.
* **Re-squash.** Free today, since `PRIMARY KEY (hash, action)` means the
  insert already collapsed duplicates — but the hook belongs in the plan step
  so it stays free to add if the key ever widens.

Batching does move the lock granularity off pure per-item: lock a window of N
rows `FOR UPDATE`, re-verify them, issue one bulk delete, delete the rows,
commit. That is a **tunable**, not a reversal of the rule above — the objection
was to holding locks for a whole *run*, and a window of ~100 rows held for one
round trip is a different trade. Contention stays confined to the hashes in
that window, so an upload collides only if it needs one of those exact
hashes — vanishingly unlikely for content-addressed chunks, and correct rather
than merely unlikely because it then simply waits.

**The drain is concurrent, and the mechanism is `SKIP LOCKED`, not sharding.**
Nothing about the design is inherently sequential, and the backends take
parallel calls by construction — every layer is `&self` returning `BoxFut`, and
`DedupService` already runs bounded windows in four places
(`CHUNK_UPLOAD_CONCURRENCY = 8` at `dedup_service.rs:1408`, `VERIFY_CONCURRENCY = 16`
and `VERIFY_MANIFEST_CONCURRENCY = 8` at `:3152`, plus a generic window helper
at `:484`). Note in particular that **the very loop this plan is fixing is
already concurrent**: the best-effort unlink at `:3611` sits inside
`buffer_unordered(…)` at `:3617`. So the drain inherits an established pattern
and should be concurrent from the first cut. On a remote backend it is the
difference between sequential RTT-bound throughput — ~10-20 deletions/s at
50-100 ms — and something that clears a post-outage backlog.

**Hash could shard the work, but it should not.** Sharding by
`hashtext(hash) % N` is *static* partitioning: shard assignment is fixed before
the work is known, so a shard that happens to hold the failing or parked rows
idles while its neighbour is still working, and the worker count has to be
decided up front. `SELECT … FOR UPDATE SKIP LOCKED` gets the same parallelism
as dynamic work-stealing — workers pop disjoint rows, self-balancing, no shard
key, no rebalancing, no coordination. It appears nowhere in the codebase today,
so it is a deliberate addition rather than an existing convention.

It also composes with §2f in a way that is worth having on purpose: the drain's
pop uses `FOR UPDATE SKIP LOCKED`, so a row an uploader is mid-cancel on is
**skipped rather than waited on**. The uploader wins the race, the drain moves
straight to the next row, and the skipped row is simply gone by then — or
picked up by a later run, which is safe precisely because the cursor carries no
correctness. Plain `FOR UPDATE` would instead stall a drain worker behind a
user transaction, which is the coupling this whole section is trying to remove.

Where hash genuinely matters is the one place it is already handled: S3 scales
request rate per key prefix, so a workload hammering one prefix throttles. Blob
keys are content-hash-derived and therefore uniformly spread across the
keyspace by construction — the prefix spreading that S3 guidance asks for is
free here, and needs no deliberate sharding. Recorded so nobody adds sharding
later for a throughput reason that does not apply.

Priority among the three, since they are often conflated: **batching (~1000×)
beats concurrency (~8-16×) beats sharding (~1×, given `SKIP LOCKED`).** And
concurrent per-item transactions want the maintenance pool rather than the user
pool (`infrastructure/db.rs`), which is exactly what that split exists for.

It does make the **schedule mandatory** rather than nice-to-have: with
new arrivals deferred to the next run, an on-demand-only job leaves fresh
entries waiting indefinitely, which is precisely today's `dedup_gc` failure.
The re-arm is a catch-up path for a backlog, not a substitute for a schedule,
and it needs a guard: re-arm only when the previous run actually settled rows.
Re-arming on a run that achieved nothing would spin against a queue whose
entries all fail and have not yet parked.

**Locking is per OBJECT, not per run — and "per object" is the load-bearing
word, not "per row".** The cursor is for scheduling and termination; the lock
is for correctness, and they belong at different granularities:

* A run-wide lock — taking every row below the cursor `FOR UPDATE` for the duration
  — would block chunk settle's cancel (§2f) for the whole run. With a slow
  backend and a deep backlog that means uploads stalling behind the drain,
  reintroducing exactly the foreground-latency coupling that moving unlinks
  off the critical path was meant to remove.
* So: pop under the cursor without a run-wide lock, then **one short
  transaction per object** (or per batch window, above) —
  `SELECT … FOR UPDATE` its row, re-verify, unlink, delete the row, commit. An
  uploader then contends only on the specific hash it needs, and only for one
  unlink's duration.

**The unit of exclusion is the object, and today the row lock is one only by
coincidence.** With a single action type there is at most one row per hash, so
locking `(H, 'deletion')` *is* locking H. That equality breaks the moment a
second action exists: PostgreSQL row locks are per row, so a worker holding
`(H, 'deletion')` does **not** block another from popping `(H, 'rewrite')` —
`SKIP LOCKED` skips the locked *row*, not the hash. Two workers would then act
on the same backend object concurrently, in an order neither chose. That is a
data-loss shape, not a performance wrinkle.

So the invariant to state now, while it is free, is: **a hash is processed by at
most one worker at a time, whatever number of rows it has.** Today's per-row
lock satisfies it. Whoever adds action #2 must keep it satisfied, by one of:

* **A per-hash advisory lock** — `pg_try_advisory_xact_lock(hashtext(hash))`,
  taken before the row work. This is the right tool for mutual exclusion on an
  entity whose row count may vary, the `try_` form gives `SKIP LOCKED`'s
  move-along-on-contention behaviour, and it releases at commit with no
  bookkeeping. Cost: `hashtext` is 32-bit, so unrelated hashes can collide into
  false contention — harmless, since the loser only defers to a later run, but
  worth knowing when a metric shows unexplained skips.
* **Popping by hash rather than by row** — take every action for one hash in a
  single transaction and apply them in a deterministic order. This is the
  version that also settles *what* the combination means rather than only who
  goes first, which is the harder half: "delete" and "rewrite" against one
  object do not commute, so their relative order is a semantic decision, not a
  scheduling one. Note `WHERE hash = $1 FOR UPDATE SKIP LOCKED` does not by
  itself achieve this — it would silently return the subset not locked, handing
  a worker a partial view of the object's pending work, which is worse than
  blocking.

This is also the point where cross-action **squash** stops being a no-op and
becomes the mechanism rather than an optimisation: holding every action for an
object is what lets the drain collapse them deliberately (a pending deletion
supersedes a pending rewrite) instead of racing them. The key collapses
duplicates *within* an action; only popping by hash collapses *across* actions.

**The re-verify inside the per-item lock is not optional.** A snapshot taken
at launch is a stale read by the time item N is processed: an uploader may
have resurrected that hash in between. Re-checking `storage.blobs` (and
manifest references) after taking the row lock is what makes the snapshot
safe, and it is the same check §2f requires. Without it the snapshot model is
precisely the stale-read hazard.

**"Only one job runs at a time, so no lock is needed" — does not hold, and is
the most natural wrong turn here.** Single-job serialisation removes
drain-vs-drain contention, which was never the problem. The counterparty in
§2f is a **user upload**: an ordinary `POST /api/files/upload` served while
the job runs. Nothing freezes the request path during a GC or drain —
`backend_migration` has a readonly mode precisely because it needs one, and
the drain deliberately does not. So job-level mutual exclusion says nothing
about the race that matters.

The unlink is a network call and cannot be inside a transaction with the
uploader's insert, so there are only three options: hold a lock across it,
make the losing race recoverable, or keep the window small enough not to
matter. The third is what today relies on (`GC_ORPHAN_GRACE_SECS` covering a
sub-second gap) and is exactly the property a deferred queue destroys. The
second is unavailable, because the uploader keeps no copy of bytes it did not
write (idempotent-skip PUT). Hence a lock.

The cost is small enough not to argue about: one row, held for the duration of
one backend DELETE, contended only by an upload that needs that exact hash. If
transactions spanning network I/O ever become a problem, the replacement is a
**lease** — claim the row with an expiry and release it after the unlink. Chunk
settle's trust rule is unchanged and needs no knowledge of claims, since it keys
on the absence of a live `storage.blobs` row rather than on the queue (§2f); the
lease only has to make the uploader *wait out* a live claim instead of blocking
on a row lock. Same guarantee, no transaction held across I/O, more moving parts
and an expiry to tune. Start with `FOR UPDATE`.

**On squashing: it happens on write, and is never a step the drain performs.**
`PRIMARY KEY (hash, action)` admits one row per object per action, so a repeat
enqueue is `DO NOTHING` against a row that already states the intent, and the
two tiers releasing one hash collapse into a single unlink. The complementary
half is that a re-upload does not insert a competing row either — it *deletes*
the deletion (§2f). So the table holds only the current intent per
`(hash, action)` at all times, maintained by whoever writes: the reaper inserts,
the uploader deletes, nobody reconciles. That is the same property as the dirty
sets it is modelled on, and it is what makes the drain order-free.

Cross-action squash is the one case this does not cover, because two rows for one
object neither collapse nor serialise on their own — see §2b, where the
per-object lock has to handle it, and where the collapse rule stops being obvious
since "delete" and some future "rewrite" do not commute.

This also closes a gap found on the way: `dedup_gc` is registered
`None, // on-demand` and nothing schedules it, so on any deployment where
nobody clicks it, reclaimable bytes accumulate forever. On S3 that is
unbounded billable storage.

**2c — Make the backlog visible.** Depth, oldest `requested_at`, parked
count and total `size_bytes` in the job's `extra_stats`; a growing or ageing
backlog becomes a finding in its own right. Without this we would have
swapped invisible orphaned bytes for an invisible backlog — recoverable,
but no more observable than before.

**2d — `backend_consistency` repair arm** that enqueues discovered orphans
(see Settled decisions). This is the only route by which the 29 existing
ones become reclaimable — and, per 2e, the permanent recovery path for the
creation side too, not a one-off cleanup.

**2e — The creation side has the same window, and it cannot be closed.**

The live write path is bytes-then-row: `store_from_stream` writes to the
backend, then inserts the `storage.blobs` row. `backend_consistency` names
this in its grace-window comment — "the write path is
durability-before-visibility, so bytes exist briefly before their row does.
Without this every in-flight upload reads as an orphan."

That is the deletion bug mirrored, and the difference is only frequency.
Deletion's second step is a network call that fails routinely, which is how
29 accumulated. Creation's second step is a local DB insert that usually
succeeds, and when it merely *errors* the caller can compensate by releasing
what it just stored. The irreducible case is a **crash or kill between the
two**, where no compensation code runs at all.

It cannot be fixed by reordering: row-first is precisely what the dead
`register_file_deferred` write-behind path does, and it trades stranded bytes
for a row pointing at bytes that never arrived — `inconsistent` traded for
`data_loss`. Bytes-first is the right call and the residue is its price.

Two consequences:

* The fsck is **permanent infrastructure**, not a migration aid. Creation-side
  crash residue will keep appearing at some low rate forever, so on a remote
  backend the bucket walk wants a periodic schedule, not only an admin button.
* **Verified: the upload path does compensate.** `save_file_with_blob_impl`
  calls `self.dedup.remove_reference(blob_hash)` on three distinct error
  paths (`file_blob_write_repository.rs:298`, `:366`, `:376`), releasing the
  just-stored blob when the row cannot be written. So the error case is NOT
  stranding bytes, and creation-side residue really is crash-only — an
  accepted window rather than a bug. This is what keeps §2e a design note
  instead of a fifth work item.

  One residual: the compensation can itself fail, and the code logs
  `rollback_err` and moves on. That leaves a stranded blob, but it needs both
  the insert and the release to fail in the same request, so it belongs with
  the crash case — rare, and recoverable only by the sweep. Which is the
  argument for the sweep again.

Worth noting the dead write-behind path is itself a hazard: it is implemented
in the repository, the port, the stubs and a test, with no production caller
and nothing in `di.rs`. Anyone wiring it up later would silently adopt the
`data_loss` ordering. Either delete it or document why it is parked.

#### A sustained backend outage on the creation side — verified, and why it stays unqueued

The deletion queue raises the obvious symmetry question: if a failed unlink
earns a durable retry, why not a failed write? Traced through the code, the
answer is that creation is already correct and **must not** be queued.

What happens today, from outside in:

* `TimeoutBlobBackend` (`di.rs:362`) bounds the hang, and `RetryBlobBackend`
  (`di.rs:387`) retries — 3 attempts, 100 ms doubling to a 10 s cap — gated on
  `DomainError::is_transient()` (`Timeout | TransientBackend`, `errors.rs:205`).
  A blip is absorbed invisibly.
* The cache cannot mask a failure: it is strictly write-through and **rolls its
  own entry back** when the inner put fails — *"Never serve a blob the backend
  rejected"* (`cached_blob_backend.rs:192-206`). Nothing local ever claims a
  durability it does not have.
* Past the retries, the error propagates and **the upload fails with a real
  status, not a generic 500.** `ErrorKind::TransientBackend` maps to **503
  Service Unavailable** and `Timeout` to **408** (`interfaces/errors.rs:133`,
  `:145`), and the kind survives the whole way out: the ingest path returns
  `Err(AppError::from(e))` (`upload_ingest.rs:249`) without flattening it. So
  the client can distinguish "storage is down, retry later" from "your request
  was wrong" — which is what the `error_type` contract needs.
* A multi-chunk CDC upload that dies partway does not strand invisible bytes:
  `IngestGuard` (`dedup_service.rs:219`) releases the chunk pins it took and
  registers the chunks it did write as `storage.blobs` rows at `ref_count = 0`
  with `orphaned_at = now()`, precisely *"so the existing GC sweep can reclaim
  the bytes — a backend file with no PG row would be invisible to it"*
  (`:287-292`). `disarm()` on success, `rollback()` for handled errors, and a
  spawned rollback from `Drop` for the rest.

**Why a creation queue would be a mistake, not a symmetry.** Two reasons, and
the second is decisive:

* **Deletion has no one left to report to.** By the time the unlink fails the
  user's delete has already returned success and the file is out of their view —
  there is no request left to fail, so an unretried failure is simply *lost*.
  Creation still has someone on the wire, and a 5xx puts the retry where it
  belongs. **The client is a better retry authority than any server queue,
  because it still holds the payload.**
* **A deletion intent is 32 bytes; a creation intent is the file.** Queueing a
  write means durably holding the payload until the backend returns — and the
  only durable store for payload is the backend that is down. That requires a
  local spill area, which is a write-behind cache: the parked
  `register_file_deferred` path above, acknowledging a write that is durable
  nowhere it claims. Trading `inconsistent` for `data_loss` again.

#### Outage *and* restart together — what is left behind, and is it visible

The worse case is an outage during which the process also dies, because then no
compensation code runs at all. Worth tracing exactly, since it is the one
combination where nothing in the request path can help.

Chunk creation, as `ingest_chunks_from_stream` documents it (`:1995-2013`):

1. FastCDC splits the stream (`AsyncStreamCDC`, `:2026`).
2. Per batch, `pin_claimable_chunks` (`:1756`) issues **one** `UPDATE storage.blobs
   SET ref_count = ref_count + 1` that checks and bumps together for chunks the
   store already has — "no check-then-bump TOCTOU window". Their bytes are dropped
   from RAM with no I/O. The pinned hashes are recorded on the guard.
3. Remaining hashes are new, written to the backend **unsynced** at concurrency 8
   (`:2261-2269`), recorded in `guard.written`.
4. At end of stream, **one** `sync_blobs` makes them durable (`:2165`), then **one**
   batched INSERT registers them at `ref_count = 1` (`ON CONFLICT … ref_count + 1,
   orphaned_at = NULL`, `:2172-2178`). Durability before visibility.
5. `guard.disarm()` (`:2191`); the manifest and file row are written by the caller
   *after* this returns.

A kill anywhere before step 5 therefore leaves **no file at all** — the manifest
that would make it visible is written later, so the user sees a failed upload, not
a half-file. That is the important property, and it holds.

Two kinds of residue are left, and they are genuinely different:

| residue | what it is | detected by |
|---|---|---|
| **orphaned pins** | `ref_count` bumps from step 2, committed standalone, with no manifest to justify them. Not stranded bytes — an inflated count on chunks that already existed, which stops them ever being reclaimed | `blobs_consistency` — it recomputes `actual_ref_count` from real references (files without manifests, plus manifests containing the chunk) and emits `refcount_mismatch` with `stored` / `actual` / `delta`, and has an opt-in repair arm |
| **written-but-unregistered chunks** | step 3/4 bytes on the backend with no PG row | only `backend_consistency`'s bucket walk — the same blind spot as the 29 |

Two things follow that are worth stating plainly:

* **In an outage specifically, the residue is almost entirely pins.** The PUTs were
  the thing failing, so little or nothing landed in the second row of that table.
  And pins put no bytes at risk — the chunks they inflate already existed and are
  still referenced by whoever put them there. The cost is reclaimability, not data.
* **The irreducible window is narrow and precisely locatable**: between
  `sync_blobs` returning and the INSERT committing (`:2165` → `:2172`). A kill
  there leaves durable bytes with no row. Closing it needs the row-first ordering,
  which trades this for `data_loss` — so it stays open, by the same argument as
  the rest of §2e.

**The real gap is not detection, it is that nothing runs.** Both detectors exist
and both are adequate; neither is scheduled. `OXICLOUD_STARTUP_JOBS` defaults to
the thumbnail migrations, so after an outage-plus-restart the residue sits
undiscovered until an admin thinks to click something. A crash is exactly the
moment a reconciliation pass is warranted, and it is exactly when nobody is
watching — which is the schedule argument from §2b arriving from a second
direction, and an argument for putting `blobs_consistency` in the startup set.

**The asymmetry that actually explains the 29 orphans.** Both paths leak bytes
on failure; they differ in *discoverability*, and that difference is the bug:

| | ends with | found by |
|---|---|---|
| creation failure | a PG row at `ref_count = 0` | `dedup_gc` — DB-driven, runs today |
| deletion failure | **no PG row at all** | only `backend_consistency`, a bucket walk with no schedule |

Creation deliberately makes its residue *visible* so the existing reclaimer
finds it. Deletion deletes the row first, so its residue is invisible to the
very job whose purpose is reclaiming — which is why 29 accumulated in silence
while creation-side residue would have been swept. **The queue is how the
deletion path gets the property the creation path already has.** That framing
is the cleanest statement of what §2 is for.

**2f — The resurrection race, and why a lock is required.**

This is the correctness heart of the design, and the queue makes it **more**
dangerous than the status quo rather than less. It must be settled before any
code lands.

The scenario: chunk `H` is reaped — `storage.blobs` row deleted, bytes still
on the backend, `pending_actions` row created. Before the drain runs, a user
uploads a file (new or identical) that chunks to include `H`.

What the uploader does today (`dedup_service.rs:289-307`): durability first,
then `INSERT … ON CONFLICT (hash) DO NOTHING` at `ref_count = 0`. The comment
there already names our failure mode — "a backend file with no PG row would
be invisible to it". So the uploader creates a fresh row for `H`, the manifest
pins it, and the file is live.

**And `pending_actions` still holds a row for `H`.** The drain then deletes
the bytes out from under a live file: row present, bytes absent —
`blob_missing_from_backend`, severity `data_loss`. Exactly the outcome this
plan exists to avoid, arrived at from the opposite direction.

Two details make it sharper than it first looks:

* **The window widens enormously.** Today the unlink follows the row delete
  within the same sweep, seconds later, and `GC_ORPHAN_GRACE_SECS` covers it
  — its comment says so: it keeps "a concurrent uploader that is about to pin
  a just-orphaned chunk from racing the row-delete → file-unlink gap". A queue
  deliberately defers the unlink, possibly for hours. Grace stops being a
  sufficient mitigation the moment the unlink is asynchronous.
* **The uploader cannot repair it.** `put_blob_from_bytes` is
  idempotent-skip (`O_CREAT|O_EXCL` on Local — see the warning in
  `backend_rotate_service.rs:410`), so an uploader that finds the bytes
  already present writes nothing. It is *relying* on bytes the drain is about
  to remove, and holds no copy afterwards.

**Correction to an earlier claim in this plan.** I argued the side table
"avoids the resurrection race entirely" because `storage.blobs` is untouched.
That is wrong: it *relocates* the race rather than removing it. With a state
column the uploader would at least SEE the marked row and could clear the mark
and take a reference in one atomic `UPDATE`; with a side table it must
remember to look in a second place. Both designs need the same serialisation.

The side table is still preferred, but for a narrower reason than stated: a
marked row is still a row, so every existing `NOT EXISTS` guard and
consistency predicate would have to learn to distinguish "live" from "marked",
whereas the side table leaves all existing paths behaving exactly as today and
confines the new logic to two places — the drain and chunk settle.

**Resolution: serialise on the deletion row for that hash.** It is the natural
mutex, being the one key both parties care about. Strictly the mutex is *the
object*, and the row is how it is expressed while deletion is the only action —
see §2b on keeping that true if actions multiply.

* **Chunk settle**, in the same transaction as the row insert:
  `SELECT … FOR UPDATE` the queue row, then `DELETE FROM pending_actions
  WHERE hash = $1 AND action = 'deletion'` — cancelling the deletion, and only
  that. An upload has no business discarding some future action queued against
  the same object.
* **Drain**, per item: `SELECT … FOR UPDATE` the queue row, re-verify no
  `storage.blobs` row and no manifest reference exists, unlink, then delete
  the queue row — all inside that transaction.

Uploader first: the queue row is gone before the drain can claim it, so the
bytes survive. Drain first: it holds the row lock, and the uploader blocks until
the unlink has completed and the queue row is gone — **which is not yet a clean
outcome, and the next two paragraphs are why.**

**`SKIP LOCKED` belongs on the drain's side only — never the uploader's.** The
two directions want opposite behaviour on contention, and the reason is
asymmetric: the drain has a thousand other rows it could usefully do instead, so
skipping costs it nothing and a skipped row is simply picked up later. The
uploader has no alternative work — it needs *this* hash — and skipping would
mean proceeding as though the object were safe while a drain is committed to
unlinking it, which is the data-loss outcome this whole section exists to
prevent. So: drain pops with `FOR UPDATE SKIP LOCKED`, chunk settle takes plain
`FOR UPDATE` and waits. The wait is bounded by one unlink.

**Two defects the lock alone does not fix**, both in the drain-first ordering.
Walk it concretely — chunk H hits refcount 0, the reaper deletes its
`storage.blobs` row and enqueues `(H,'deletion')` atomically, and before the
drain runs a user re-imports a file that chunks to include H:

1. **The `_replace` trigger is wrong.** The rule "when chunk settle finds a
   `pending_actions` row, distrust the backend copy" never fires in the case that
   needs it: if the drain won the lock it has already *deleted* that row, so the
   uploader wakes to find nothing and takes the ordinary idempotent-skip path
   against bytes that were just unlinked. Its row then points at nothing —
   `blob_missing_from_backend`, data_loss. Finding no row is exactly as
   dangerous as finding one.
2. **The PUT is on the wrong side of the lock.** Durability-first puts the write
   *before* the transaction, so a drain holding the lock can unlink *after* the
   uploader's write — destroying even a `_replace`. And by then the uploader may
   no longer hold the bytes to retry with, since idempotent-skip means it never
   kept a copy of what it did not write.

**The root cause is that the trust decision is keyed on the wrong authority.**
`put_blob_from_bytes` skips when *the backend object exists* — but "backend
object present with no PG row" is precisely the orphan / pending-deletion state.
The authority must be the **PG row**, and one invariant makes that sound: the
reaper deletes the `storage.blobs` row and inserts the queue row in a single
statement, so *a live blob row and a pending deletion for the same hash are
mutually exclusive.* Therefore:

* **Live PG row (or manifest reference) for H** → refcount > 0, so no deletion
  can be pending or become pending while the reference is held → skip the write
  and take a reference. Unchanged common path, and it needs no queue lookup and
  no backend exists-probe.
* **No live row** → the backend copy is untrustworthy *whether or not a queue row
  is visible*, and the write must happen inside the critical section:

```text
BEGIN;
  SELECT … FROM storage.pending_actions
    WHERE hash = $1 AND action = 'deletion' FOR UPDATE;   -- waits out a drain
  PUT H  (replace variant, unconditional)                 -- inside the section
  DELETE FROM storage.pending_actions
    WHERE hash = $1 AND action = 'deletion';              -- no-op if absent
  INSERT INTO storage.blobs …;
COMMIT;
```

That closes every interleaving. **Uploader acquires first**: it holds the row, the
drain's `SKIP LOCKED` passes H over, the write lands, the cancel commits, and a
later drain finds nothing to do. **Drain acquires first**: it re-verifies (no blob
row — the uploader has not committed), unlinks, deletes the row, commits; the
uploader then proceeds down the no-live-row branch and rewrites the bytes
unconditionally, and no drain can be racing it because no queue row remains to
pop. **No contention**: genuinely new content, which had to be written anyway.

Note what this fixes beyond the race: re-uploading content that is orphaned on
the backend now *self-heals* it, instead of skipping the write and adopting bytes
of unverified provenance.

The cost is a real PUT on the no-live-row path instead of an exists-probe and
skip — which is close to free, because that path is either new content (writing
regardless) or an orphan (where writing is the correct action, not waste). It
may even be cheaper, since the PG row answers the question that probe was asking.

Holding a transaction across a backend call is not pretty, and it is now
unavoidable on the contested path rather than merely convenient — that is the
price of the uploader needing its bytes durable and un-unlinkable at the same
instant. A **lease** (claim with expiry) is the escape if these transactions
become a problem: same guarantee, no transaction spanning network I/O, more
moving parts. `FOR UPDATE` is the simpler correct starting point.

This is the `reupload-during-unlink` case in Verification, and it is the one
test that must exist before the migration ships.

### 3. Backoff that actually backs off

`RetryBlobBackend` is correct within its own scope: `max_retries: 3`,
100 → 200 → 400 ms. The problem is that this budget is **per call**, and
nothing above it decays. When the three are exhausted the collab debouncer
logs `will retry on next tick` and the next tick starts a fresh budget from
100 ms — so a sustained outage has a dirty document retrying forever at tick
cadence, three DNS lookups a time, which reads as *accelerating* rather than
backing off.

It self-heals and loses nothing, so severity is low; the fix belongs on the
collab flush tick (per-document backoff after consecutive transient
failures), not in the decorator.

**As implemented.** Two fields on the collab actor — `flush_failures` and
`flush_retry_not_before` — and a `flush_due` that returns false while a backoff is
outstanding, whatever the debounce thresholds say. The delay doubles from the tick
interval and caps at five minutes.

Capped rather than unbounded because this must never become "give up": the
document keeps accepting keystrokes that exist only in memory, so the write has to
keep being retried until it succeeds or the actor is evicted. The cap is what
bounds how stale the on-disk copy stays once the backend recovers.

Any success clears it, including a no-op short-circuit — if the path is healthy
enough to compare hashes, it is healthy. And the log line now states the real
delay instead of "will retry on next tick", which was the misleading part: it was
true, and the next tick started a fresh retry budget.

`flush_backoff` is a free function rather than a method so it is testable without
constructing an `ActorState`; the tests pin the doubling, the cap, that the cap
HOLDS for a long tick rather than being an artefact of the exponent clamp, and
that `u32::MAX` failures neither wraps nor panics.

#### And the retry budget is not what it looks like: the SDK retries too

Found while tracing the outage path, and it changes the arithmetic above.
`s3_blob_backend.rs:73` builds the client with `.behavior_version_latest()`,
and in `aws-smithy-runtime`'s default-retry plugin that is decisive:

```rust
let retry_config = if is_aws_sdk
    && behavior_version.is_at_least(BehaviorVersion::v2026_01_12()) {
    RetryConfig::standard()      // ← enabled
} else {
    RetryConfig::disabled()
};
```

`BehaviorVersion::latest()` *is* `v2026_01_12` (`behavior_version.rs:39-41`),
whose own release note reads "enables retries by default for AWS SDK clients".
So the SDK retries underneath `RetryBlobBackend`, which retries on top of it —
**nesting nobody chose**, since `retry_config` is never set on the builder and
the enabling default arrived with an SDK upgrade rather than a decision here.

Consequences worth being precise about:

* The effective budget is **multiplicative**, roughly 4 outer attempts × 3 SDK
  attempts ≈ 12 HTTP attempts per logical blob operation, not the 4 the
  decorator's config implies. An operator tuning
  `OXICLOUD_STORAGE_RETRY_MAX_RETRIES` is tuning one factor of a product.
* It is **not even a stable multiplier**: `RetryConfig::standard()` uses a retry
  token bucket, so the SDK's contribution shrinks under sustained failure —
  exactly the regime an outage creates. Aggregate behaviour is therefore
  load-dependent, which is the worst property for something meant to be
  predictable, and it plausibly contributes to what the outage test showed.
* Only S3 is affected. Azure and Local do not have this layer, so the
  "uniform chain" the decorator exists to provide is already not uniform.

**Recommendation: keep both, but make the division of labour explicit** — and
mirror the decision this file already made one layer down. The timeout comment
at `s3_blob_backend.rs:50-54` says it plainly: the decorator "provides the
configurable outer bound for every backend, while these are the SDK's finer,
per-attempt instruments underneath it." Retry has the same shape, and the same
answer: the SDK is the better *classifier* (it distinguishes throttling from
5xx from a dead connection, and jitters), while the decorator is the better
*uniform outer bound*. So set the SDK's `RetryConfig` explicitly rather than
inheriting it, drop the decorator's count for S3 to a small number so the
product stays near 3-6, and state the intent in a comment next to the timeout
one. What must not stay is the current position, where neither layer knows the
other exists.

Cheap to check before deciding: `RUST_LOG=aws_smithy_runtime=debug` shows the
SDK's own attempts, so the real multiplier is observable rather than inferred.

### 4. The same-content refcount leak (`manifest_refcount_mismatch`) — **DONE**

Root cause is a single line — `file_blob_write_repository.rs:253`:

```rust
// Decrement old blob ref (only if hash changed, best-effort)
if old_hash != new_hash
```

The caller has **already taken a reference**: `write_content` calls
`store_from_stream` (+1) and then swaps (`di.rs:4040`). When the content is
unchanged, `old_hash == new_hash`, the release is skipped, and that +1
stands. **Every same-content rewrite leaks exactly one reference** — which
is precisely the observed `stored: 2, actual: 1, delta: -1`, on two files of
66 and 133 bytes.

The guard reads as deliberate ("nothing changed, nothing to release"), and it
would be right in isolation. It is wrong *given* the caller's increment: the
+1 has to be undone whether or not the hash moved.

Collab reaches it whenever a session re-flushes unchanged text. The
`flush_to_blob` short-circuit key `last_flushed_content_hash` is in-memory
and per-session, so any teardown — restart, idle-GC reap, eviction,
reconnect — loses it, and the next flush rewrites identical bytes through
the leaking path. Any re-upload or WebDAV PUT of unchanged content does the
same; collab just gets there far more often.

Belongs in this plan because it is the *same subsystem pointed the other
way*: a leaked reference means a blob never reaches zero, so it is never
queued for deletion at all. Fixing reclamation while references leak upward
only moves where the space is wasted.

**Guard this one carefully.** It is refcount code, and an over-correction
reaps live content: the failure mode of getting it wrong is `data_loss`,
against `inconsistent` for the bug itself. The fix wants the release to
happen unconditionally with the increment it pairs with — ideally by making
the pairing explicit at the call site rather than leaving two functions to
agree about it — plus a test that a same-content rewrite leaves `ref_count`
unchanged across N repeats.

Note this is the *third* place in this investigation where an increment and
its release disagree about the same-hash case; the attached-blob path has the
same shape in a different table. Worth checking whether one fix covers all
of them before writing three.

**As implemented.** The guard is simply gone: the release is now
unconditional, because when `old_hash == new_hash` the value to release is the
same string either way, so one unconditional `remove_reference(&old_hash)` is
correct in both arms. The minimal diff turned out to be the right one — no
restructuring of the call-site pairing was needed.

Verified before touching it, given the `data_loss` warning above: all three
callers reach it through the single wrapper `update_file_content_with_blob`,
whose comment already states the contract — *"swap_blob_hash consumes its
reference and releases it on failure"* — and every error path in
`swap_blob_hash` already releases `new_hash` (`:218`, `:234`, `:244`). That is
the decisive evidence: if any caller did **not** hold a reference, those
existing error paths would already be over-releasing and reaping live content,
which is a far louder bug than a slow leak. So the incoming reference is real
on every path, and consuming it on success is what the function was always
meant to do.

Left deliberately best-effort. A failure to release now over-counts — a
storage leak that `blobs_consistency` detects and repairs — where failing the
request would discard a write that already succeeded. Wrong in the cheap
direction, on purpose.

### 5. Boot: distinguish "unreachable" from "misconfigured"

A transient backend error at startup panics after a **700 ms** total budget
(3 retries × 100/200/400 ms), via `main.rs:687`. The error kind is
`TransientBackend` — the code knows it is transient — and the panic text
advises checking that "the storage volume is writable by the oxicloud user
(UID 1001)", which for a DNS failure sends the operator to entirely the
wrong place.

Fail-fast on unreachable storage is defensible and matches the house
preference for a loud boot failure over a silent degraded start. The
objection is narrower: 700 ms cannot distinguish a misconfigured bucket from
a network that is not up yet, and the latter is the case that actually
happens (service ordering at boot). A bounded boot retry — 30–60 s, logging
each attempt — keeps fail-fast for genuine misconfiguration, which surfaces
as a non-transient error and should still panic at once.

### 6. Cache retains plaintext of deleted content — ordering **DONE**, job TODO

`CachedBlobBackend::delete_blob` invalidates the index and removes the
cached file — but *after* `self.inner.delete_blob(&hash).await?`. The `?`
means a failed backend delete propagates before the cache cleanup runs, so
the cached copy survives.

This is how the orphaned content in this investigation was read at all: the
plaintext was still sitting in `.blob-cache` days after its row was deleted.

Two consequences. Space, bounded by the LRU budget — minor. And **deleted
content remaining readable in plaintext on local disk**, which for a product
whose stated direction is end-to-end encryption is the more interesting one:
the cache sits *outside* the encryption wrapper
(`di.rs:416` wraps with encryption, `di.rs:437` wraps that with the cache),
so S3 holds ciphertext while the cache holds cleartext.

Cleanup should not be conditional on the remote delete succeeding.

**As implemented.** `delete_blob` now invalidates the index and unlinks the
cached file **before** calling `inner.delete_blob(&hash)`, and returns the
inner result directly — so the local copy goes whether or not the remote call
succeeds, and the error still propagates unchanged to the caller.

Local-first rather than "inner, then evict in both arms", which would also have
fixed the reported bug. The ordering matters for the *crash* case: a kill
between the two steps must not be the one that retains plaintext. Evicting
first means a crash leaves at worst a cold cache entry for a blob whose delete
then failed — one remote re-fetch — while the object itself becomes an orphan
the sweep already knows how to find. That is the same bias the rest of this
plan follows: leak bytes rather than retain plaintext or lose data.

#### Does `.blob-cache/` need a consistency check of its own?

Asked during review, and the answer splits by *which* consequence you care
about — because the cache is in much better shape structurally than the backend
is.

**On space, no.** Two properties already hold, both verified:

* the on-disk cache is **reconciled with the in-memory index at boot** —
  `initialize` walks the cache dir and rebuilds the moka index from what it
  finds (`cached_blob_backend.rs:138-148`), so a restart does not orphan disk
  files behind an empty index;
* the index is **size-bounded**, with a byte weigher, `max_capacity`, and an
  eviction listener that unlinks the file (`:91-93`).

So stale entries cannot grow without limit and cannot become invisible. A
stale entry is reclaimed by ordinary cache pressure. That is strictly better
than the backend situation, where a stranded object had no row and no walker.

**On privacy, yes — and that is the whole reason.** The cache holds
**plaintext** where S3 holds ciphertext, so a stale entry is retained cleartext
of content the user deleted. Two things make it more than theoretical:

* Eviction is bounded by *size pressure and LRU order*, not by time. On a cache
  that is not full, a stale plaintext entry can persist indefinitely — there is
  no `time_to_live` making it age out.
* The `?` above makes the failure path a **systematic** producer of exactly
  these entries rather than a rare race. Every orphan in this investigation is a
  candidate, which is precisely how their content was read.

**So the fix is the ordering, and a check is only a backstop.** In priority:

1. **Never gate cache eviction on the remote unlink.** Evict in both arms, or
   evict first — it is a local unlink that cannot fail for network reasons, so
   it has no business inheriting the remote call's failure mode.
2. **Cache eviction must NOT be deferred into `pending_actions`.** The queue
   exists to defer the *remote* unlink; deferring plaintext removal would be
   strictly worse than today. Eviction stays eager at reap time, and the queue
   carries only the backend object. Worth stating explicitly because "move
   deletion off the critical path" invites moving all of it.
3. **And then a periodic eviction job, not a consistency check.** Review
   reframed this and the reframing is right — see below.

#### `backend_cache_cleanup`: periodic, and repairing by default

First a correction to the paragraph above, because the distinction matters:
**boot does not evict anything today.** `initialize` *re-adopts* what it finds
on disk into the index; it never asks whether those hashes still have live
references. So the only moment anything looks at the cache directory is boot,
and even then it only rebuilds bookkeeping. Stale plaintext is re-adopted, not
removed, and thereafter leaves only under LRU pressure.

That makes boot-only the wrong cadence for an additional reason: a self-hosted
instance is exactly the deployment that runs for months without a restart. The
window during which deleted content stays readable is then bounded by uptime —
i.e. unbounded in practice. A job that runs **at boot and on a schedule** is the
fix, and both halves already exist as mechanism: `OXICLOUD_STARTUP_JOBS` for the
boot run, the ordinary job schedule for the rest. Nothing new is needed.

**It should repair by default — a deliberate exception to the house rule**, and
worth arguing rather than assuming. Discovery-only exists because repairing a
storage inconsistency can destroy the only copy of something. That cannot happen
here: a cache entry is *by construction* a copy, and the authoritative bytes are
in the backend. Deleting a stale entry removes plaintext that should already be
gone; deleting a live one costs a cache miss. There is no destructive arm to
gate. And for this particular finding the discovery-only default is actively
wrong: reporting "there are N plaintext copies of deleted files on disk" and then
waiting for someone to click *repair* is a decision to keep them.

So: `Mutates::Always`, which is what actually distinguishes it from every
`*_consistency` job.

**On the name**, settled after weighing three candidates, since job names are
effectively permanent once operators script against them:

* **`backend_cache_cleanup`** — chosen. The `backend_` prefix is accurate,
  because `CachedBlobBackend` *is* a layer in the backend stack
  (`Cached(Encrypted(S3))`), and it groups the job with `backend_consistency`,
  `backend_migration`, `backend_reclaim` and `backend_rotate`. That grouping is
  the deciding factor rather than a nicety: `AdminJobsPanel` sorts on the name,
  so an operator scanning storage-backend jobs finds all five together. It also
  makes a useful pair legible — `backend_reclaim` reclaims remote objects, this
  reclaims their local copies.
* **`l2_cache_cleanup`** — rejected. The codebase never uses L1/L2 for caching;
  every occurrence of `L2` is L2-*normalisation* in the face-embedding code
  (`face_geometry.rs`, `people_service.rs`), so the prefix would collide with an
  established meaning in the same repo and assumes an unwritten tier hierarchy.
* **`storage_cache_cleanup`** — rejected. `storage_` was already dropped as a job
  prefix (it is why `storage_reconcile` became `backend_*`): nearly everything
  here is storage, so it does not discriminate.

Two naming constraints worth recording because both are functional:

* **Not `*_consistency`.** The suffix is load-bearing — `consistency_batch`
  auto-discovers children by `ends_with("_consistency")` — and a `Mutates::Always`
  job does not belong in a discovery batch.
* **Not `*_eviction`.** In cache terminology eviction means *capacity-driven*
  removal, which is exactly what moka already does and what this job must not
  duplicate. This job removes entries whose origin is gone, which is garbage, not
  pressure. `cleanup` also correctly signals scan-and-infer, distinguishing it
  from `backend_reclaim`'s drain-a-queue-of-recorded-intents — the differing verbs
  carry information rather than inconsistency.

Cost is what makes the schedule easy: a local directory walk plus one reference
query, no network and no egress. Unlike `backend_consistency`, where a bucket
walk costs real money on S3, this can run often without anyone weighing it.

Three details for the implementation:

* **Reuse the grace window.** An upload populates the cache *before* the backend
  put (`:186-192`) and the PG row lands later still, so a fresh entry legitimately
  has no reference yet. Skip entries newer than a grace window, exactly as
  `backend_consistency` does — otherwise the job churns against in-flight
  uploads. Getting this wrong costs only a re-fetch, never data, but it would
  look like a bug.
* **A pending deletion counts as no reference, and that is correct.** When
  `(H,'deletion')` is queued, H's row is already gone, so the job evicts the
  plaintext promptly even though the remote unlink is still deferred. That is the
  right split, and it is the same principle as rule 2 above stated from the other
  side: the local copy goes now, the remote object goes when the backend allows.
* **Don't re-implement the size bound.** Moka owns capacity eviction in-process
  and does it better. This job owns *staleness*, which moka cannot know because
  it has no view of references.

This also subsumes the one-off problem: plaintext already on disk from before the
ordering fix ships. Without the job that residue needs an admin to notice it;
with it, the first scheduled run clears it.

**As implemented.** Hourly, `Mutates::Always`, and registered **only when a cache
exists** — the cache decorator is built for remote backends only, so on a Local
deployment there is no second copy of anything to go stale and the job would be a
directory walk over nothing, forever.

Two implementation notes worth keeping:

* **It walks the DIRECTORY, not the moka index.** The index is in-memory and
  rebuilt at boot, so an entry the index has forgotten would be invisible to an
  index-based walk — and those are exactly the files this job exists to find.
* **DI now retains the concrete `Arc<CachedBlobBackend>`.** The cache is the one
  layer with storage of its own that needs sweeping, and neither its directory nor
  its index is reachable through `dyn BlobStorageBackend`. Coercing the handle away
  at construction, as the code did, would have forced a downcast later.

The grace window errs toward keeping entries in all three ambiguous cases — no
mtime, an unreadable stat, or a clock that moved backwards — because skipping
costs one more sweep while guessing wrong deletes a live cache entry. The unit
tests pin each of those.

**Coverage limitation, stated rather than papered over:** the API suite runs on a
Local backend, where the cache is disabled, so none of this is exercised
end-to-end there. The unit tests cover the staleness predicate; a real run needs a
remote-backend environment, which today means the Azurite suite.

## Verification

1. **Concurrency first, before any GC change.** Reap-then-reupload;
   reupload-during-unlink; unlink-fails-then-retries; park-after-N. These
   decide whether the design is safe, and they are cheap against the
   side-table variant precisely because `storage.blobs` is untouched.
2. **The failure that started this**: enqueue, make the backend fail the
   unlink, assert the queue row survives with `attempts` incremented, then
   let the backend succeed and assert both object and queue row are gone.
   This is the case the current code cannot pass.
3. **No new orphans on upload failure.** Already true today — the outage
   test produced zero new orphans, because durability-before-visibility
   means a failed upload writes no row. Pin it so it stays true.
4. `backend_consistency` before/after a drain: the count must fall to zero
   for enqueued hashes, and the repair arm must enqueue rather than delete.
5. Refcount: a same-content rewrite must leave `ref_count` unchanged, and
   `manifests_consistency` must report no mismatch after N repeats.
   **Landed** as `tests/api/refcount_same_content_rewrite.hurl` (registered in
   `run.sh` beside `dedup_blob_cleanup.hurl`). It uploads a fixture whose
   content is unique to the file so `ref_count` can be asserted absolutely,
   rewrites it three times through the WebDAV PUT overwrite-by-path branch —
   asserting 204 rather than 201 each time, since a second file row would
   legitimately raise the count and mask the bug — and holds `ref_count == 1`
   throughout. Both refcount tenants are then re-run and compared against
   baselines captured up front rather than against zero, so pre-existing drift
   from sibling suites cannot flunk it.

   The assertion that matters most is the last one: after the file is trashed
   **and purged**, `exists == false`. That is the user-visible harm rather than
   a restatement of the count — a surplus reference means the blob can never
   reach zero, so its bytes are pinned for the life of the deployment. With the
   leak and three rewrites, the blob survives the purge.

   **Green.** The first run already confirmed the fix — `ref_count == 1` after
   each of the three identical rewrites, and both consistency tenants back at
   their baselines — but tripped on a wrong status code in the teardown
   (`DELETE /api/trash/{id}` returns **200**, not 204, unlike the `/api/files`
   and `/api/folders` deletes). Corrected, and the suite passes.

   One fragility worth knowing rather than fixing: a mid-run failure poisons the
   next run, because folder names are unique per parent and the `ref_count`
   assertions are absolute, so a leftover folder or file breaks steps 3 and 5
   respectively. Hurl has no conditionals, so self-healing is not available; the
   file's header documents the four-request cleanup instead. That is the same
   trade `refcount_cascade.hurl` makes — it ships a diagnostic script rather than
   attempting idempotence.
6. `just check`, `just test`, `just test-integration`, `just api-test`.

### How to test this — instrument per claim, and mocks are the least useful one

The cases here look hard to test because they are all "what happens when the
backend fails at *this* instant". That is a reason to reach for fault injection
and a barrier, not for mocks: a `mockall` mock of `BlobStorageBackend` would
assert that we called the port we already know we called, while the claims worth
pinning live in PostgreSQL's locking semantics and in the real decorator stack.
Five instruments, in rough order of value:

**0. Already landed, as the first instance of instrument 1 below.**
`failed_backend_delete_still_evicts_the_plaintext_copy`
(`cached_blob_backend.rs`) proves §6's ordering fix with an `UndeletableBackend`
— a hand-rolled delegating decorator whose `delete_blob` always fails
transiently. It asserts both halves that matter: the failure still propagates
(and stays `is_transient`, so the caller can retry the remote object) *and* the
local plaintext is gone anyway.

Confirmed to be a real regression test rather than a tautology by reverting the
ordering and watching it fail on the right assertion — *"plaintext of deleted
content survived a failed backend delete"*. Worth doing for every test in this
plan: each one exists to catch a specific defect, so each should be seen failing
against that defect before being trusted.

**1. A reusable fault-injecting backend decorator — the key instrument.** The
codebase already does this per-file and by hand: `HangingBackend`
(`timeout_blob_backend.rs:471`) is a test-local `impl BlobStorageBackend`, no
mockall involved. Promote that into one `FaultyBlobBackend` under the
`test_utils` feature, configurable with "fail the next N `delete_blob` calls",
"fail `put_blob_from_bytes` for hash H", "hang", and — the part that matters —
**a barrier hook that blocks inside the call until the test releases it.**

Composing a real decorator beats a mock twice over: it exercises the true stack
(`Cached(Encrypted(Faulty))`), and the hook turns the race tests from timing
guesses into deterministic sequencing. §2f is only hard to test if you try to
*provoke* the interleaving; with a hook inside `delete_blob` the test drives it:
enqueue H, start the drain, let it block inside the unlink, run an upload of the
same content to completion, release the drain, then assert the bytes are present
and the queue row gone. No sleeps, no flakes. This single instrument covers
reap-then-reupload, reupload-during-unlink, unlink-fails-then-retries, park-after-N,
and the partial-multi-chunk-upload rollback.

**2. Real PostgreSQL, never a mock, for everything SQL.** For several claims the
thing under test *is* Postgres' behaviour, so a mock cannot fail the way
production would:

* the atomic `WITH reaped AS (DELETE …) INSERT` enqueue — kill the statement and
  assert the row delete and the queue insert are both absent or both present;
* `ON CONFLICT (hash, action) DO NOTHING` **preserving** `attempts`,
  `last_error` and `parked_at` — the regression test for the reset this plan
  removed;
* `FOR UPDATE SKIP LOCKED` — two pooled connections in one test, `BEGIN` on
  both, assert the second *skips* while holding the first, and that a plain
  `FOR UPDATE` on the uploader's side *blocks* instead. This is the asymmetry
  §2f depends on, and it is a ten-line test;
* `blobs_consistency`'s `actual_ref_count` recompute, against hand-built
  reference shapes (legacy file rows, manifests, repeated chunks in one manifest).

Use the tests DB on **5433**, never 5432.

**3. Crash semantics without crashing a process.** What makes a crash special is
only that *compensation never runs* — so suppress the compensation instead of
killing anything. Drive an ingest so it pins existing chunks and writes new
ones, then `std::mem::forget` the `IngestGuard` (or a test-only flag making
rollback a no-op) to simulate the kill, and assert `blobs_consistency` reports
the expected `refcount_mismatch` with a negative delta, and that its repair arm
restores the count. That tests the detector, which is the part that actually has
to work. Keep **one** genuine SIGKILL scenario in `tests/api/` — start the
server, kill it mid-upload, restart, run the job — following the shell-driven
precedent of `thumb_import_check.sh`; more than one buys little.

**4. Hurl for the admin-visible contract**, following `transcode_import.hurl`
and `drive_policy_repair.hurl`: backlog depth / oldest / parked are exposed and
move (§2c); triggering `backend_reclaim` reports the counts; the repair arm
enqueues rather than deletes. Remember recoverable jobs nest their extras at
`$.outcome.extra.extra_stats.<key>`, not `$.outcome.extra.<key>`.

**5. Plain unit tests** for the arithmetic that needs no store at all: backoff
progression, the parking threshold, cursor boundary handling, and the
`is_transient` classification that decides whether anything retries.

**What stays genuinely untestable, and should be stated rather than faked.** The
irreducible window between `sync_blobs` returning and the INSERT committing
cannot be closed, so a test can only assert that residue landing there is
*discoverable* — which is item 3. And nested SDK retry (§3) is a property of the
AWS client, so it is observed with `RUST_LOG=aws_smithy_runtime=debug` rather
than asserted; if a regression test is wanted, assert the *config* we set, not
the attempt count we would be guessing at.

**Where `mockall` still earns its place**: trait-boundary unit tests that touch
neither store — that a service calls `authz.require(...)` before mutating, say.
That is what the `test_utils` mocks exist for, and it is not this plan's problem.

## Follow-up scope — NOT in this PR

> **State as of 2026-10-04.** Both items below have since landed — item 1
> in full, item 2's precondition with it. Kept because each records *why*
> the shape is what it is, which the next detector will need. The only
> thing still outstanding in this whole plan is §1's deletion of the
> legacy whole-file blob path, **deferred to the next major** — it needs
> the migration terminated fleet-wide and a major-version boundary, not
> code.

From an audit for the defect class this plan exists to remove: a backend request
whose failure is logged and forgotten, leaving an inconsistency nothing can
rediscover. The finding is that **the deletion path was the outlier, not the
norm** — the migration and satellite code is written with real discipline here.
But its enabling condition is systemic.

### 1. The detectors are never scheduled — the biggest item — **DONE 2026-10-03**

**Funded.** `OXICLOUD_JOBS_SCHEDULED` defaults to
`consistency_batch=168h`, and the batch fans out to **every** job whose
name ends in `_consistency` — so new detectors are picked up without an
entry of their own, on a database-friendly serial walk rather than seven
jobs ticking at once. Weekly rather than daily because the batch includes
`backend_consistency`, whose bucket walk costs real money on S3.

A scheduled run is discovery-only: `deep` and `repair` both default to
`false`, and `repair` is **refused at boot** if named in the schedule —
a tick that repairs deletes on a cadence with nobody consenting after the
first time.

So the guarantee below now holds in both halves rather than one. The
analysis is kept because it is the argument for why the schedule exists,
and because the same reasoning applies to the next detector anyone adds.

*As found — every consistency job registered on-demand:*

| job | interval |
|---|---|
| `blobs_consistency`, `manifests_consistency`, `satellites_consistency` | `None` |
| `files_consistency`, `folders_consistency`, `drives_consistency` | `None` |
| `drive_policies_consistency`, `backend_consistency` | `None` |
| `backend_reclaim`, `backend_cache_cleanup` | scheduled (added here) |

This matters more than any individual call site, because **"it's fine, a sweep
will find it" is the justification the codebase leans on everywhere — including
this plan's own invariant — and it is currently unfunded.** It is also exactly
the mechanism behind the 29 orphans: `dedup_gc` was `None, // on-demand`, nobody
clicked, and bytes accumulated for months.

So this plan's stated guarantee — *every disagreement is either recorded in the
outbox or discoverable by the sweep, and both are bounded and observable* — holds
in its first half and, until the detectors run on their own, only aspirationally
in its second.

Giving at least `blobs_consistency` and `satellites_consistency` a schedule is
cheap (DB-only). `backend_consistency` is the judgement call, since a bucket walk
costs real money on S3 — a weekly `deep=false` default would be defensible.

**Review direction: `*_consistency` jobs need retry-and-pause on recoverable
error, not the queue treatment.** They are read-mostly, so a transient failure
should pause the run at its cursor and resume, exactly as
`docs/plan/jobs-handling-recoverable-error.md` describes — not be routed through
`pending_actions`, which exists for intents that must outlive a process.

**Also DONE** (2026-10-02/03), as Part A of
`failure-classification-and-satellite-lifecycles.md`: the transience
classification, a bounded retry, and a pause at the cursor with
`error_reason` recorded on the run. A run that gives up now also *tells
someone* — the alerting that plan added keys off that column.

### 2. Log-and-forget sites, classified

*Acceptable — discoverable or recomputable* — and the subjection to item 1
is **now satisfied**, so these are genuinely discoverable rather than
aspirationally so:
`remove_reference` failures at `dedup_service.rs:901`, `:989`, `:1105`, `:1311`
leave an over-count so the blob never reaches 0, which `blobs_consistency`
recomputes and repairs. `IngestGuard`'s rollback still registers chunks at
`ref_count = 0`, so GC finds them. Thumbnails and transcodes are pure functions of
their source.

*Correctly handled:* `persist_migration_readonly(true)` and
`persist_active_backend_name` both fail the run rather than continuing; the
readonly RELEASE logs but states the exact consequence and errs conservative.

*Genuinely silent (`let _ = …`, not even logged), all local scratch rather than
content:* `thumbnail_service.rs:1914` (legacy sidecar unlink — a shrinking set,
since thumbnails now go to `store_derived_blob`), `s3_blob_backend.rs:211,237`
(spool removal), `chunked_upload_service.rs` ×6, `image_transcode_service.rs:688`.

### 3. §1d — closed, not deferred

Reviewed and dropped: the "both read every blob" premise was wrong.
`backend_rechunk` filters in the database to legacy blobs only, so the overlap
with `backend_rotate` is that subset alone — and it shrinks to zero as the
migration completes. See §1d for the full reasoning. The residue is one sentence
of operator guidance, not code.

## Not in scope

- **Soft-undelete of blob content.** A queue with a delay window could
  double as one, and it is tempting. It is a product decision with its own
  UX, and trash already covers user-level undelete; conflating them would
  make the retention window a side effect of a reclamation knob.
- **Reworking the grace window.** Once intent is durable the grace window's
  original job — making the reupload race "vanishingly narrow" — is mostly
  done by construction, so it could probably shrink. Left alone until the
  queue is proven; changing two race-mitigations at once is how a race gets
  reintroduced.
