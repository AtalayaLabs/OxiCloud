# Round 27 — NextCloud PROPFIND oc:id per-row buffer (alloc), contact JSONB write direct-serialize (alloc)

Two behaviour-preserving allocation cuts from the ROUND25/26 backlog, each behind
a counting-allocator BEFORE/AFTER gate that `exit(1)`s ("`GATE FAIL … rollback`")
unless AFTER allocates strictly fewer than BEFORE.

Reproduce:

```bash
RUSTFLAGS="-C target-cpu=x86-64-v3" \
  cargo run --release --features bench --example bench_round27_micro
```

---

## [H1] NextCloud PROPFIND: per-row `oc:id` String → one reused buffer per page

The streaming PROPFIND page loops built `oc:id` as a fresh `String` per child —
`format_oc_id(id, svc)` = `format!("{:08}{}", id, instance_id)` — then passed
`oc_id.as_deref()` into `write_{file,folder}_response`. The sibling per-row costs
(href, etag, dates) were already reduced to a reused buffer / borrowed events
(ROUND19 §M6, ROUND20 §C1); `oc:id` was explicitly left as the last per-row
String (ROUND20 deferred). AFTER adds `format_oc_id_into(&mut out, id, svc)` (the
0-alloc form) and computes into one `oc_buf` reused across the page, alongside the
existing `href` buffer — **1 String/row → 0** (amortized to one buffer per page).
The write functions still take `Option<&str>`, so their signatures don't change;
the emitted `oc:id` bytes are identical.

Scoped to the two **PROPFIND** page loops (the hot directory-listing path — the
most common NextCloud operation). The lower-traffic REPORT/trashbin sites and the
single-emit self-response sites are left as `format_oc_id` (see *Not shipped*).

| arm    |    ns/op | allocs/op |
|--------|---------:|----------:|
| BEFORE | 34 185.3 |  1 000.00 |
| AFTER  | 14 484.9 |      2.00 |

**998 → 0 per-row allocs (2 amortized buffers for the whole page), 2.36× wall**
over a 500-row page. Gate: AFTER allocs/op strictly lower. Equivalence: the
`oc:id` bytes from the reused buffer match `format_oc_id` for every id.

## [P2] Contact create/update: throwaway `serde_json::Value` DOM → `Json(&dtos)` direct serialize

`contact_pg_repository::{create,update}_contact` built a throwaway
`serde_json::Value` per JSONB column (`serde_json::to_value(&email_dtos)` etc.)
and bound that — sqlx re-serializes the `Value` to JSONB bytes at encode time, so
the flow was `DTOs → Value DOM (alloc) → bytes`, the tree discarded. AFTER binds
`sqlx::types::Json(&dtos)`, whose `Encode` runs `serde_json::to_writer` on the
borrowed value straight into the JSONB buffer — no intermediate DOM. This is the
write-side twin of the read-side ROUND23 §J1 fix. The old
`.unwrap_or(JsonValue::Null)` fallback was effectively dead (serializing a
`Vec<plain-struct>` can't fail).

| arm    | ns/op | allocs/op |
|--------|------:|----------:|
| BEFORE | 781.8 |     21.00 |
| AFTER  | 167.0 |      2.00 |

**21 → 2 allocs (the whole Value DOM removed), 4.68× wall** for a 3-entry column.
Gate: AFTER allocs/op strictly lower.

**Key-order note (behaviour-preserving, verified).** `serde_json::to_value` backs
the object with a sorted `Map`, so the BEFORE path emitted keys alphabetically
(`email,is_primary,type`) while direct serialize keeps struct order
(`email,type,is_primary`). This is *not* an observable change: Postgres normalizes
JSONB key order on store, so both inputs land as the **identical** stored value —
confirmed via psql (`'{…alpha…}'::jsonb = '{…struct…}'::jsonb` → `t`, both
normalizing to `{"type":…,"email":…,"is_primary":…}`) — and the read path decodes
by field name (ROUND23 §J1's `Json<Vec<Dto>>`), so the round-tripped `Contact` is
identical. The contact `etag` is computed from the domain entity before the write,
not from the stored JSONB, so it is unaffected. The benchmark's equivalence gate
asserts the two serializations decode back to the same DTOs.

---

## Not shipped — carried forward

- **`format_oc_id_into` for the REPORT + trashbin loops.** The four REPORT emit
  loops (`report_handler`) share the identical per-row-String shape and would take
  the same buffer treatment; the trashbin per-item writer (`write_trash_item_response`)
  would need the buffer threaded through its signature. Lower traffic than
  PROPFIND; deferred to keep this round's diff PROPFIND-local.
- **S3 read zero-copy forward** — needs a MinIO/stub `ByteStream` fixture.
- **Frontend folder-listing cache** — the dead `getCachedFolder`/`cacheFolder`
  SWR cache. A pure-frontend revival only saves *latency* (instant paint on
  revisit) because the `/api/folders/{id}/resources` feed carries no ETag, so the
  background revalidate still refetches the full body; the *bandwidth* win needs a
  backend `/resources` ETag + conditional 304, plus SWR wiring that respects the
  route's cursor pagination. A dedicated backend+frontend pass.

## Environment / methodology

- Counting global allocator (`benches/bench_round27_micro.rs`), no Postgres. Each
  section is BEFORE (replica of the shipped-before shape) vs AFTER (replica of the
  shipped-after shape, which the source now matches), with a value-equivalence
  assertion (H1: identical `oc:id` bytes; P2: identical serialized JSONB) and a
  `GATE FAIL … rollback` `exit(1)` if AFTER doesn't allocate fewer than BEFORE.
- Built with `RUSTFLAGS="-C target-cpu=x86-64-v3"` (the checked-in
  `.cargo/config.toml` pins `target-cpu=native`, which `SIGILL`s on this host).
- Verified beyond the bench: `cargo fmt --all --check` clean,
  `cargo clippy --features bench -- -D warnings` clean, `cargo test --lib
  --features bench` green.
