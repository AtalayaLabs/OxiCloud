//! Operator-tools subcommand tree.
//!
//! Dispatched from `src/main.rs` when the first positional arg matches
//! a known domain (`opaque`, `migrate`, `storage`). Bare `oxicloud` (or
//! oxicloud with legacy top-level flags like `--config`) falls through
//! to server startup — backwards compat with existing Docker CMD lines
//! and systemd units.
//!
//! History: this tree previously lived in a standalone `oxicloud-cli`
//! binary. Folded into the main `oxicloud` binary in v0.9.0 so the
//! release tarball ships one executable. Growth pattern preserved from
//! the old bin's header — see docs/plan/bundled-binary.md § 1b.
//!
//! ## Layout
//!
//! ```text
//! oxicloud <domain> <action> [flags]
//!
//! Domains:
//!   opaque    OPAQUE aPAKE substrate management
//!               setup            Print a fresh ServerSetup value for
//!                                OXICLOUD_AUTH_OPAQUE_SERVER_SETUP
//!               reset            Clear envelope(s) so silent-migration
//!                                re-mints under current KSF
//!   migrate   One-time data migrations
//!               nfc-filenames    NFC-normalize storage.files.name
//!                                (pre-June-2026 databases)
//!   storage   Storage-config repair + crypto helpers (was --select-storage
//!             and --fingerprint before v0.9.0 CLI harmonization).
//!               select           Set the active storage-entry backend in DB
//!               fingerprint      Print SSH-style fingerprint of an AES-256 key
//! ```
//!
//! Growth pattern: each new domain gets its own module below (e.g.
//! `mod opaque`, `mod migrate`) with a `#[derive(Subcommand)]` enum
//! for its actions and a `run(action) -> u8` entrypoint. Keep
//! each module self-contained so a future extraction is a file move.
//!
//! ## Environment
//!
//! * `DATABASE_URL` — required by any subcommand that talks to the DB
//!   (`opaque reset`, `migrate nfc-filenames`); not needed for pure
//!   primitive helpers (`opaque setup`). Each subcommand documents its
//!   own dependencies.

use clap::{Parser, Subcommand};

pub mod migrate;
pub mod opaque;
pub mod storage;

#[derive(Parser)]
#[command(
    name = "oxicloud",
    version,
    about = "OxiCloud operator toolbox — subcommand entrypoint for \
             operational tasks that don't belong in the main server \
             binary. Run `oxicloud` (with no subcommand) to start the \
             server."
)]
struct Cli {
    #[command(subcommand)]
    domain: Domain,
}

#[derive(Subcommand)]
enum Domain {
    /// OPAQUE aPAKE substrate management (setup, reset).
    Opaque {
        #[command(subcommand)]
        action: opaque::Action,
    },
    /// One-time data migrations (historical schema/data fixes).
    Migrate {
        #[command(subcommand)]
        action: migrate::Action,
    },
    /// Storage-config repair + crypto helpers.
    Storage {
        #[command(subcommand)]
        action: storage::Action,
    },
}

/// Entrypoint called from `src/main.rs` after it detects a subcommand
/// on argv[1]. Builds a single-threaded tokio runtime — the operator
/// tools don't need multi-thread scheduling and starting a smaller
/// runtime keeps CLI invocations cheap.
pub fn run() -> u8 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for CLI");
    rt.block_on(async {
        let cli = Cli::parse();
        match cli.domain {
            Domain::Opaque { action } => opaque::run(action).await,
            Domain::Migrate { action } => migrate::run(action).await,
            Domain::Storage { action } => storage::run(action).await,
        }
    })
}
