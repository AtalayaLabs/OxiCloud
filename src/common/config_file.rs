//! TOML configuration file → environment variables.
//!
//! Every setting OxiCloud reads is an `OXICLOUD_*` environment variable, in
//! `AppConfig::from_env()` and in the handful of readers that run before it
//! (`common::runtime`) or alongside it (`cookie_auth`, `trusted_proxy`). A
//! config file therefore does not replace that surface — it fills it: the
//! document is flattened into the same variables before anything reads them,
//! so one loader covers every reader, present and future, without touching
//! a single call site.
//!
//! The mapping needs no translation table: a key is the variable name with
//! the `OXICLOUD_` prefix dropped, lower-cased, and the leading table is the
//! section it belongs to.
//!
//! ```toml
//! [server]
//! server_port = 8085
//! base_path = "/cloud"
//!
//! [auth.rate_limit]      # nested tables join back into the name
//! login_max = 10         # → OXICLOUD_RATE_LIMIT_LOGIN_MAX
//! ```
//!
//! Unlike the environment, a config file can be checked: a key that does not
//! name a real setting fails the boot instead of being silently ignored.
//!
//! Precedence is the conventional one — compiled defaults < config file <
//! environment — so a variable set for the process still wins over the file.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Top-level tables, in the order the example file lists them.
pub const SECTIONS: &[&str] = &[
    "server",
    "database",
    "auth",
    "storage",
    "features",
    "integrations",
];

