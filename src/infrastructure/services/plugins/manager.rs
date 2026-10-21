//! Plugin discovery + dispatch + admin management. Implements
//! [`PluginDispatchPort`] and [`PluginManagementPort`] over the Extism
//! [`PluginRuntime`].
//!
//! Discovery scans a directory of plugin subdirectories (each `plugin.toml` +
//! `.wasm`) at startup; a plugin that fails validation or load is audit-logged
//! and skipped, never fatal. Dispatch builds a fresh sandbox per invocation on
//! the blocking pool, so a slow or hostile plugin never stalls async workers or
//! the upload path that triggered it.
//!
//! The same in-memory plugin set backs both ports, guarded by an `RwLock`: a
//! management op (install / toggle / remove) takes the write lock and is
//! reflected on the live dispatch path with no restart. Enable/disable state is
//! persisted as a `.disabled` marker file in the plugin's own directory so it
//! survives a restart without a database.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use super::log_store::PluginLogStore;
use super::manifest;
use super::runtime::{InvokeOutcome, PluginRuntime};
use crate::application::ports::plugin_ports::{
    LogPage, LogQuery, OXICLOUD_PLUGIN_ABI, PluginContext, PluginDispatchPort, PluginEvent,
    PluginInfo, PluginInput, PluginLogEvent, PluginManagementPort, PluginMgmtError,
    RetentionSettings, event_export_name,
};
use crate::common::config::PluginConfig;
use tokio::sync::broadcast;

/// Name of the marker file that, when present in a plugin's directory, loads it
/// disabled. Created/removed by [`PluginManagementPort::set_enabled`].
const DISABLED_MARKER: &str = ".disabled";

/// A validated, loadable plugin held in memory.
struct LoadedPlugin {
    id: String,
    name: String,
    version: String,
    abi: u32,
    subscribe: HashSet<String>,
    /// Whether dispatch delivers events to this plugin. Mirrors the on-disk
    /// `.disabled` marker.
    enabled: bool,
    /// The plugin's own directory (not necessarily named after `id`). Used to
    /// write the disabled marker and to delete the plugin on removal.
    dir: PathBuf,
    runtime: Arc<PluginRuntime>,
}

impl LoadedPlugin {
    fn info(&self) -> PluginInfo {
        let mut subscriptions: Vec<String> = self.subscribe.iter().cloned().collect();
        subscriptions.sort();
        PluginInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            abi: self.abi,
            subscriptions,
            enabled: self.enabled,
        }
    }
}

/// Owns all loaded plugins and dispatches events to them.
pub struct ExtismPluginManager {
    config: PluginConfig,
    /// Root directory plugins are discovered in and installed into.
    root_dir: PathBuf,
    plugins: RwLock<Vec<LoadedPlugin>>,
    /// Per-plugin structured log storage (shared with the maintenance task).
    log_store: Arc<PluginLogStore>,
    /// Caps concurrent plugin invocations across all plugins so dispatch can
    /// shed load instead of flooding the shared blocking pool.
    invocation_sem: Arc<Semaphore>,
}

impl ExtismPluginManager {
    /// Scan `dir` for plugins and build a manager from those that validate and
    /// load. Returns an empty manager (logging the cause) if `dir` is absent or
    /// unreadable — a missing plugins directory is normal, not an error.
    pub fn load_from_dir(config: PluginConfig, dir: &Path) -> Self {
        // The log root is a sibling of the plugins dir by default (or the
        // configured override); it lives outside any individual plugin dir so a
        // plugin uninstall (`remove_dir_all`) never wipes another's logs.
        let log_dir = config
            .log_dir
            .clone()
            .unwrap_or_else(|| dir.join(".plugin-logs"));
        let log_store = Arc::new(PluginLogStore::new(
            log_dir.clone(),
            config.log_max_file_bytes,
            config.log_max_segments,
            RetentionSettings {
                retention_days: config.log_retention_days,
                max_bytes: config.log_total_max_bytes,
            },
            config.log_queue_capacity,
        ));
        let invocation_sem = Arc::new(Semaphore::new(config.max_concurrent_invocations.max(1)));

        let mut plugins = Vec::new();
        let mut rejected = 0usize;

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::info!(
                    target: "oxicloud::plugins",
                    dir = %dir.display(),
                    error = %e,
                    "plugins directory not readable; no plugins loaded"
                );
                return Self {
                    config,
                    root_dir: dir.to_path_buf(),
                    plugins: RwLock::new(plugins),
                    log_store,
                    invocation_sem,
                };
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Never treat the log root as a plugin directory.
            if path == log_dir {
                continue;
            }
            match Self::load_one(&config, &path) {
                Ok(loaded) => {
                    tracing::info!(
                        target: "oxicloud::plugins",
                        plugin_id = %loaded.id,
                        enabled = loaded.enabled,
                        dir = %path.display(),
                        "plugin loaded"
                    );
                    plugins.push(loaded);
                }
                Err(reason) => {
                    rejected += 1;
                    tracing::warn!(
                        target: "audit",
                        event = "plugin.load_rejected",
                        reason = reason,
                        plugin_dir = %path.display(),
                        "👮🏻‍♂️ plugin rejected at load"
                    );
                }
            }
        }

