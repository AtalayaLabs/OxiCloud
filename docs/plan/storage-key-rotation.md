# Plan — Storage encryption-key rotation

## Context

Today an encrypted storage entry declares a single key:

```env
OXICLOUD_STORAGE_s3_prod_BACKEND=s3
OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY=<base64 32 bytes>
OXICLOUD_STORAGE_s3_prod_ENCRYPTION_CIPHER=aes-256-gcm   # optional, defaults to aes-256-gcm
```

Blobs land at `<hash>.blob` as raw AES-GCM output (`nonce | ciphertext | tag`) —
no discriminating prefix. That key can never change. Any real deployment needs
to rotate keys — after a suspected leak, on a periodic policy, or after a staff
turnover. The current answer is "create a second entry and run a migration to
it," which:

* forces the admin to provision a second bucket / directory,
* churns object storage costs,
* and does not scale to routine rotations.

The proper fix is **in-place key rotation** inside the same entry. The storage
config holds a *list* of keys, the server writes with the newest one, reads
with any of them, and a background job re-encrypts every blob under the newest
key. When the job completes, the admin removes the old key from the list.

At the same time we solve two related on-disk problems that today's format
leaves open:

1. **No self-description.** A `<hash>.blob` today is ambiguous: could be raw
   plaintext (unencrypted deployment) or raw AES-GCM output (encrypted
   deployment). Reads guess based on the entry config; a misconfigured key or
   a wrong-cipher default silently returns garbage.
2. **No path from encrypted to plaintext.** Once encryption is on for an
   entry, the only way off is via a second entry and full migration — same
   ergonomics problem as key rotation.

Both fall out of introducing a self-describing on-blob header (`v1`). The
object-key suffix stays `<hash>.blob` for both eras — the magic bytes at the
top of every v1 blob are the discriminator, so legacy files stay readable
byte-identically and new files coexist alongside them at the same suffix.
Migration is lazy.

Related memory (this plan supersedes it):

> **[Encryption key rotation in place — deferred, unsafe today]** — the previous
> "namespace object keys by encryption generation" direction (`<hash>.k2.blob`)
> is DROPPED in favour of the pair-list + v1-header approach below. That
> approach fought content-addressability by generation-namespacing per key;
> this one keeps a stable content-addressable object key and encodes generation
> in a compact on-blob header.

## Design

### The pair-list config

Replace the singular `_ENCRYPTION_KEY` + `_ENCRYPTION_CIPHER` with a single
comma-separated list of pairs on `_ENCRYPTION_KEY` alone:

```env
OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY=aes-256-gcm:<base64 K1>,aes-256-gcm:<base64 K2>
```

Rules:

* **Format** — `[<cipher>:]<base64 key>` per pair, comma-separated. Whitespace
  around commas / colons tolerated.
* **Cipher optional.** Only one real cipher (`aes-256-gcm`) exists today, so
  `<base64 key>` on its own is legal and behaves as `aes-256-gcm:<key>`. When a
  second cipher lands, the colon-prefixed form is the disambiguator.
* **The `none` cipher.** Legal, has no key material (`none:` — trailing colon
  with an empty key). Enables migrating BOTH ways: encrypt a previously-plaintext
  deployment (`none:,aes-256-gcm:<K>`), or decrypt an encrypted deployment
  (`aes-256-gcm:<K>,none:`). Correctness relies on the v1 header (see
  *Encrypted-blob format*) discriminating encrypted vs plaintext, NOT on
  fallback ordering. A `none` pair by itself in the list is equivalent to
  omitting `_ENCRYPTION_KEY` entirely (kept for symmetry).
* **Order matters. Last pair wins on writes.** For v1-header reads, the
  `key_fp` in the header selects the exact pair; the list order is irrelevant
  at read time. For legacy reads (see *Coexistence*), the head pair is used
  because pre-v1 deployments had only one key by construction.
* **At least one pair.** Empty list → boot aborts.
* **Uniqueness.** Same key appearing twice = boot aborts (config drift smell).
  `none` may appear at most once.
* **Fingerprint.** For each real-cipher pair, log a truncated SHA-256 of the
  key material at boot (12 hex chars) so a swap-order accident is loud in the
  audit stream. `none` logs as `none:—`.

### Retire `_ENCRYPTION_CIPHER`