/// Every setting a config file may carry, as `(section, variable)`.
///
/// Kept honest by `known_table_covers_every_env_var`, which scans the source
/// for `env::var("OXICLOUD_…")` reads and fails on anything missing here.
pub const KNOWN: &[(&str, &str)] = &[
    // ── [server] ─────────────────────────────────────────────────────────
    ("server", "OXICLOUD_BASE_PATH"),
    ("server", "OXICLOUD_BASE_URL"),
    ("server", "OXICLOUD_DEFAULT_LOCALE"),
    ("server", "OXICLOUD_MAX_BLOCKING_THREADS"),
    ("server", "OXICLOUD_MESSAGEBUS_ENABLE"),
    ("server", "OXICLOUD_MESSAGEBUS_KEEPALIVE_SECONDS"),
    ("server", "OXICLOUD_METRICS_LISTEN"),
    ("server", "OXICLOUD_NEXTCLOUD_ENABLED"),
    ("server", "OXICLOUD_NEXTCLOUD_INSTANCE_ID"),
    ("server", "OXICLOUD_NEXTCLOUD_VERSION"),
    ("server", "OXICLOUD_REUSE_PORT"),
    ("server", "OXICLOUD_SERVER_HOST"),
    ("server", "OXICLOUD_SERVER_PORT"),
    ("server", "OXICLOUD_STARTUP_JOBS"),
    ("server", "OXICLOUD_STATIC_PATH"),
    ("server", "OXICLOUD_TEMP_DIR"),
    ("server", "OXICLOUD_TREE_ETAG_FLUSH_MS"),
    ("server", "OXICLOUD_TRUST_PROXY_CIDR"),
    ("server", "OXICLOUD_TRUST_PROXY_HEADERS"),
    ("server", "OXICLOUD_WEBDAV_DRIVE_LISTING_PREFIX"),
    ("server", "OXICLOUD_WORKER_THREADS"),
    // ── [database] ─────────────────────────────────────────────────────────
    ("database", "OXICLOUD_DB_CONNECTION_STRING"),
    ("database", "OXICLOUD_DB_MAINTENANCE_MAX_CONNECTIONS"),
    ("database", "OXICLOUD_DB_MAINTENANCE_MIN_CONNECTIONS"),
    ("database", "OXICLOUD_DB_MAX_CONNECTIONS"),
    ("database", "OXICLOUD_DB_MIN_CONNECTIONS"),
    ("database", "OXICLOUD_DB_POOL_MONITOR_INTERVAL_SECS"),
    ("database", "OXICLOUD_DB_STATEMENT_TIMEOUT_SECS"),
    ("database", "OXICLOUD_GRANT_CLEANUP_ENABLED"),
    ("database", "OXICLOUD_GRANT_CLEANUP_GRACE_DAYS"),
    ("database", "OXICLOUD_GRANT_CLEANUP_INTERVAL_HOURS"),
    // ── [auth] ─────────────────────────────────────────────────────────
    ("auth", "OXICLOUD_ACCESS_TOKEN_EXPIRY_SECS"),
    ("auth", "OXICLOUD_ALLOW_EXTERNAL_USERS"),
    ("auth", "OXICLOUD_AUTHZ_ENGINE"),
    ("auth", "OXICLOUD_AUTH_METHODS"),
    ("auth", "OXICLOUD_AUTH_MIN_PASSWORD_LENGTH"),
    ("auth", "OXICLOUD_AUTH_OPAQUE_KSF_ITERATIONS"),
    ("auth", "OXICLOUD_AUTH_OPAQUE_KSF_MEMORY_KIB"),
    ("auth", "OXICLOUD_AUTH_OPAQUE_KSF_PARALLELISM"),
    ("auth", "OXICLOUD_AUTH_OPAQUE_MODE"),
    ("auth", "OXICLOUD_AUTH_OPAQUE_SERVER_SETUP"),
    ("auth", "OXICLOUD_AUTH_POLICIES"),
    ("auth", "OXICLOUD_COOKIE_SECURE"),
    ("auth", "OXICLOUD_DISABLE_REGISTRATION"),
    ("auth", "OXICLOUD_DPOP_MODE"),
    ("auth", "OXICLOUD_EXPOSE_SYSTEM_USERS"),
    ("auth", "OXICLOUD_EXTERNAL_EMAIL_DOMAINS"),
    ("auth", "OXICLOUD_HASH_MEMORY_COST"),
    ("auth", "OXICLOUD_HASH_PARALLELISM"),
    ("auth", "OXICLOUD_HASH_TIME_COST"),
    ("auth", "OXICLOUD_JWT_SECRET"),
    ("auth", "OXICLOUD_LOCKOUT_DURATION_SECS"),
    ("auth", "OXICLOUD_LOCKOUT_MAX_FAILURES"),
    ("auth", "OXICLOUD_MAGIC_LINK_INVITE_PER_CALLER_PER_HOUR"),
    ("auth", "OXICLOUD_MAGIC_LINK_INVITE_TTL_HOURS"),
    ("auth", "OXICLOUD_MAGIC_LINK_LOGIN_TTL_MINUTES"),
    ("auth", "OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS"),
    ("auth", "OXICLOUD_MAGIC_LINK_SEND_PER_EMAIL_PER_HOUR"),
    ("auth", "OXICLOUD_MAGIC_LINK_SEND_PER_IP_PER_HOUR"),
    ("auth", "OXICLOUD_MAGIC_LINK_TTL_HOURS"),
    ("auth", "OXICLOUD_OIDC_ADMIN_GROUPS"),
    ("auth", "OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH"),
    ("auth", "OXICLOUD_OIDC_AUTO_PROVISION"),
    ("auth", "OXICLOUD_OIDC_CLIENT_ID"),
    ("auth", "OXICLOUD_OIDC_CLIENT_SECRET"),
    ("auth", "OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN"),
    ("auth", "OXICLOUD_OIDC_ENABLED"),
    ("auth", "OXICLOUD_OIDC_FRONTEND_URL"),
    ("auth", "OXICLOUD_OIDC_ISSUER_URL"),
    ("auth", "OXICLOUD_OIDC_PROVIDER_NAME"),
    ("auth", "OXICLOUD_OIDC_REDIRECT_URI"),
    ("auth", "OXICLOUD_OIDC_SCOPES"),
    ("auth", "OXICLOUD_RATE_LIMIT_DELTA_UPLOAD_MAX"),
    ("auth", "OXICLOUD_RATE_LIMIT_DELTA_UPLOAD_WINDOW_SECS"),
    ("auth", "OXICLOUD_RATE_LIMIT_LOGIN_MAX"),
    ("auth", "OXICLOUD_RATE_LIMIT_LOGIN_WINDOW_SECS"),
    ("auth", "OXICLOUD_RATE_LIMIT_REFRESH_MAX"),
    ("auth", "OXICLOUD_RATE_LIMIT_REFRESH_WINDOW_SECS"),
    ("auth", "OXICLOUD_RATE_LIMIT_REGISTER_MAX"),
    ("auth", "OXICLOUD_RATE_LIMIT_REGISTER_WINDOW_SECS"),
    ("auth", "OXICLOUD_RATE_LIMIT_USER_PROFILE_MAX"),
    ("auth", "OXICLOUD_RATE_LIMIT_USER_PROFILE_WINDOW_SECS"),
    ("auth", "OXICLOUD_REFRESH_TOKEN_EXPIRY_SECS"),
    ("auth", "OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS"),
    ("auth", "OXICLOUD_REQUIRE_VERIFIED_EMAIL"),
    ("auth", "OXICLOUD_SHARE_SESSION_EXPIRY_SECS"),
    // ── [storage] ─────────────────────────────────────────────────────────
    ("storage", "OXICLOUD_AZURE_ACCOUNT_KEY"),
    ("storage", "OXICLOUD_AZURE_ACCOUNT_NAME"),
    ("storage", "OXICLOUD_AZURE_CONTAINER"),
    ("storage", "OXICLOUD_AZURE_ENDPOINT_URL"),
    ("storage", "OXICLOUD_AZURE_SAS_TOKEN"),
    ("storage", "OXICLOUD_CHUNK_DIR"),
    ("storage", "OXICLOUD_CHUNK_MAX_BYTES"),
    ("storage", "OXICLOUD_DIRECT_PUT_MAX_BYTES"),
    ("storage", "OXICLOUD_INGEST_OVERLAP"),
    ("storage", "OXICLOUD_LEGACY_RECHUNK"),
    ("storage", "OXICLOUD_LOCAL_READ_PREFETCH"),
    ("storage", "OXICLOUD_MAX_UPLOAD_SIZE"),
    ("storage", "OXICLOUD_S3_ACCESS_KEY"),
    ("storage", "OXICLOUD_S3_BUCKET"),
    ("storage", "OXICLOUD_S3_ENDPOINT_URL"),
    ("storage", "OXICLOUD_S3_FORCE_PATH_STYLE"),
    ("storage", "OXICLOUD_S3_REGION"),
    ("storage", "OXICLOUD_S3_SECRET_KEY"),
    ("storage", "OXICLOUD_STORAGE_BACKEND"),
    ("storage", "OXICLOUD_STORAGE_CACHE_ENABLED"),
    ("storage", "OXICLOUD_STORAGE_CACHE_MAX_SIZE"),
    ("storage", "OXICLOUD_STORAGE_CACHE_PATH"),
    ("storage", "OXICLOUD_STORAGE_ENCRYPTION_ENABLED"),
    ("storage", "OXICLOUD_STORAGE_ENCRYPTION_KEY"),
    ("storage", "OXICLOUD_STORAGE_ENTRIES"),
    ("storage", "OXICLOUD_STORAGE_PATH"),
    ("storage", "OXICLOUD_STORAGE_RETRY_BACKOFF_MULTIPLIER"),
    ("storage", "OXICLOUD_STORAGE_RETRY_ENABLED"),
    ("storage", "OXICLOUD_STORAGE_RETRY_INITIAL_BACKOFF_MS"),
    ("storage", "OXICLOUD_STORAGE_RETRY_MAX_BACKOFF_MS"),
    ("storage", "OXICLOUD_STORAGE_RETRY_MAX_RETRIES"),
    ("storage", "OXICLOUD_STORAGE_USAGE_RECONCILE_SECS"),
    // ── [features] ─────────────────────────────────────────────────────────
    ("features", "OXICLOUD_CONTENT_INDEX_DIR"),
    ("features", "OXICLOUD_CONTENT_INDEX_FLUSH_MS"),
    ("features", "OXICLOUD_CONTENT_INDEX_MAX_FILE_BYTES"),
    ("features", "OXICLOUD_CONTENT_INDEX_MAX_TEXT_BYTES"),
    ("features", "OXICLOUD_ENABLE_AUTH"),
    ("features", "OXICLOUD_ENABLE_CONTENT_SEARCH"),
    ("features", "OXICLOUD_ENABLE_EXTERNAL_MOUNTS"),
    ("features", "OXICLOUD_ENABLE_FACES"),
    ("features", "OXICLOUD_ENABLE_FILE_SHARING"),
    ("features", "OXICLOUD_ENABLE_MUSIC"),
    ("features", "OXICLOUD_ENABLE_PLACES"),
    ("features", "OXICLOUD_ENABLE_PLUGINS"),
    ("features", "OXICLOUD_ENABLE_SEARCH"),
    ("features", "OXICLOUD_ENABLE_TRASH"),
    ("features", "OXICLOUD_ENABLE_VIDEO_THUMBNAILS"),
    ("features", "OXICLOUD_FACES_DETECTOR_MODEL"),
    ("features", "OXICLOUD_FACES_DET_SIZE"),
    ("features", "OXICLOUD_FACES_DET_THRESHOLD"),
    ("features", "OXICLOUD_FACES_EMBEDDER_MODEL"),
    ("features", "OXICLOUD_FACES_INDEX_CONCURRENCY"),
    ("features", "OXICLOUD_FACES_INTRA_THREADS"),
    ("features", "OXICLOUD_FACES_NMS_THRESHOLD"),
    ("features", "OXICLOUD_FACES_ORT_DYLIB"),
    ("features", "OXICLOUD_FFMPEG_PATH"),
    ("features", "OXICLOUD_NOTIFICATIONS_RETENTION_DAYS"),
    ("features", "OXICLOUD_NOTIFY_INTERNAL_USERS_ON_SHARE"),
    ("features", "OXICLOUD_PLUGINS_DIR"),
    ("features", "OXICLOUD_PLUGIN_CACHE_IDLE_TTL_SECS"),
    ("features", "OXICLOUD_PLUGIN_LOG_DIR"),
    ("features", "OXICLOUD_PLUGIN_LOG_MAX_FILE_BYTES"),
    ("features", "OXICLOUD_PLUGIN_LOG_MAX_SEGMENTS"),
    ("features", "OXICLOUD_PLUGIN_LOG_QUEUE_CAPACITY"),
    ("features", "OXICLOUD_PLUGIN_LOG_RETENTION_DAYS"),
    ("features", "OXICLOUD_PLUGIN_LOG_TOTAL_MAX_BYTES"),
    ("features", "OXICLOUD_PLUGIN_MAX_BUNDLE_DECOMPRESSED_BYTES"),
    ("features", "OXICLOUD_PLUGIN_MAX_CONCURRENT_INVOCATIONS"),
    ("features", "OXICLOUD_PLUGIN_MAX_INPUT_BYTES"),
    ("features", "OXICLOUD_PLUGIN_MAX_MEMORY_PAGES"),
    ("features", "OXICLOUD_PLUGIN_TIMEOUT_MS"),
    ("features", "OXICLOUD_SEARCH_CACHE_MAX_BYTES"),
    ("features", "OXICLOUD_VIDEO_THUMBNAIL_CONCURRENCY"),
    ("features", "OXICLOUD_VIDEO_THUMBNAIL_MAX_MB"),
    ("features", "OXICLOUD_VIDEO_THUMBNAIL_TIMEOUT_SECS"),
    // ── [integrations] ─────────────────────────────────────────────────────────
    ("integrations", "OXICLOUD_SMTP_FROM"),
    ("integrations", "OXICLOUD_SMTP_HOST"),
    ("integrations", "OXICLOUD_SMTP_MOCK"),
    ("integrations", "OXICLOUD_SMTP_PASS"),
    ("integrations", "OXICLOUD_SMTP_PORT"),
    ("integrations", "OXICLOUD_SMTP_TLS"),
    ("integrations", "OXICLOUD_SMTP_USER"),
    ("integrations", "OXICLOUD_WOPI_BASE_URL"),
    ("integrations", "OXICLOUD_WOPI_DISCOVERY_URL"),
    ("integrations", "OXICLOUD_WOPI_ENABLED"),
    ("integrations", "OXICLOUD_WOPI_LOCK_TTL_SECS"),
    ("integrations", "OXICLOUD_WOPI_PUBLIC_BASE_URL"),
    ("integrations", "OXICLOUD_WOPI_SECRET"),
    ("integrations", "OXICLOUD_WOPI_TOKEN_TTL_SECS"),
];

