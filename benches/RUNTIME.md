# Tokio runtime tuning benchmark

Measures the two things `build_runtime` (`src/main.rs`) changes versus the bare
`#[tokio::main]` defaults, sized by `common::runtime::pool_sizes`:

- **Worker count.** `#[tokio::main]` defaults to `available_parallelism()`, which
  honours CPU *affinity* (`sched_getaffinity`: cpuset, `taskset`) but **ignores
  the CFS bandwidth quota** (`docker --cpus`, cgroup v2 `cpu.max`, v1
  `cpu.cfs_quota_us`). On a 2-core-quota container on a many-core host it spawns
  one worker per *host* core. `effective_parallelism()` folds the quota back in.
- **Blocking pool.** `#[tokio::main]` defaults to a flat `max_blocking_threads =
  512` — a multi-GB RSS blast radius for this heavy `spawn_blocking` user
  (thumbnails, transcode, zip, PDF/text extraction, Argon2 ≈19 MB/hash). The
  builder caps it at `max(32, 8 × workers)`.

## Reproduce

```bash
cargo build --release --features bench --example bench_tokio_runtime
# Pin to 2 cores to model a 2-core CPU quota on a bigger host:
taskset -c 0,1 ./target/release/examples/bench_tokio_runtime
# Part B uses a fixed glibc mmap threshold for a clean RSS read:
MALLOC_MMAP_THRESHOLD_=131072 MALLOC_TRIM_THRESHOLD_=131072 \
  taskset -c 0,1 ./target/release/examples/bench_tokio_runtime
# tunables: BENCH_CONCURRENCY=96 BENCH_SECONDS=4 BENCH_BURN_KB=256
#   BENCH_WORKERS_BEFORE=32  BENCH_BLOCKING_TASKS=96 BENCH_ALLOC_MB=16 BENCH_MAX_BLOCKING_AFTER=16
```

## Results (4-core box, pinned to 2 cores via `taskset -c 0,1`)

### [A] Worker over-subscription under CPU contention

96 concurrent async "requests", each an async hop + a 256 KiB BLAKE3 (models a
handler that interleaves I/O with on-worker compute), over 4 s.

| runtime               |  req/s | p50 µs | p99 µs |
|-----------------------|-------:|-------:|-------:|
| before: 32 workers    | 46 854 |    121 | 60 360 |
| after: 2 workers      | 42 893 |  2 140 |  4 962 |

→ **throughput −8.5 %, p99 latency −91.8 %** (after vs before)

### [B] Blocking-pool RSS blast radius

96 concurrent `spawn_blocking` tasks, 16 MiB resident each, held 120 ms
(fixed glibc mmap threshold so freed allocations leave RSS promptly).

| max_blocking_threads        | peak RSS MiB | vs default |
|-----------------------------|-------------:|-----------:|
| before: 512 (tokio default) |        1 231 |          — |
| after: 16 (bounded)         |          261 |   −970 MiB |

## Conclusions

1. **Blocking-pool cap — clear win, no downside.** Bounding 512→16 cut peak RSS
   under a 96-task flood from **1231 MiB to 261 MiB (−970 MiB)**. The cap only
   engages under a pile-up; steady-state operation is unaffected, and the app's
   heaviest blocking consumers are already semaphore-limited (Argon2 = 2,
   thumbnail decode ≈ cores), so `max(32, 8×workers)` is generous headroom that
   simply removes the unbounded tail that can OOM-kill the process under a spike.

2. **Worker sizing — a latency/throughput trade, favourable for a server.**
   Over-subscription (32 workers on 2 cores, what tokio's default does under a
   CFS quota) won **+8.5 % peak throughput** but at a **catastrophic p99 of
   60 ms** (12× the tuned 5 ms) with a bimodal distribution — some requests fly
   (p50 121 µs), others starve. Sizing to the quota (2 workers) gives uniform,
   predictable latency at a small throughput cost. For an interactive file
   server, p99 dominates UX (timeouts, head-of-line blocking), so this is the
   right trade.

3. **This microbenchmark is a worst case *for* the tuned config.** It is pure
   on-worker CPU, which is exactly where over-subscription's throughput edge
   shows. Real OxiCloud handlers push CPU to `spawn_blocking` and the async
   workers mostly await I/O (DB, disk) — there the over-subscription throughput
   edge evaporates (idle workers just park) while its tail-latency penalty
   remains. Production should see the worker change as ≥ neutral on throughput
   and strictly better on tail latency.

4. **No regression off-quota.** `effective_parallelism()` == `available_
   parallelism()` whenever there is no CFS quota (or affinity already restricts
   the process), so on bare metal / affinity-pinned deployments the worker count
   is unchanged from the old default. The change only bites under a CFS quota —
   precisely the case it fixes.

5. **Follow-up:** the same `available_parallelism()` blind spot affects the
   image/rayon pools (`thumbnail_service.rs`, `image_transcode_service.rs`,
   `di.rs` video) — they over-spawn under a CFS quota too. Switching those to
   `common::runtime::effective_parallelism()` is the natural next step (left out
   here to keep this change focused on the runtime).

Both knobs are env-overridable (`OXICLOUD_WORKER_THREADS` /
`OXICLOUD_MAX_BLOCKING_THREADS`) and logged at startup ("Tokio runtime pools
sized"), so operators can see and tune what is in effect.
