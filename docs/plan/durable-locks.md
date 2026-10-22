# Plan — Durable locks: surviving a restart without losing an edit

**Status:** design captured 2026-10-17, **not started**. Found while reasoning
about whether a WOPI `Version` format change needed an interlock
([`normalize-file-dates.md`](normalize-file-dates.md) §2). It did not — because
a deploy clears the lock table anyway. That answer is correct and the reason it
is correct is a defect.

Neither lock store is persisted, and there is no lock table in the schema:

| store | backing | expiry |
|---|---|---|
| `WopiLockService` | `Arc<RwLock<HashMap<String, LockEntry>>>` | `expires_at: Instant` — **monotonic** |
| `WebDavLockStore` | `moka::sync::Cache<String, LockEntry>` ×2 (`by_path`, `by_token`) | moka `Expiry` policy from `info.timeout` |

```
$ grep -rn -i "CREATE TABLE.*lock" migrations/*.sql
(no matches)
```

So **every deploy, restart or crash silently drops every held lock** — WOPI and
WebDAV alike. Default WOPI TTL is 1800 s (`OXICLOUD_WOPI_LOCK_TTL_SECS`), so a
restart can discard a lock with up to half an hour of validity remaining.

This sits against the project's first design axis — *no data loss, no data
corruption, ever* — and it is reachable by routine operations: deploy during
working hours while two people have the same document open.

| phase | state |
|---|---|
| §1 persist WOPI locks | **NOT STARTED** |
| §2 persist WebDAV locks | **NOT STARTED** — same table, same wall-clock conversion |
| §3 reclaim-on-boot and observability | **NOT STARTED** |

---

## The two failure modes

### Silent lock loss — annoying

An editing session keeps working locally, unaware. Its next `PutFile` or
`RefreshLock` presents `X-WOPI-Lock: <id>` the server has no record of, and the
handler answers `409 CONFLICT` with the `X-WOPI-Lock` header
(`wopi_handler.rs:481`, `:491`, `:500`). The user sees "could not save" and
whatever recovery flow the editor offers.

This path is already covered by a unit test, which is a good sign the behaviour
is understood: `test_refresh_lock_on_unlocked_file_returns_conflict`
(`wopi_lock_service.rs:205`). What is missing is that the *cause* — a restart —
is invisible to everyone involved.

### Lost update — the serious one

1. Session **A** opens `report.docx` and holds a lock.
2. Deploy. The lock table is empty.
3. Session **B** opens the same file, finds it unlocked, and takes a **fresh**
   lock.
4. Both sessions now believe they hold the lock. Whoever saves second
   **silently overwrites** the other's work.

Nothing in the current design prevents step 3, because step 2 erased the only
record that A exists. WebDAV `LOCK` has the identical shape.

Content-addressing does not save us here: both saves are legitimate writes of
different content, each with a valid-looking lock. The ref-count and blob layers
behave correctly; the *edit* is what is lost.

---

## §1 — Persist WOPI locks **NOT STARTED**

### `Instant` is the blocker, not the storage

The obvious move — serialise `LockEntry` into a table — does not work as
written, because

```rust
// wopi_lock_service.rs:15
expires_at: Instant,
```

is a **monotonic** clock reading. It is meaningful only within one process run;
it has no epoch, cannot be compared across restarts, and has no sensible
serialisation. So durability requires converting expiry to wall-clock
`DateTime<Utc>` first.

That conversion is itself a behaviour change worth noting: monotonic clocks are
immune to NTP steps and wall-clock adjustments, whereas `DateTime<Utc>` is not.
A backwards clock step could extend a lock's apparent life, and a forward step
could expire one early. For a 1800 s TTL guarding an interactive editing session
that is an acceptable trade — the alternative is losing every lock on every
deploy — but it should be a stated trade rather than an accident.

### Schema