/// Keys a `[storage.entries.<name>]` table may carry. The entry name is the
/// operator's own label, so these variables are built per entry
/// (`OXICLOUD_STORAGE_<name>_BACKEND`) and cannot be listed in [`KNOWN`].
pub const ENTRY_KEYS: &[&str] = &[
    "AZURE_ACCOUNT_KEY",
    "AZURE_ACCOUNT_NAME",
    "AZURE_CONTAINER",
    "AZURE_ENDPOINT_URL",
    "AZURE_SAS_TOKEN",
    "BACKEND",
    "ENCRYPTION_KEY",
    "ROOT_DIR",
    "S3_ACCESS_KEY",
    "S3_BUCKET",
    "S3_ENDPOINT_URL",
    "S3_FORCE_PATH_STYLE",
    "S3_REGION",
    "S3_SECRET_KEY",
];

/// What went wrong while reading a config file.
#[derive(Debug)]
pub enum ConfigFileError {
    /// The file could not be read.
    Read(String),
    /// The document is not valid TOML.
    Parse(String),
    /// A table, key or value the schema does not allow.
    Schema(String),
}

impl fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(m) | Self::Parse(m) | Self::Schema(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ConfigFileError {}

type Result<T> = std::result::Result<T, ConfigFileError>;

/// The variable a dotted key path maps to: the path minus its section,
/// upper-cased and prefixed.
fn env_name(path: &[String]) -> String {
    let mut name = String::from("OXICLOUD");
    for segment in &path[1..] {
        name.push('_');
        name.push_str(&segment.to_uppercase());
    }
    name
}

/// Render a scalar the way the environment would carry it. Arrays join with
/// `,` — the separator every list-valued variable already parses.
fn render(value: &toml::Value, path: &str) -> Result<String> {
    Ok(match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(x) => x.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                if matches!(item, toml::Value::Array(_) | toml::Value::Table(_)) {
                    return Err(ConfigFileError::Schema(format!(
                        "`{path}`: a list may only hold strings, numbers or booleans"
                    )));
                }
                parts.push(render(item, path)?);
            }
            parts.join(",")
        }
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Table(_) => unreachable!("tables are walked, not rendered"),
    })
}