`_ENCRYPTION_CIPHER` was added recently and has not shipped in a release. Drop
it entirely in the same slice — the pair-list makes it redundant, and the on-blob
v1 header carries the cipher choice explicitly per blob anyway. No back-compat
shim needed.

### Encrypted-blob format (v1 header)

Every v1 blob starts with a fixed self-describing header. Layout:

```
"OXCPT"        5 bytes  — magic marker (0x4F 0x58 0x43 0x50 0x54)
<version>      2 bytes  — big-endian u16; v1 = 0x0001
<key_fp>       8 bytes  — sha256(key material)[..8]; identifies which pair
                          decrypts. For a `none` (plaintext) v1 blob this is
                          eight zero bytes and the fields below are absent.
<nonce>       12 bytes  — random per blob (AES-GCM standard nonce length)
<ciphertext>   N bytes  — encrypted payload; same length as the plaintext
<auth_tag>    16 bytes  — AEAD authentication tag
                          ────
fixed overhead: 43 bytes for encrypted v1; 15 bytes for plaintext v1
```

* **The magic `OXCPT`** unambiguously distinguishes a v1 blob from legacy raw
  bytes. It's the sole discriminator — no filename convention, no DB column,
  the first five bytes tell the reader everything.
* **`<version>`** is the only field that can change format without touching
  every blob. v1 is what this plan ships; v2 slots are reserved
  (see *Forward compatibility*).
* **`<key_fp>`** eliminates guess-work on read. The pair-list lookup becomes
  O(1) — no fallback tag-check loop. A key-fp with no matching pair means
  `NoKeyForBlob` (500 with a clear message), NEVER silent garbage.
* **`<nonce>`** and **`<auth_tag>`** are the AES-GCM primitive's own outputs.
  Their length is fixed by v1 = AES-256-GCM. If a future v2 uses a different
  AEAD with the same shape (say ChaCha20-Poly1305, tag = 16 B, nonce = 12 B)
  v2 can keep the layout; a truly-different AEAD triggers a new version.
