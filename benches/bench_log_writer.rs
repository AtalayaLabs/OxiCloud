//! Round-11 log-writer benchmark — synchronous fmt layer (stdout under a
//! global lock, on the async workers) vs `tracing_appender::non_blocking`
//! with `lossy(false)` (audit lines must never drop; the emitting thread
//! blocks only if the 128k-line channel fills).
//!
//! Two writer profiles:
//!   - fast: stdout redirected to /dev/null (best case for the sync arm)
//!   - slow: a writer that burns ~20 µs per line under the same lock,
//!     modelling a laggy pipe / journald / TTY consumer
//!
//! The global subscriber can only be installed once per process, so the
//! arm is chosen via env and the harness runs the binary once per arm:
//!
//!   BENCH_LOG_ARM=sync        cargo bench --features=bench --bench bench_log_writer >/dev/null
//!   BENCH_LOG_ARM=nonblocking cargo bench --features=bench --bench bench_log_writer >/dev/null
//!   BENCH_LOG_WRITER=slow BENCH_LOG_ARM=...   (slow-writer profile)
//!
//! Measurements print to stderr. Emits 4 workers × 25k events; reports
//! total wall, per-event p50/p99/p999 emit latency, and (for the
//! non-blocking arm) confirms zero dropped lines via a line count gate
//! (lossy(false) + guard flush).

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static LINES: AtomicU64 = AtomicU64::new(0);

/// Counts lines then forwards to stdout (which the run command redirects
/// to /dev/null). The `slow` profile burns ~20 µs per write while holding
/// the caller's lock, modelling a slow consumer.
struct CountingWriter {
    slow: bool,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LINES.fetch_add(1, Ordering::Relaxed);
        if self.slow {
            let t = Instant::now();
            while t.elapsed().as_micros() < 20 {
                std::hint::spin_loop();
            }
        }
        std::io::stdout().write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

#[derive(Clone)]
struct MakeCounting {
    slow: bool,
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeCounting {
    type Writer = CountingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CountingWriter { slow: self.slow }
    }
}

fn main() {
    let arm = std::env::var("BENCH_LOG_ARM").unwrap_or_else(|_| "sync".into());
    let slow = std::env::var("BENCH_LOG_WRITER").as_deref() == Ok("slow");
    let workers = 4usize;
    let per_worker = 25_000u64;

    // Same filter shape as main.rs.
    let filter = tracing_subscriber::EnvFilter::new("info,http=warn,http::web=error");

    // Keep the non-blocking guard alive for the whole run.
    let _guard: Option<tracing_appender::non_blocking::WorkerGuard> = match arm.as_str() {
        "nonblocking" => {
            let (nb, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
                .lossy(false)
                .finish(CountingWriter { slow });
            tracing_subscriber::registry()
                .with(filter)
                .with(tracing_subscriber::fmt::layer().with_writer(nb))
                .init();
            Some(guard)
        }
        _ => {
            tracing_subscriber::registry()
                .with(filter)
                .with(tracing_subscriber::fmt::layer().with_writer(MakeCounting { slow }))
                .init();
            None
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();

    let (wall, mut lat_us): (f64, Vec<f64>) = rt.block_on(async {
        let t0 = Instant::now();
        let mut handles = Vec::new();
        for w in 0..workers {
            handles.push(tokio::spawn(async move {
                let mut lats = Vec::with_capacity(per_worker as usize);
                for i in 0..per_worker {
                    let t = Instant::now();
                    tracing::info!(worker = w, seq = i, "bench log line with a few fields");
                    lats.push(t.elapsed().as_secs_f64() * 1e6);
                    if i % 512 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                lats
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.unwrap());
        }
        (t0.elapsed().as_secs_f64(), all)
    });

    // Flush (drop guard for non-blocking) before counting lines.
    drop(_guard);
    std::thread::sleep(std::time::Duration::from_millis(200));

    lat_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| lat_us[((lat_us.len() as f64 * p) as usize).min(lat_us.len() - 1)];
    let total = workers as u64 * per_worker;
    let emitted = LINES.load(Ordering::Relaxed);

    eprintln!(
        "arm={arm} writer={} events={total} wall={:.3}s  ({:.0} ev/s)",
        if slow {
            "slow(20µs)"
        } else {
            "fast(/dev/null)"
        },
        wall,
        total as f64 / wall
    );
    eprintln!(
        "  emit latency µs: p50={:.1} p99={:.1} p999={:.1} max={:.1}",
        pct(0.50),
        pct(0.99),
        pct(0.999),
        lat_us[lat_us.len() - 1]
    );
    eprintln!(
        "  gate[no lines dropped]: {}",
        if emitted >= total { "OK" } else { "FAILED" }
    );
}