/// Flatten a parsed document into `(variable, value)` pairs.
fn walk(
    table: &toml::Table,
    path: &mut Vec<String>,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    for (key, value) in table {
        path.push(key.clone());
        if let toml::Value::Table(inner) = value {
            if path.len() == 1 && !SECTIONS.contains(&key.as_str()) {
                return Err(ConfigFileError::Schema(format!(
                    "`[{key}]` is not a config section — expected one of {}",
                    SECTIONS.join(", ")
                )));
            }
            walk(inner, path, out)?;
        } else {
            if path.len() < 2 {
                return Err(ConfigFileError::Schema(format!(
                    "`{key}` sits outside every section — put it under one of {}",
                    SECTIONS.join(", ")
                )));
            }
            let dotted = path.join(".");
            if path.len() > 2 && path[0] == "storage" && path[1] == "entries" {
                // `[storage.entries.<name>]` — the middle segment is the
                // operator's label and keeps its case; the variables are
                // built per entry, so they are checked against ENTRY_KEYS.
                if path.len() != 4 {
                    return Err(ConfigFileError::Schema(format!(
                        "`{dotted}`: a storage entry is `[storage.entries.<name>]` with plain keys"
                    )));
                }
                let suffix = path[3].to_uppercase();
                if !ENTRY_KEYS.contains(&suffix.as_str()) {
                    return Err(ConfigFileError::Schema(format!(
                        "`{dotted}` is not a storage-entry setting — expected one of {}",
                        ENTRY_KEYS.join(", ").to_lowercase()
                    )));
                }
                let entry = &path[2];
                out.insert(
                    format!("OXICLOUD_STORAGE_{entry}_{suffix}"),
                    render(value, &dotted)?,
                );
                path.pop();
                continue;
            }
            let name = env_name(path);
            let section = &path[0];
            match KNOWN.iter().find(|(_, var)| *var == name) {
                Some((owner, _)) if owner == section => {
                    out.insert(name, render(value, &dotted)?);
                }
                Some((owner, _)) => {
                    return Err(ConfigFileError::Schema(format!(
                        "`{dotted}` names `{name}`, which belongs to `[{owner}]`, not `[{section}]`"
                    )));
                }
                None => {
                    return Err(ConfigFileError::Schema(format!(
                        "`{dotted}` is not a setting (it would be `{name}`)"
                    )));
                }
            }
        }
        path.pop();
    }
    Ok(())
}