* **Plaintext v1 blobs** (produced when the entry's head pair is `none`) use
  the same magic + version + all-zero key_fp, then the raw plaintext bytes
  directly (no nonce, no ciphertext framing, no tag). The header alone is
  enough to say "this is plaintext v1", cleanly distinguished from encrypted
  v1 and from legacy raw-plaintext.

**Why no CRC.** The AEAD tag covers ciphertext + nonce integrity. Content
addressability (`<hash>` = BLAKE3(plaintext)) covers whole-blob integrity for
either format. A CRC over the header adds nothing — a corrupt version byte
fails on decode; a corrupt key_fp fails to find a pair. Both fail loud.

### Coexistence with legacy blobs

Legacy and v1 blobs share the `<hash>.blob` object-key namespace. The magic
bytes at position 0 discriminate them safely:

| First 5 bytes | Interpretation | Read path |
|---|---|---|
| `"OXCPT"` | v1 blob | verify magic, dispatch on version + key_fp |
| anything else | legacy | apply entry config: raw plaintext if no encryption pair, single-key AES-GCM decode with the head pair if encryption is set (by construction pre-v1 used one key, and any pair-list produced by upgrading from a pre-v1 config keeps that key at the head) |

Collision probability of legacy bytes accidentally matching `"OXCPT"` at
position 0 is 2⁻⁴⁰ per blob. If a match does happen, the read continues into
the v1 path, fails at version/key_fp check with `UnsupportedBlobVersion` or
`NoKeyForBlob`, and returns a hard error. **Never silent corruption.** For
context: on a 10 M-blob deployment, expected number of collision hard-errors
across the lifetime of legacy files is ~10⁻⁵ blobs. Effectively never.

Properties this buys us:

* **No forced migration.** Legacy `<hash>.blob` files stay readable
  indefinitely. Existing deployments upgrade to v1 code with zero
  format-conversion work.
* **Lazy conversion on hot paths.** Any COW overwrite (WebDAV MOVE, PUT-over,
  content-hash re-upload) naturally lands as v1 at the same object key.
* **Explicit conversion via `backend_rotate`.** The rotate job walks
  `storage.blobs`, reads each blob via the magic-byte dispatch, and if the
  blob is not already v1 with the head-pair key, PUTs it back as v1 with
  the head pair — in place, same object key.
* **No schema changes.** Nothing new in `storage.blobs`; nothing for
  `blobs_consistency`, `storage_migrate`, or any other sibling job to learn.

### Legacy-pair guardrail

Once any legacy blob remains on disk, the pre-v1 single key that produced
those blobs MUST remain in the pair-list at the head position (or in the list
somewhere the legacy read path can find it). Removing it early breaks every
legacy read.

Guardrail:

* Boot logs a `warn` line if any pair-list has more than one entry AND the
  head pair's fingerprint differs from the second entry's — signals "you
  added new keys but haven't rotated legacy blobs yet".
* A **legacy-blob counter** is surfaced in the admin panel per storage entry.
  The counter is maintained by `blobs_consistency`: during its normal walk it
  branches on the magic-byte check and records the legacy count as a run
  statistic on `jobs.recoverable_runs` (existing surface, no schema hit). The
  admin panel reads the most recent count and displays it. Refresh cadence is
  whatever consistency scan cadence the deployment has (weekly by default; on
  demand from the admin panel).
* The *"Rotation complete — safe to remove the old key"* hint appears in the
  entry card only when the last `backend_rotate` run completed with zero
  findings AND the most recent consistency scan reported zero legacy blobs.
* Nothing enforces removal at code level — the admin is trusted, given a
  clear signal, and warned.

### Read path

Per blob read:

1. Fetch `<hash>.blob` from the backend.
2. Check first 5 bytes.
3. If `"OXCPT"` → v1 read path:
   * Read `<version>`. Not `0x0001` → `UnsupportedBlobVersion`.
   * Read `<key_fp>`. If zero → plaintext-v1; return raw bytes (post-header
     payload). Else find the pair whose `sha256(key)[..8]` matches; not
     found → `NoKeyForBlob`; found → AES-GCM decrypt with `<nonce>` /
     `<ciphertext>` / `<auth_tag>` and return plaintext.
4. Otherwise → legacy read path:
   * If entry has any real-cipher pair, attempt AES-GCM decrypt with the
     head pair's key; return plaintext on success, `DecryptFailed` on tag
     failure.
   * If entry has only `none` (or no pair at all), return raw bytes as
     plaintext.

Never falls through silently. Every failure is a distinct typed error the
handler can map to a 500 with an actionable message.

### Write path

1. Always writes to `<hash>.blob`.
2. Head pair is `none` → write v1 header (magic + version + zero key_fp) +
   raw plaintext.
3. Head pair is a real cipher → compute AEAD; write v1 header (magic +
   version + key_fp) + nonce + ciphertext + auth_tag.

v1 code never produces a legacy-format blob. Any leftover legacy blobs on
disk pre-date the v1-code deployment.

### The rotation job

New `RecoverableJobHandler` tenant, `backend_rotate`. Mirrors
`backend_migration`'s shape:

* **Iterates `storage.blobs`** in hash-lex order. Cursor is the last-processed
  hash (64 hex chars). Same cursor encoding as `backend_migration` and
  `blobs_consistency`.
* **Per blob:**
  1. Fetch `<hash>.blob` and dispatch via the standard read path.
  2. Decide if a rewrite is needed based on what actually decoded:
     * Legacy blob (no v1 magic) → always rewrite (upgrade to v1 with head
       pair).
     * v1 encrypted, decrypted under a pair-index other than head → rewrite
       (key rotation).
     * v1 encrypted, already under head — skip.
     * v1 plaintext, head pair is `none` — skip.
     * v1 plaintext, head pair is a real cipher — rewrite
       (encrypt-in-place upgrade).
     * v1 encrypted, head pair is `none` — rewrite
       (decrypt-in-place downgrade).
  3. Write via the standard v1 write path — same object key, atomic
     replace.
  4. Checkpoint. On per-blob failure, record a `rotation_failed` finding
     with severity `data_loss` (bytes may not have crossed), continue.
* **Concurrency-safe by construction.**
  * A concurrent upload during rotation writes v1 with the head key. The
    rotate walk will short-circuit on that hash if it reaches it later.
  * A COW overwrite is the same story.
  * The v1 write is atomic at object-storage level (S3 replace, Local
    rename-into-place). A concurrent reader sees either state.
  * No readonly mode. This is a critical improvement over
    `backend_migration`: rotation is per-blob idempotent, so we don't need
    to freeze writes.
* **Restart-survivable** — same boot-time sweep as every other recoverable
  handler.
* **`?deep=true` unused** — rotation has no "slow variant" mode. Parameter
  accepted for uniformity, ignored.

### Admin UX

In the admin storage-panel entry card, add a **Rotate encryption key** action.
Preconditions:

* Entry has encryption enabled with ≥ 2 pairs OR entry has legacy blobs
  outstanding OR the pair-list has otherwise changed since the last rotation.
* Otherwise the button is disabled with the tooltip *"Add a second pair in
  `OXICLOUD_STORAGE_<name>_ENCRYPTION_KEY` first, or wait for legacy blobs to
  accumulate — nothing to rotate right now."*

Clicking the button dispatches the `backend_rotate` job for that entry. The
job's progress rides on the same `X-Server-Status` header infrastructure the
maintenance banner uses — but this time WITHOUT engaging read-only mode.
Banner variant reads *"Rotating encryption key on `<entry>` — X% (Y / Z
blobs). All operations continue normally."* and disappears on completion.