        tracing::info!(
            target: "oxicloud::plugins",
            loaded = plugins.len(),
            rejected,
            dir = %dir.display(),
            "plugin discovery complete"
        );
        Self {
            config,
            root_dir: dir.to_path_buf(),
            plugins: RwLock::new(plugins),
            log_store,
            invocation_sem,
        }
    }

    /// The shared log store, handed to the maintenance task by DI.
    pub fn log_store(&self) -> Arc<PluginLogStore> {
        self.log_store.clone()
    }

    /// Validate and load a single plugin directory. Returns a stable audit
    /// `reason` key on rejection.
    fn load_one(config: &PluginConfig, dir: &Path) -> Result<LoadedPlugin, &'static str> {
        let manifest_path = dir.join("plugin.toml");
        if !manifest_path.exists() {
            return Err("no_manifest");
        }
        let toml_str =
            std::fs::read_to_string(&manifest_path).map_err(|_| "manifest_unreadable")?;
        let manifest = manifest::parse_and_validate(&toml_str).map_err(|e| e.reason())?;

        // The entrypoint becomes a path joined onto the plugin dir; reject a
        // traversal-unsafe value on disk too (mirrors the `install` check), so a
        // hand-placed manifest can't read a `.wasm` outside its own directory.
        if !is_safe_component(&manifest.plugin.entrypoint) {
            return Err("bad_entrypoint");
        }

        let wasm_path = dir.join(&manifest.plugin.entrypoint);
        let wasm_bytes = std::fs::read(&wasm_path).map_err(|_| "wasm_unreadable")?;

        let runtime = PluginRuntime::new(manifest.plugin.id.clone(), wasm_bytes);
        // Probe a throwaway instance: abi must match AND every subscribed event
        // must have its `on_<event>` handler exported.
        let required_exports: Vec<String> = manifest
            .events
            .subscribe
            .iter()
            .map(|e| event_export_name(e))
            .collect();
        Self::probe(config, &runtime, &required_exports)?;

        Ok(LoadedPlugin {
            id: manifest.plugin.id,
            name: manifest.plugin.name,
            version: manifest.plugin.version,
            abi: manifest.plugin.abi,
            subscribe: manifest.events.subscribe.into_iter().collect(),
            enabled: !dir.join(DISABLED_MARKER).exists(),
            dir: dir.to_path_buf(),
            runtime: Arc::new(runtime),
        })
    }

    /// Probe loadability, mapping the runtime outcome to a stable reason key.
    fn probe(
        config: &PluginConfig,
        runtime: &PluginRuntime,
        required_exports: &[String],
    ) -> Result<(), &'static str> {
        match runtime.check_loadable(config, required_exports) {
            InvokeOutcome::Ok => Ok(()),
            InvokeOutcome::AbiMismatch { .. } => Err("abi_mismatch"),
            InvokeOutcome::MissingExport(_) => Err("missing_export"),
            _ => Err("not_loadable"),
        }
    }

    /// Number of successfully loaded plugins (used by DI for the startup summary
    /// and by tests).
    pub fn loaded_count(&self) -> usize {
        self.read_plugins().len()
    }

    /// Drop the cached compiled module of every plugin idle past the configured
    /// TTL, reclaiming memory. Driven by a periodic timer in DI; the next event
    /// to a freed plugin recompiles transparently.
    pub fn evict_idle_compiled(&self) {
        let ttl = Duration::from_secs(self.config.cache_idle_ttl_secs);
        let mut evicted = 0usize;
        for plugin in self.read_plugins().iter() {
            if plugin.runtime.evict_if_idle(ttl) {
                evicted += 1;
            }
        }
        if evicted > 0 {
            tracing::debug!(
                target: "oxicloud::plugins",
                evicted,
                "evicted idle compiled plugin modules"
            );
        }
    }

    fn read_plugins(&self) -> std::sync::RwLockReadGuard<'_, Vec<LoadedPlugin>> {
        self.plugins.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_plugins(&self) -> std::sync::RwLockWriteGuard<'_, Vec<LoadedPlugin>> {
        self.plugins.write().unwrap_or_else(|e| e.into_inner())
    }

    /// `NotFound` unless a plugin with this id is currently installed. Checked
    /// before any log-file access so an HTTP-supplied id can't reach the
    /// filesystem for a plugin that doesn't exist.
    fn ensure_installed(&self, id: &str) -> Result<(), PluginMgmtError> {
        if self.read_plugins().iter().any(|p| p.id == id) {
            Ok(())
        } else {
            Err(PluginMgmtError::NotFound)
        }
    }
}

