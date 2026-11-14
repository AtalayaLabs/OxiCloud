# CPU pool concurrency benchmark — thumbnail decode under a CPU quota

Measures what `effective_parallelism()` changes for the image pools
(`ThumbnailService::max_concurrent_decodes`, `image_transcode_service`, `di.rs`
video ffmpeg fan-out): the number of concurrent CPU-heavy renders permitted.
Those pools used to size from `available_parallelism()`, which ignores the CFS
quota (`--cpus` / cgroup `cpu.max`), so under a container quota they permit one
render per *host* core. Drives the **real service path** — `Semaphore(K)` gating
`spawn_blocking(ThumbnailService::bench_render_all)` with a gallery of concurrent
callers — and sweeps the permit count K, measuring throughput, p50/p99, and peak
RSS for K concurrent decodes.

## Reproduce

```bash
cargo build --release --features bench --example bench_pool_concurrency
taskset -c 0,1 ./target/release/benches/bench_pool_concurrency   # model a 2-core quota
# tunables: BENCH_K_LIST=1,2,4,8,16 BENCH_GALLERY=48 BENCH_SECONDS=4
```

## Results (4-core box, pinned to 2 cores; image: synthetic 48 MP JPEG)

### [A] Throughput + tail latency (48 concurrent gallery callers)

| permits | renders/s | p50 ms | p99 ms |
|--------:|----------:|-------:|-------:|
|       1 |      16.5 |   7342 |  10370 |
|       2 (effective) | 20.0 | 5009 | 5816 |
|       4 |      20.8 |   4895 |   5536 |
|       8 |      20.0 |   4784 |   5685 |
|      16 |      18.0 |   4576 |   6140 |

### [B] Peak RSS, K concurrent decodes (one wave)

| permits | peak RSS MiB |
|--------:|-------------:|
|       1 |          137 |
|       2 |          137 |
|       4 |          137 |
|       8 |          137 |
|      16 |          137 |

## Conclusions

1. **The thumbnail-decode pool is not a bottleneck — over-permitting costs
   nothing measurable here.** Throughput is flat from K=2 to K=8 (CPU-bound: two
   cores stay saturated regardless), p99 barely moves, and **peak RSS is flat at
   137 MiB across K=1..16**. K=1 under-utilises (one decode can't fill two cores);
   K=16 is marginally worse on throughput/p99. So sizing this pool to the CFS
   quota neither gains nor loses on this workload.

2. **This confirms the codebase's own design.** `thumbnail_service.rs` documents
   that *shrink-on-load* (DCT-scaled decode straight to thumbnail size, ~18–25 MB
   regardless of source resolution) is why the historical concurrency throttle
   was removed — "the RAM ceiling no longer forces throttling and we can saturate
   every core". The flat RSS is exactly that: each concurrent decode's transient
   buffer is small, so 16 in flight cost the same resident memory as 1.

3. **Decision: NOT migrated (reverted).** Because the only pool this bench could
   isolate showed zero measured benefit, the `effective_parallelism()` migration
   of the image pools was reverted — adding code without a measured win isn't
   worth it. The `effective_parallelism()` helper stays (it has a *measured*
   benefit in the Tokio runtime — see `RUNTIME`), so a future, deliberately
   measured case can adopt it per-pool.
   The one pool with a plausible a-priori argument is the **ffmpeg video
   fan-out** (one heavyweight OS process per permit — 32 ffmpeg processes for a
   2-core budget on a many-core host is self-evidently wasteful). That was left
   on `available_parallelism()` too, to revisit *with* a measurement if a
   high-host-core / low-quota deployment running video thumbnails ever warrants
   it. The transcode rayon pool over-sizing only costs parked thread stacks
   (negligible).

4. **Honest caveat on scale.** This was run at a 2-core quota on a 4-core host
   (K_oversub = 8 ≈ 4×). On a 64-core host under a 2-core quota the host-count
   permit would be 64 (32× over), where even small per-decode costs and scheduler
   pressure add up — the regime this change protects against but which this box
   can't reproduce.