The entry card shows a **legacy-blob counter** sourced from the most recent
`blobs_consistency` run: *"N legacy-format blobs remain (upgrade included in
next rotation)"*. Refresh-on-demand button next to it triggers a targeted
`blobs_consistency` scan (already available via the admin surface). The
*"Rotation complete — safe to remove the old key"* hint appears only when
N = 0 and the last `backend_rotate` run completed with zero findings.

### Removing the old pair

Not automated. The admin edits `.env`, restarts. Rationale:

* We do not want the running server to silently mutate its own `.env`.
* A retained old pair is a benign cost (nothing hits it on read since
  `key_fp` lookup is O(1)).
* Explicit human step matches the "add a new pair → restart" symmetry.

## Deployment flow

### First-time upgrade to v1

Zero admin work required. On upgrade:

* v1 code deploys.
* New blobs are written in v1 format at the existing `<hash>.blob` object
  key.
* Existing legacy blobs stay readable via the magic-byte dispatch — the
  legacy read path is preserved verbatim.
* Admin can optionally trigger a `backend_rotate` run to consolidate every
  legacy blob into v1 format. Not required — legacy blobs migrate
  opportunistically via COW overwrites and stay readable indefinitely
  otherwise.

### Rotating an encryption key

The user-facing recipe (goes verbatim into `docs/guide/backend-storage.md`):

```
1. Generate a new key:
   openssl rand -base64 32

2. Append it to the entry's key list. Order matters — the NEW key goes LAST:
   OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY=aes-256-gcm:<OLD>,aes-256-gcm:<NEW>

3. Restart OxiCloud. New uploads are now encrypted with the new key; existing
   blobs still decrypt with the old one.

4. In the admin panel, click "Rotate encryption key" on the entry. This
   dispatches a background job that re-encrypts every existing blob under
   the new key AND upgrades any remaining legacy-format blobs to v1. All
   operations keep working during rotation.

5. Wait for the job to complete (progress shows in the top banner and in
   the Jobs admin page).

6. Remove the OLD key from the list:
   OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY=aes-256-gcm:<NEW>

7. Restart OxiCloud. Rotation is complete.
```

### Encrypting a previously-plaintext deployment

```
1. Generate a new key (as above).
2. Add it AFTER `none`:
   OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY=none:,aes-256-gcm:<K>
3. Restart. New uploads are encrypted; existing plaintext blobs stay readable.
4. Run `backend_rotate` to encrypt existing blobs in place.
5. Remove `none:` from the list; restart.
```

### Decrypting an encrypted deployment

```
1. Add `none:` AFTER the current encryption key:
   OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY=aes-256-gcm:<K>,none:
2. Restart. New uploads are plaintext; existing encrypted blobs stay readable.
3. Run `backend_rotate` to decrypt existing blobs in place.
4. Remove the key pair, keep `none:` only (or drop `_ENCRYPTION_KEY` entirely);
   restart.
```

## Testing strategy