impl PluginDispatchPort for ExtismPluginManager {
    fn dispatch(&self, event: PluginEvent) {
        for plugin in self.read_plugins().iter() {
            if !plugin.enabled || !plugin.subscribe.contains(event.name) {
                continue;
            }

            let input = PluginInput {
                abi: OXICLOUD_PLUGIN_ABI,
                event: event.name.to_string(),
                context: PluginContext {
                    plugin_id: plugin.id.clone(),
                    user_id: event.user_id.clone(),
                    invocation_id: event.invocation_id.clone(),
                },
                payload: event.payload.clone(),
            };
            let input_json = match serde_json::to_string(&input) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(
                        target: "oxicloud::plugins",
                        plugin_id = %plugin.id,
                        error = %e,
                        "failed to serialize plugin input; skipping"
                    );
                    continue;
                }
            };

            // Load shedding: cap concurrent invocations so a flood of events (or
            // slow plugins) can't exhaust the shared blocking pool. Past the cap
            // the event is dropped — plugins are observe-only, so shedding is
            // safe; we just record it.
            let permit = match self.invocation_sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        target: "audit",
                        event = "plugin.dispatch_shed",
                        reason = "at_capacity",
                        plugin_id = %plugin.id,
                        invocation_id = %event.invocation_id,
                        plugin_event = %event.name,
                        "👮🏻‍♂️ plugin event dropped: invocation limit reached"
                    );
                    continue;
                }
            };

            let runtime = plugin.runtime.clone();
            let config = self.config.clone();
            let plugin_id = plugin.id.clone();
            let invocation_id = event.invocation_id.clone();
            let export = event_export_name(event.name);
            let log_store = self.log_store.clone();

            // Run the synchronous wasm call off the async workers. Fire-and-forget:
            // the upload already succeeded; plugins are post-hoc observers.
            tokio::task::spawn_blocking(move || {
                // Hold the permit for the lifetime of the invocation.
                let _permit = permit;
                let result = runtime.invoke(&config, &export, &invocation_id, &input_json);
                // Persist every invocation (the plugin's own log lines plus the
                // host outcome) to the plugin's structured log. Ordered, async,
                // and non-fatal — a failed write never affects the request.
                log_store.append(&plugin_id, &invocation_id, &result.logs, &result.outcome);
                if !result.outcome.is_ok() {
                    tracing::warn!(
                        target: "audit",
                        event = "plugin.invocation_failed",
                        reason = result.outcome.reason(),
                        plugin_id = %plugin_id,
                        invocation_id = %invocation_id,
                        detail = ?result.outcome,
                        "👮🏻‍♂️ plugin invocation failed"
                    );
                }
            });
        }
    }

    fn has_subscribers(&self, event: &str) -> bool {
        self.read_plugins()
            .iter()
            .any(|p| p.enabled && p.subscribe.contains(event))
    }
}