/// Parse a TOML document into the environment variables it sets.
pub fn flatten(document: &str) -> Result<BTreeMap<String, String>> {
    let table: toml::Table = document
        .parse()
        .map_err(|e| ConfigFileError::Parse(format!("{e}")))?;
    let mut out = BTreeMap::new();
    walk(&table, &mut Vec::new(), &mut out)?;
    Ok(out)
}

/// How many settings a config file contributed, and how many it yielded to
/// the environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Settings exported from the file.
    pub set: usize,
    /// Settings the file declares that were already in the environment, which
    /// therefore keeps its value.
    pub overridden: usize,
}

/// Read `path` and export everything it declares that the environment does
/// not already carry.
///
/// The environment wins: defaults < config file < environment. A variable set
/// for the process is the more specific, more immediate instruction — that is
/// what `-e` on a container, a systemd `Environment=` line or a one-off shell
/// export means, and a config file is the baseline it adjusts.
pub fn apply(path: &Path) -> Result<Applied> {
    let document = std::fs::read_to_string(path)
        .map_err(|e| ConfigFileError::Read(format!("failed to read {}: {e}", path.display())))?;
    let settings = flatten(&document)?;
    let mut applied = Applied::default();
    for (name, value) in &settings {
        if std::env::var_os(name).is_some() {
            applied.overridden += 1;
            continue;
        }
        // SAFETY: called once during startup, from `main` before any thread
        // is spawned and before anything reads the configuration.
        unsafe { std::env::set_var(name, value) };
        applied.set += 1;
    }
    Ok(applied)
}