**Most of this ships as Rust tests, not Hurl.** The rotation surface is
byte-level, cryptographic, and stateful across restart boundaries — none of
which Hurl can meaningfully assert. Push almost everything down into
`#[cfg(test)]` blocks alongside the code, following the codebase's existing
inline-tests convention (see AGENTS.md — "tests are primarily `#[cfg(test)]`
modules within source files").

**What lives in Rust (unit + integration):**

* **Pair-list parser.** Unit tests in `config.rs` on `_ENCRYPTION_KEY` parsing:
  1-pair, 2-pair, cipher-optional shape, `none:` alone, `none:,aes:K`,
  `aes:K,none:`, whitespace tolerance, duplicate rejection, multiple-`none`
  rejection, empty-list rejection, retired-`_ENCRYPTION_CIPHER` rejection,
  base64 length validation. Table-driven; fast.
* **v1 header round-trip.** Unit tests in
  `encrypted_blob_backend.rs::tests`: encrypt payload → verify header bytes
  (magic, version, key_fp) → decrypt → assert plaintext identity. One test per
  format flavour (encrypted, plaintext-with-`none`-head). Verify `<hash>` =
  BLAKE3(plaintext) with no offset for plaintext-v1 (the "recovery tool"
  invariant).
* **Legacy read-path preservation.** Fixture: a hand-crafted raw AES-GCM
  ciphertext (`nonce | ct | tag`) produced by the pre-v1 code path. Assert
  that magic-byte dispatch falls through to the legacy branch and decrypts
  correctly. Fixture stays in-tree as `tests/fixtures/legacy_blob.bin` so
  regressions on the legacy path can never sneak through.
* **`OXCPT` collision on random data.** Property test: generate random N-byte
  payloads, feed through legacy read path. Assert the ~2⁻⁴⁰ magic-collision
  case fails with `UnsupportedBlobVersion` or `NoKeyForBlob` — hard error,
  never silent misread. Keeps the "collisions can't silently corrupt" claim
  in the plan honest.
* **Rotation decision tree.** Unit tests on the per-blob `decide()` helper of
  `backend_rotate_service.rs`. All six cases from *The rotation job* section
  as separate tests with clear names (`legacy_upgrades_to_v1`,
  `v1_encrypted_under_head_skips`,
  `v1_encrypted_under_older_pair_rewrites`,
  `v1_plaintext_with_none_head_skips`,
  `v1_plaintext_encrypts_when_head_is_cipher`,
  `v1_encrypted_decrypts_when_head_is_none`).
* **Recoverable-job round-trip.** Integration test in
  `backend_rotate_service::tests` using the existing recoverable-run harness:
  seed N blobs (mix of legacy + v1-under-old-key), trigger rotation, assert
  every blob ends v1-with-head, `format_generation` (if we add it later) or
  the consistency-scan count reports zero legacy remaining, findings=0.
* **Crash recovery.** Same harness: interrupt mid-run at cursor position K,
  restart, assert resume from K and eventual completion with correct final
  state. Same discipline `backend_migration` already uses.
* **Concurrency safety.** Test that a `put_blob` call during a rotation
  targeting the same hash produces exactly one v1 blob at end-state (either
  the rotate's or the concurrent write's — both are head-format so the final
  state is indistinguishable). Verifies the "per-blob idempotent" claim.
* **Config-restart semantics.** Table-driven test on the pair-list state
  machine: start with pair-list [K1], add K2 → [K1,K2], run rotation,
  drop K1 → [K2], assert every blob still decrypts. Exercises the "safe to
  remove old key" transition end-to-end without going through the admin UI.

**What Hurl covers (thin — the API surface, not the mechanics):**

* Trigger endpoint auth: `POST /api/admin/storage/entries/{name}/rotate` is
  admin-only (403 for non-admins, 401 for unauthenticated).
* Trigger endpoint preconditions: 400 or similar when pairs < 2 AND no
  legacy blobs exist AND pair-list hasn't changed.
* Trigger endpoint concurrency: second POST while a run is Active returns
  409.
* Progress header: `X-Server-Status` includes a `rotation` payload during a
  running job and drops it on completion.

Hurl does NOT try to:
* Inspect on-disk bytes.
* Restart the server with a new pair-list.
* Seed a legacy blob directly.
* Verify decryption after old-key removal.

Those all belong in Rust tests where we can hold the pool, the backend, and
the config in the same test's memory.

**Test data.** Legacy-blob fixtures are byte-frozen (`tests/fixtures/*.bin`)
and committed. Don't regenerate them from live code — the whole point is that
they were produced by pre-v1 code and won't ever again be. Regeneration
scripts (kept outside `tests/`) are OK for one-time refreshes if the format
changes.

## Slices

Same discipline as `storage-multi-entry.md`. Each slice is a mergeable
increment; no slice depends on a later one.

### Slice K1 — Pair-list config parsing

**Scope.** Config layer only. No behaviour change on the write / read path
yet — the parser produces `Vec<KeyPair>` and the existing code keeps calling
`pairs.last()`.

* `NamedStorageEntry.encryption` becomes `Option<Vec<KeyPair>>` where
  `KeyPair = { cipher: CipherKind, key_material: Option<[u8; 32]> }`.
  `CipherKind` = `{ AesGcm256, None }`; `None` carries no key.
* Parser: split on `,`, per-pair split on `:` (1 or 2 parts), decode base64
  when a real cipher, reject empty list, reject duplicates, reject multiple
  `none`.
* Retire `OXICLOUD_STORAGE_<N>_ENCRYPTION_CIPHER` — parser errors on it with
  guidance to move the cipher into the pair.
* Boot logs each pair's fingerprint (`sha256(key)[..12]` for real ciphers,
  `—` for `none`) and marks the head. Warn line if head fp differs from any
  non-head fp (rotation window signal).