#[async_trait]
impl PluginManagementPort for ExtismPluginManager {
    fn list(&self) -> Vec<PluginInfo> {
        let mut infos: Vec<PluginInfo> = self.read_plugins().iter().map(|p| p.info()).collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), PluginMgmtError> {
        let mut plugins = self.write_plugins();
        let plugin = plugins
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(PluginMgmtError::NotFound)?;

        let marker = plugin.dir.join(DISABLED_MARKER);
        if enabled {
            match std::fs::remove_file(&marker) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(PluginMgmtError::Io(e.to_string())),
            }
        } else {
            std::fs::write(&marker, b"").map_err(|e| PluginMgmtError::Io(e.to_string()))?;
        }
        plugin.enabled = enabled;
        Ok(())
    }

    fn install(&self, manifest_toml: &str, wasm: Vec<u8>) -> Result<PluginInfo, PluginMgmtError> {
        // Validate the manifest and the wasm before touching the filesystem.
        let manifest = manifest::parse_and_validate(manifest_toml)
            .map_err(|e| PluginMgmtError::Rejected(e.reason()))?;

        // `id` becomes a directory name and `entrypoint` a filename — both must
        // be single, traversal-free path components.
        if !is_safe_component(&manifest.plugin.id) {
            return Err(PluginMgmtError::Rejected("bad_id"));
        }
        if !is_safe_component(&manifest.plugin.entrypoint) {
            return Err(PluginMgmtError::Rejected("bad_entrypoint"));
        }

        let required_exports: Vec<String> = manifest
            .events
            .subscribe
            .iter()
            .map(|e| event_export_name(e))
            .collect();
        let runtime = PluginRuntime::new(manifest.plugin.id.clone(), wasm.clone());
        Self::probe(&self.config, &runtime, &required_exports)
            .map_err(PluginMgmtError::Rejected)?;

        let id = manifest.plugin.id.clone();
        let target = self.root_dir.join(&id);

        // Hold the write lock across the collision check and the directory swap
        // so two concurrent installs of the same id cannot race. Admin installs
        // are rare; readers block only briefly.
        let mut plugins = self.write_plugins();
        if plugins.iter().any(|p| p.id == id) || target.exists() {
            return Err(PluginMgmtError::IdExists);
        }

        std::fs::create_dir_all(&self.root_dir).map_err(|e| PluginMgmtError::Io(e.to_string()))?;
        // Write to a temp dir then rename, so a crash mid-write never leaves a
        // half-written plugin discoverable.
        let tmp = tempfile::Builder::new()
            .prefix(".tmp-install-")
            .tempdir_in(&self.root_dir)
            .map_err(|e| PluginMgmtError::Io(e.to_string()))?;
        std::fs::write(tmp.path().join("plugin.toml"), manifest_toml)
            .map_err(|e| PluginMgmtError::Io(e.to_string()))?;
        std::fs::write(tmp.path().join(&manifest.plugin.entrypoint), &wasm)
            .map_err(|e| PluginMgmtError::Io(e.to_string()))?;
        let tmp_path = tmp.keep();
        if let Err(e) = std::fs::rename(&tmp_path, &target) {
            let _ = std::fs::remove_dir_all(&tmp_path);
            return Err(PluginMgmtError::Io(e.to_string()));
        }

        let loaded = LoadedPlugin {
            id: id.clone(),
            name: manifest.plugin.name.clone(),
            version: manifest.plugin.version.clone(),
            abi: manifest.plugin.abi,
            subscribe: manifest.events.subscribe.iter().cloned().collect(),
            enabled: true,
            dir: target,
            runtime: Arc::new(runtime),
        };
        let info = loaded.info();
        plugins.push(loaded);
        Ok(info)
    }

    fn install_bundle(&self, zip: Vec<u8>) -> Result<PluginInfo, PluginMgmtError> {
        use std::io::{Cursor, Read};

        // Aggregate decompressed ceiling, enforced as each entry is unpacked so
        // a zip bomb can't blow up memory before validation (the route also caps
        // the compressed body). We only ever extract two named entries.
        let max_decompressed: u64 = self.config.max_bundle_decompressed_bytes;

        let mut archive = zip::ZipArchive::new(Cursor::new(zip))
            .map_err(|_| PluginMgmtError::Rejected("bad_zip"))?;

        // Locate `plugin.toml` — at the archive root or under a single wrapping
        // folder (e.g. `myplugin/plugin.toml`).
        // zip 9's `file_names()` returns `Result<Cow<'_, str>,
        // ZipError>` items (was `&str` in zip 8). Skip undecodeable
        // entries — matches the pre-upgrade "we never saw it" behaviour
        // and preserves the `bad_zip` → `Rejected` surface for genuinely
        // malformed bundles that fail at `ZipArchive::new` upstream.
        let manifest_name = archive
            .file_names()
            .filter_map(|r| r.ok())
            .find(|n| !n.ends_with('/') && (n == "plugin.toml" || n.ends_with("/plugin.toml")))
            .map(|n| n.into_owned())
            .ok_or(PluginMgmtError::Rejected("no_manifest_in_zip"))?;

        let mut manifest_toml = String::new();
        {
            let entry = archive
                .by_name(&manifest_name)
                .map_err(|_| PluginMgmtError::Rejected("no_manifest_in_zip"))?;
            entry
                .take(max_decompressed + 1)
                .read_to_string(&mut manifest_toml)
                .map_err(|_| PluginMgmtError::Rejected("bad_zip"))?;
        }
        if manifest_toml.len() as u64 > max_decompressed {
            return Err(PluginMgmtError::Rejected("too_large"));
        }

        // Parse just to learn the entrypoint name; `install` does the full
        // validation (and rejects a traversal-unsafe entrypoint).
        let manifest = manifest::parse_and_validate(&manifest_toml)
            .map_err(|e| PluginMgmtError::Rejected(e.reason()))?;

        // Resolve the entrypoint relative to the manifest's folder in the zip.
        let prefix = match manifest_name.rfind('/') {
            Some(i) => &manifest_name[..=i],
            None => "",
        };
        let wasm_name = format!("{prefix}{}", manifest.plugin.entrypoint);

        // Budget the wasm against what the manifest already consumed.
        let remaining = max_decompressed - manifest_toml.len() as u64;
        let mut wasm = Vec::new();
        {
            let entry = archive
                .by_name(&wasm_name)
                .map_err(|_| PluginMgmtError::Rejected("entrypoint_not_in_zip"))?;
            entry
                .take(remaining + 1)
                .read_to_end(&mut wasm)
                .map_err(|_| PluginMgmtError::Rejected("bad_zip"))?;
        }
        if wasm.len() as u64 > remaining {
            return Err(PluginMgmtError::Rejected("too_large"));
        }

        self.install(&manifest_toml, wasm)
    }

    fn remove(&self, id: &str) -> Result<(), PluginMgmtError> {
        let mut plugins = self.write_plugins();
        let pos = plugins
            .iter()
            .position(|p| p.id == id)
            .ok_or(PluginMgmtError::NotFound)?;
        let removed = plugins.remove(pos);
        // Also reclaim the plugin's logs so a later reinstall of the same id
        // doesn't inherit stale entries.
        self.log_store.remove_plugin_logs(id);
        match std::fs::remove_dir_all(&removed.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PluginMgmtError::Io(e.to_string())),
        }
    }

    async fn read_logs(&self, id: &str, query: LogQuery) -> Result<LogPage, PluginMgmtError> {
        self.ensure_installed(id)?;
        Ok(self.log_store.read_page(id, query).await)
    }

    async fn clear_logs(&self, id: &str) -> Result<(), PluginMgmtError> {
        self.ensure_installed(id)?;
        self.log_store.clear(id).await;
        Ok(())
    }

    async fn get_retention(&self, id: &str) -> Result<RetentionSettings, PluginMgmtError> {
        self.ensure_installed(id)?;
        Ok(self.log_store.get_retention(id).await)
    }

    async fn set_retention(
        &self,
        id: &str,
        settings: RetentionSettings,
    ) -> Result<(), PluginMgmtError> {
        self.ensure_installed(id)?;
        self.log_store.set_retention(id, settings).await;
        Ok(())
    }

    fn subscribe_logs(&self) -> broadcast::Receiver<PluginLogEvent> {
        self.log_store.subscribe()
    }
}

/// Whether `s` is a single, traversal-free path component safe to use as a
/// directory or file name under the plugins root.
fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
}