/// Whether a path should be read as TOML rather than as a `.env` file.
pub fn is_toml_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_the_variable_that_spells_them() {
        let vars = flatten(
            r#"
            [server]
            server_port = 8085
            static_path = "/srv/oxicloud/static"

            [auth]
            auth_methods = ["password", "oidc"]
            require_verified_email = true
            "#,
        )
        .unwrap();
        assert_eq!(vars["OXICLOUD_SERVER_PORT"], "8085");
        assert_eq!(vars["OXICLOUD_STATIC_PATH"], "/srv/oxicloud/static");
        assert_eq!(vars["OXICLOUD_AUTH_METHODS"], "password,oidc");
        assert_eq!(vars["OXICLOUD_REQUIRE_VERIFIED_EMAIL"], "true");
    }

    /// A nested table is a grouping aid: the segments join back into the same
    /// variable the flat spelling would produce.
    #[test]
    fn nested_tables_join_into_the_variable_name() {
        let nested = flatten(
            r#"
            [auth.rate_limit]
            login_max = 10

            [storage.s3]
            bucket = "oxi"
            "#,
        )
        .unwrap();
        assert_eq!(nested["OXICLOUD_RATE_LIMIT_LOGIN_MAX"], "10");
        assert_eq!(nested["OXICLOUD_S3_BUCKET"], "oxi");

        let flat = flatten(
            r#"
            [auth]
            rate_limit_login_max = 10

            [storage]
            s3_bucket = "oxi"
            "#,
        )
        .unwrap();
        assert_eq!(nested, flat);
    }

    /// Multi-entry storage keeps the operator's own entry label, so those
    /// variables are built per entry rather than listed in KNOWN.
    #[test]
    fn storage_entries_carry_their_label() {
        let vars = flatten(
            r#"
            [storage.entries.local_main]
            backend = "local"
            root_dir = "/srv/oxicloud/blobs"

            [storage.entries.s3_cold]
            backend = "s3"
            s3_bucket = "cold"
            "#,
        )
        .unwrap();
        assert_eq!(vars["OXICLOUD_STORAGE_local_main_BACKEND"], "local");
        assert_eq!(
            vars["OXICLOUD_STORAGE_local_main_ROOT_DIR"],
            "/srv/oxicloud/blobs"
        );
        assert_eq!(vars["OXICLOUD_STORAGE_s3_cold_S3_BUCKET"], "cold");
        assert!(flatten("[storage.entries.a]\nnope = 1\n").is_err());
    }

    /// The point of a schema: a misspelt setting stops the boot instead of
    /// reverting to its default in silence.
    #[test]
    fn unknown_keys_are_rejected() {
        let err = flatten("[auth]\njwt_secrets = 'x'\n").unwrap_err();
        assert!(
            err.to_string().contains("OXICLOUD_JWT_SECRETS"),
            "unhelpful error: {err}"
        );
    }

    /// A real setting filed under the wrong section names the right one.
    #[test]
    fn misplaced_keys_name_their_section() {
        let err = flatten("[server]\njwt_secret = 'x'\n").unwrap_err();
        assert!(err.to_string().contains("[auth]"), "unhelpful error: {err}");
    }

    #[test]
    fn unknown_sections_and_loose_keys_are_rejected() {
        assert!(flatten("[nope]\nx = 1\n").is_err());
        assert!(flatten("server_port = 8085\n").is_err());
    }

    #[test]
    fn toml_paths_are_recognised_by_extension() {
        assert!(is_toml_path(Path::new("/etc/oxicloud/config.TOML")));
        assert!(!is_toml_path(Path::new("/etc/oxicloud/prod.env")));
    }

    /// Every section in [`SECTIONS`] owns at least one setting, and every
    /// entry in [`KNOWN`] belongs to a section that exists.
    #[test]
    fn sections_and_table_agree() {
        for (section, var) in KNOWN {
            assert!(
                SECTIONS.contains(section),
                "{var} sits in unknown section {section}"
            );
        }
        for section in SECTIONS {
            assert!(
                KNOWN.iter().any(|(s, _)| s == section),
                "section {section} owns nothing"
            );
        }
    }

    /// The shipped example is the first thing an operator copies, so it must
    /// satisfy the same schema the server enforces.
    #[test]
    fn shipped_example_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/config/oxicloud.example.toml");
        let document = std::fs::read_to_string(&path).expect("example config is shipped");
        let vars = flatten(&document).expect("example config parses against the schema");
        assert_eq!(vars["OXICLOUD_SERVER_PORT"], "8086");
        assert_eq!(vars["OXICLOUD_STORAGE_local_main_BACKEND"], "local");
    }

    /// The table is only useful while it covers what the code reads. Scan the
    /// tree for `env::var("OXICLOUD_…")` and fail on anything unlisted — a new
    /// knob must be reachable from a config file, not just from the shell.
    #[test]
    fn known_table_covers_every_env_var() {
        fn scan(dir: &Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("readable source dir") {
                let path = entry.expect("readable entry").path();
                if path.is_dir() {
                    scan(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let body = std::fs::read_to_string(&path).expect("readable source file");
                    for (idx, _) in body.match_indices("env::var") {
                        let rest = &body[idx..];
                        let Some(open) = rest.find('"') else { continue };
                        let Some(close) = rest[open + 1..].find('"') else {
                            continue;
                        };
                        let name = &rest[open + 1..open + 1 + close];
                        let plain = name
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                        if name.starts_with("OXICLOUD_") && plain && !rest[..open].contains(';') {
                            found.push(name.to_string());
                        }
                    }
                }
            }
        }

        // `OXICLOUD_CONFIG` names the file itself — it cannot be set from
        // inside it, so it is deliberately absent from the table.
        const META: &[&str] = &["OXICLOUD_CONFIG"];

        let mut found = Vec::new();
        scan(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut found,
        );
        found.sort();
        found.dedup();
        assert!(found.len() > 100, "the scan found suspiciously little");
        let missing: Vec<_> = found
            .iter()
            .filter(|name| !META.contains(&name.as_str()))
            .filter(|name| !KNOWN.iter().any(|(_, var)| var == name))
            .collect();
        assert!(
            missing.is_empty(),
            "read from the environment but absent from KNOWN: {missing:?}"
        );
    }
}