```sql
CREATE TABLE storage.resource_locks (
    resource_id  UUID        NOT NULL,
    kind         TEXT        NOT NULL,   -- 'wopi' | 'webdav'
    lock_id      TEXT        NOT NULL,   -- WOPI lock string / WebDAV token
    expires_at   TIMESTAMPTZ NOT NULL,
    acquired_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (resource_id, kind)
);

CREATE INDEX idx_resource_locks_expiry ON storage.resource_locks (expires_at);
```

One table for both kinds, since §2 needs the same shape and the semantics only
differ in the token format. `PRIMARY KEY (resource_id, kind)` enforces the
one-lock-per-resource-per-protocol invariant **in the database** rather than in
a `HashMap` guarded by an `RwLock` — which also removes the read-modify-write
race the current `lock()` implementation has between its conflict check and its
insert.

Keep the in-memory map as a read cache if the hot path needs it, but the table
is the authority.

### Expiry

Lazy, not swept: a lock whose `expires_at` has passed is simply not a lock, so
every read filters on `expires_at > now()`. The existing
`start_cleanup_task`/`cleanup_expired` pair becomes housekeeping to keep the
table small, not correctness. That matters because a sweep that stops running
must not resurrect expired locks — the same reasoning the consistency jobs use.

---

## §2 — Persist WebDAV locks **NOT STARTED**

Same table, same wall-clock conversion. `WebDavLockStore` keeps two moka caches
(`by_path`, `by_token`) with expiry derived from `info.timeout`; both become
queries against `storage.resource_locks`.

Two WebDAV-specific details:

- Locks are keyed by **path**, not resource id, and a path is not stable across
  a MOVE. Decide whether to key on resource id (and so follow renames, which
  RFC 4918 arguably wants) or keep path semantics. This is a behaviour decision,
  not a storage one.
- RFC 4918 allows infinite-depth collection locks. If those are supported, a
  durable store needs to answer "is any ancestor locked" — an `ltree` ancestor
  query rather than a point lookup.

---

## §3 — Reclaim on boot and observability **NOT STARTED**

Persisting locks means a restart now *inherits* them, which introduces the
opposite problem: a lock held by a session that will never come back (the user
closed the laptop) blocks the file until its TTL expires. With a 1800 s default
that is a visible annoyance where today it is invisible.

So boot should log what it inherited, and the admin surface should be able to
list and force-release a lock. An operator being able to see "this file is
locked, by this protocol, since this time, expiring then" is the missing half of
making locks durable.

Audit lines on forced release, per the convention in `AGENTS.md`:
`event = "lock.force_released"`, with a stable `reason`.

---

## Interaction with the dates plan

`normalize-file-dates.md` proposes moving WOPI `Version` from a timestamp to
`blob_hash`, and argues no interlock is needed because a restart disrupts
cross-restart sessions regardless. **That argument depends on this defect.**

So the ordering matters:

- do the `Version` change **while locks are ephemeral** — it is free, as argued
  there; or
- do it **after** this plan lands, and accept that it then needs the interlock
  that was dismissed.

Cheapest path is to do the `Version` change first. Worth noting explicitly in
both documents so neither is read in isolation.

---

## Rejected alternatives

- **Longer TTLs.** Does not help: the lock is gone, not expired.
- **Refuse to restart while locks are held.** Makes deploys hostage to an idle
  editor, and the lock table is unreadable from the new process anyway.
- **Rely on the editor to re-lock.** WOPI has no re-lock-after-host-restart
  handshake; the editor believes it already holds the lock. And it would not fix
  step 3 of the lost-update sequence, where a *second* session legitimately
  acquires the lock the first one lost.
- **Keep locks in memory but replicate between instances.** Solves multi-instance
  but not restart, which is the common case. A table solves both.

---

## Open questions

1. **Is multi-instance a goal?** In-memory locks are also broken across replicas
   today. A table fixes that for free, which may matter more than restart
   durability if horizontal scaling is on the roadmap.
2. **Key WebDAV locks on path or resource id?** (§2) Changes MOVE behaviour.
3. **Does anything else hold ephemeral state that should survive a restart?**
   Worth one sweep for `HashMap`-behind-`RwLock` and moka caches that are
   authoritative rather than caches — the same question this plan answers for
   locks.