* Docs: `example.env`, `docs/config/env.md`, `docs/guide/backend-storage.md`
  updated with the pair syntax and the three recipes (rotate / encrypt /
  decrypt).

**Exit criteria.** Boot with a 1-pair config behaves identically to today.
Boot with a 2-pair config succeeds and logs both fingerprints. Boot with
`_ENCRYPTION_CIPHER` alongside `_ENCRYPTION_KEY` fails with a migration hint.

### Slice K2 — v1 read/write paths in `EncryptedBlobBackend`

**Scope.** The encrypted-backend decorator learns the v1 header format. Reads
dispatch on magic bytes; writes always emit v1 headers. Legacy read path
preserved verbatim. `blobs_consistency` gains a magic-byte branch to track
the legacy-blob count.

* `EncryptedBlobBackend` refactored around `BlobFormat::V1` writer + reader.
  Legacy path preserved but read-only from new code.
* Read dispatches on magic; write always emits v1 header at `<hash>.blob`.
* `blobs_consistency`: during its normal walk, per-blob magic-byte check;
  legacy count recorded on the run's `stats` JSON. No schema hit — reuses
  the existing `stats` bag on `jobs.recoverable_runs`.
* `storage_migrate`: no changes needed. It copies raw bytes between backends;
  format is preserved on the target automatically because the object bytes
  are opaque to it.

**Exit criteria.** A brand-new deployment writes only v1 blobs. An upgraded
deployment reads existing legacy blobs and writes new v1 blobs at the same
object-key. `blobs_consistency` reports a legacy-blob count in its run stats.

### Slice K3 — The `backend_rotate` recoverable job

**Scope.** New handler, admin-triggered, iterates blobs, per-blob decision
tree (legacy → v1 upgrade, v1 with old key → v1 with head key, plaintext ↔
encrypted where applicable), records findings.

* New file `src/infrastructure/services/backend_rotate_service.rs`.
* Registered in `JobRegistry` as `backend_rotate`. Runs on the same
  recoverable-runs engine (crash recovery, cursor persistence, pause/resume).
* Trigger endpoint: `POST /api/admin/storage/entries/{name}/rotate`.
  Requires admin. Refuses if no work would happen (all blobs already at
  head format + head key). Refuses if a `backend_rotate` or
  `backend_migration` run is already Active for any entry.
* Per-blob decision tree per *The rotation job* section above. In-place
  atomic replace at the same `<hash>.blob` object key.
* No readonly mode engaged. `X-Server-Status` header payload gains a
  `rotation` field alongside `migration`.

**Exit criteria.** On a filled sandbox: (1) legacy-only entry rotates to
v1; (2) encrypted entry with 2 pairs rotates so head-pair-only remains
decryptable; (3) plaintext entry with `none,cipher` head rotates to
encrypted-v1; (4) encrypted entry with `cipher,none` head rotates to
plaintext-v1. Each round-trip verified by dropping the retired pair from
config and successfully reading every blob.

### Slice K4 — Admin panel action + banner + docs

**Scope.** UI wiring, banner variant, guide docs, legacy-blob counter.

* Entry card: **Rotate encryption key** button + tooltip states as above.
* Legacy-blob counter chip on each entry card. Sources the count from the
  most recent `blobs_consistency` run's stats (already surfaced in the Jobs
  admin page). Refresh button triggers a fresh scan.
