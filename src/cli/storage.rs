//! `storage` subcommand domain — storage-config repair + crypto helpers.
//!
//! Two actions today:
//! * `select <name>` — set `admin_settings.storage.active_backend_name`
//!   in the DB to the named entry and exit. Used to unblock boot after
//!   renaming or removing a storage entry in `.env` while the DB still
//!   points at the old name (the server aborts boot with a pointer to
//!   this subcommand when that happens). See
//!   `docs/plan/storage-multi-entry.md` § Fallback.
//! * `fingerprint <base64key|->` — print the SSH-style colon-hex
//!   fingerprint of a base64-encoded AES-256 key. Matches the
//!   `head_key_fp` field the `backend_rotate` job reports on completion
//!   and the raw `<key_fp>` field embedded in every v1 blob header — so
//!   an admin can pair a key in `OXICLOUD_STORAGE_<N>_ENCRYPTION_KEY`
//!   with the current on-disk head and safely drop any key whose
//!   fingerprint does NOT match the last-successful rotate.
//!
//! Both actions previously lived as top-level flags (`--select-storage`,
//! `--fingerprint`) on the `oxicloud` binary. Moved into the subcommand
//! tree in v0.9.0 for CLI consistency — see docs/plan/bundled-binary.md
//! § 1c. Behaviour is identical.

use std::env;
use std::io::Read;

use clap::Subcommand;

use crate::common::config::{AppConfig, fingerprint_from_base64_key};
use crate::infrastructure::services::entry_backend::persist_active_backend_name;

#[derive(Subcommand)]
pub enum Action {
    /// Select the active storage-entry backend. Writes
    /// `admin_settings.storage.active_backend_name = <name>` in the DB
    /// and exits. Does NOT boot the server. Use to recover from the
    /// "boot fails on missing entry" case after renaming or removing a
    /// storage entry in `.env`.
    ///
    /// The named entry MUST appear in `OXICLOUD_STORAGE_ENTRIES` — this
    /// subcommand re-parses the same env the server would parse at boot,
    /// so a successful run guarantees the subsequent boot will find the
    /// entry (no drift between the two code paths).
    Select {
        /// Storage-entry name (must appear in OXICLOUD_STORAGE_ENTRIES).
        name: String,
    },

    /// Print the SSH-style colon-hex fingerprint (16-hex, 8-byte
    /// truncation of sha256) of a base64-encoded AES-256 key.
    ///
    /// Matches the `head_key_fp` field the `backend_rotate` job reports
    /// on completion, and the raw `<key_fp>` field embedded in every v1
    /// blob header. Used to identify which key in
    /// `OXICLOUD_STORAGE_<N>_ENCRYPTION_KEY` corresponds to the current
    /// on-disk head — safe to drop any key whose fingerprint does NOT
    /// match the last-successful rotate's `head_key_fp`.
    ///
    /// Pass `-` to read the key from stdin so it never touches shell
    /// history:
    ///
    /// ```text
    /// echo -n '<base64>' | oxicloud storage fingerprint -
    /// ```
    Fingerprint {
        /// Base64-encoded AES-256 key, or `-` to read the key from stdin.
        key: String,
    },
}

pub async fn run(action: Action) -> u8 {
    match action {
        Action::Select { name } => run_select(&name).await,
        Action::Fingerprint { key } => run_fingerprint(&key),
    }
}

/// Verify `name` is declared in the current env, UPDATE
/// `admin_settings.storage.active_backend_name`, exit.
///
/// Loading AppConfig here re-runs the same env-parse the server does
/// at boot, so a successful `storage select` guarantees a subsequent
/// normal boot will find the entry — no drift between the two code
/// paths.
async fn run_select(name: &str) -> u8 {
    let config = AppConfig::from_env();
    if config.storage_entries.is_empty() {
        eprintln!(
            "OXICLOUD_STORAGE_ENTRIES is not set (or synthesised — legacy path). \
             `storage select` needs at least one named entry to switch to."
        );
        return 2;
    }
    if !config.storage_entries.iter().any(|e| e.name == name) {
        let available = config
            .storage_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "entry `{name}` is not declared in OXICLOUD_STORAGE_ENTRIES. \
             Available: [{available}]"
        );
        return 2;
    }

    let db_url = match env::var("DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "DATABASE_URL not set — `storage select` needs the same DB the server \
                 would boot on"
            );
            return 2;
        }
    };
    let pool = match sqlx::PgPool::connect(&db_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to connect to DATABASE_URL: {e}");
            return 1;
        }
    };

    if let Err(e) = persist_active_backend_name(&pool, name).await {
        eprintln!("failed to write admin_settings.storage.active_backend_name = `{name}`: {e}");
        return 1;
    }

    println!(
        "active_backend_name = `{name}` written to admin_settings. Restart the server to switch."
    );
    0
}

fn run_fingerprint(key: &str) -> u8 {
    let key_b64 = if key == "-" {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("failed to read key from stdin: {e}");
            return 2;
        }
        buf.trim().to_string()
    } else {
        key.to_string()
    };
    match fingerprint_from_base64_key(&key_b64) {
        Ok(fp) => {
            println!("{fp}");
            0
        }
        Err(e) => {
            eprintln!("storage fingerprint: {e}");
            2
        }
    }
}
