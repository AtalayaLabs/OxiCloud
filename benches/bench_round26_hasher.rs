//! Round-26 hasher pack (no Postgres) — wall-gated, since a hasher swap changes
//! 0 allocations (the deterministic alloc counter can't score it).
//!
//!   [G1] The delta-upload "have/need" negotiation builds `HashSet`s over up to
//!        `max_chunk_count()` client-supplied 64-hex BLAKE3 hashes per request
//!        (`distinct_hashes`, `authorize_chunk_download`'s `distinct_seen`).
//!        std `HashSet` uses SipHash-1-3 (DoS-resistant but ~2-4x slower on
//!        short keys). AFTER uses `foldhash::quality::RandomState` — a faster
//!        non-cryptographic hash that STAYS DoS-resistant because it is
//!        per-instance random-seeded (the required property for these
//!        attacker-controlled inputs — not `FxHash`/fixed-seed). foldhash is
//!        already in the lockfile transitively (hashbrown), so it adds no crate.
//!        Gate: AFTER wall (build set + membership scan) strictly lower, AND
//!        two RandomState instances must seed differently (DoS resistance kept).
//!
//! Run:
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round26_hasher
//! Tunables (env): G1_HASHES (40000), G1_PASSES (50)

use std::collections::HashSet;
use std::env;
use std::hint::black_box;
use std::time::Instant;

use foldhash::quality::RandomState;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn p50(mut s: Vec<f64>) -> f64 {
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn gate(tag: &str, metric: &str, before: f64, after: f64) {
    if after >= before {
        eprintln!("GATE FAIL [{tag}] {metric}: AFTER {after} !< BEFORE {before} — rollback");
        std::process::exit(1);
    }
}

/// Deterministic 64-hex "hash" strings (mirror a BLAKE3 chunk hash).
fn hashes(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let mut s = String::with_capacity(64);
            for k in 0..8 {
                use std::fmt::Write;
                let _ = write!(
                    s,
                    "{:08x}",
                    (i as u64).wrapping_mul(2_654_435_761).wrapping_add(k)
                );
            }
            s
        })
        .collect()
}

fn main() {
    println!("# Round-26 hasher pack\n");
    let n: usize = env_or("G1_HASHES", 40_000);
    let passes: usize = env_or("G1_PASSES", 50);
    let keys = hashes(n);

    // DoS-safety: two RandomState instances must NOT hash identically (random
    // per-instance seed — precomputed-collision attacks stay infeasible).
    {
        use std::hash::{BuildHasher, Hasher};
        let (a, b) = (RandomState::default(), RandomState::default());
        let mut ha = a.build_hasher();
        let mut hb = b.build_hasher();
        std::hash::Hash::hash(&keys[0], &mut ha);
        std::hash::Hash::hash(&keys[0], &mut hb);
        if ha.finish() == hb.finish() {
            eprintln!("GATE FAIL [G1] two RandomState seeds produced the same hash — not DoS-safe");
            std::process::exit(1);
        }
    }

    // Equivalence: both build the same distinct set + same membership answers.
    let sip: HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
    let fold: HashSet<&str, RandomState> = keys.iter().map(|s| s.as_str()).collect();
    assert_eq!(sip.len(), fold.len(), "G1 distinct count differs");
    for k in &keys {
        assert_eq!(
            sip.contains(k.as_str()),
            fold.contains(k.as_str()),
            "G1 membership differs"
        );
    }

    let work_sip = || {
        let set: HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
        let mut hits = 0usize;
        for k in &keys {
            if set.contains(k.as_str()) {
                hits += 1;
            }
        }
        black_box(hits)
    };
    let work_fold = || {
        let set: HashSet<&str, RandomState> =
            HashSet::with_capacity_and_hasher(keys.len(), RandomState::default());
        let mut set = set;
        for k in &keys {
            set.insert(k.as_str());
        }
        let mut hits = 0usize;
        for k in &keys {
            if set.contains(k.as_str()) {
                hits += 1;
            }
        }
        black_box(hits)
    };

    black_box(work_sip());
    black_box(work_fold());
    let mut before = Vec::new();
    let mut after = Vec::new();
    for _ in 0..passes {
        let t = Instant::now();
        black_box(work_sip());
        before.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        black_box(work_fold());
        after.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let b = p50(before);
    let a = p50(after);
    println!("## [G1] delta-upload hash set: SipHash vs foldhash::quality ({n} hashes)");
    println!("| arm    | p50 ms (build+scan) |");
    println!("| BEFORE (SipHash)   | {b:>10.3} |");
    println!("| AFTER  (foldhash)  | {a:>10.3} |");
    println!(
        "# {:.2}x wall — DoS resistance retained (random per-instance seed)\n",
        b / a.max(1e-9)
    );
    gate("G1", "p50 ms", b, a);
    println!("Round-26 hasher section passed its gate.");
}