* `ReadOnlyBanner.svelte` gains a third variant `variant="rotating"` — same
  shape, milder tone (info not warning), copy makes it clear that operations
  continue.
* Server-status header payload gains `rotation?: {entry, migrated, total,
  percent}` alongside the existing `migration?:` field. `readonly` stays
  false during rotation.
* `docs/guide/backend-storage.md`: new **Rotating an encryption key**,
  **Encrypting a plaintext deployment**, and **Decrypting an encrypted
  deployment** sections with the recipes above.
* `docs/config/env.md`: pair syntax section under `_ENCRYPTION_KEY`.
* Admin panel *"Rotation complete — safe to remove the old key"* hint gated
  on findings=0 AND legacy count=0.

**Exit criteria.** Round-trip on a real deployment (Ed's OVH S3): add pair
→ restart → rotate → verify blobs (including legacy) → remove old pair →
restart → blobs still readable.

## Forward compatibility (v2 and beyond)

The `<version>` field is the single load-bearing knob for future format
changes. Reserved slots:

* `0x0001` — this plan. AES-256-GCM, server-side keys, individual blob
  integrity via AEAD tag + content-hash.
* `0x0002…0x00FF` — reserved for server-side format bumps: new AEAD,
  chain-authenticated CDC chunks (either *manifest HMAC* — no header changes,
  `chunk_manifests` gains a server-computed HMAC over the ordered chunk-hash
  list — or *Merkle root in header* — v2 grows to carry `manifest_root` +
  `chunk_index`, letting any streamed chunk be verified in isolation). Both
  defend against manifest reorder / truncation / injection attacks that
  content-addressability alone doesn't cover.
* `0x0100…0x01FF` — reserved for client-side encryption (E2E) variants.
  `<key_fp>` becomes the client-key fingerprint; the server can no longer
  decrypt and passes ciphertext through the streaming path unchanged.

v1 and v2 coexist in the same storage indefinitely — the magic-byte read
dispatch handles arbitrary versions at position 5-6. Migration between
generations reuses `backend_rotate`'s pattern: rewrite each blob with the
new-generation writer, in-place at the same object key.

## Non-goals

* **Asymmetric / KMS-backed keys.** Out of scope. Symmetric AES-256-GCM only.
  A KMS-backed variant is a separate slice built on top of this one.
* **Chunk-chain integrity / manifest tampering defense.** Recognised as a
  real gap (chunk reorder / truncate / inject via DB write). Deferred to the
  v2 header slice — see *Forward compatibility*.
* **Client-side encryption (E2E).** Same — v2 slot reserved, separate slice.
* **Automatic old-key removal.** Explicit human step, see above.
* **Rotation of a key on the "in-transit" side** (client → server TLS).
  Handled by the reverse proxy; unrelated.
* **Per-blob key derivation** (KDF from a master key + blob hash). A
  legitimate hardening path but orthogonal to the rotation story; folds in as
  a future slice under this same pair-list config.
* **Rollback safety across format generations.** If v1 code is rolled back
  to pre-v1 after new v1 blobs have been written, old code will 500 on those
  blobs (AES-GCM tag fails on OXCPT-prefixed bytes). Not addressed by this
  plan; the safety net is restore-from-backup. Deploying an
  incompatibility-breaking format change is a pre-planned event, not a hot
  rollback scenario.

## Open questions

* **Should we throttle the rotate job?** Same question `backend_migration` had.
  Answer: not in v1. If throughput bites, add a `_ROTATE_MAX_MB_PER_SEC` on
  the entry later.
* **Should rotation be idempotent under repeat trigger?** Yes. Running it a
  second time with the same config is a no-op walk (every blob is already at
  head format + head key).
* **Should we ship a `format_generation` column on `storage.blobs` as a
  query-optimizer?** No in v1. Consistency-scan stats cover the "how many
  legacy blobs remain" question at admin cadence, and per-read magic-byte
  dispatch is a 5-byte compare. If a real hot-path or admin-panel latency
  need emerges, the column is a small back-fill migration to add later.
* **Should the legacy-blob counter block key removal at code level?** No —
  keeping the trust model consistent with "admin edits .env, we don't fight
  them" is the current default. The counter + hint is enough. If real
  incidents happen we can escalate to a hard guard later.
