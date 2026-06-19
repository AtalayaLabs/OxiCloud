//! build.rs — Static-asset pipeline for OxiCloud
//!
//! **Release mode** (`cargo build --release`):
//!   1. Copies `static/` → `static-dist/` (processed mirror).
//!   2. Resolves CSS `@import` chains → flat `main.css`.
//!   3. Bundles all index.html CSS/JS → `app.{hash}.css` / `app.{hash}.js`.
//!   4. Minifies every `.css` (lightningcss) and `.js` (oxc).
//!   5. Rewrites `index.html` with bundled asset paths.
//!   6. Minifies locale JSON files.
//!   7. Updates `sw.js` cache manifest.
//!   8. Writes HTML files to `$OUT_DIR` for `include_str!()`.
//!
//! **Debug mode** (`cargo build`):
//!   • Copies HTML files to `$OUT_DIR` for `include_str!()` only.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

// ─── HTML files embedded via include_str!() in Rust source ───────────────────
const HTML_INCLUDE: &[&str] = &[
    "login.html",
    "profile.html",
    "admin.html",
    "device-verify.html",
    "nextcloud-login.html",
    "share.html",
];

// ═══════════════════════════════════════════════════════════════════════════════
// Entry point
// ═══════════════════════════════════════════════════════════════════════════════

fn main() {
    let manifest_dir = penv("CARGO_MANIFEST_DIR");
    let out_dir = penv("OUT_DIR");
    let static_dir = manifest_dir.join("static");

    println!("cargo:rerun-if-changed=static");
    println!("cargo:rerun-if-changed=build.rs");

    git_status();

    // ── Guard: Docker cacher stage has no static/ ────────────────────────────
    if !static_dir.exists() {
        for name in HTML_INCLUDE {
            let _ = fs::write(out_dir.join(name), "");
        }
        return;
    }

    let is_release = env_or("PROFILE", "debug") == "release";

    if is_release {
        process_release(&manifest_dir, &static_dir, &out_dir);
    } else {
        // Debug: copy original HTML for include_str!()
        for name in HTML_INCLUDE {
            let src = static_dir.join(name);
            if src.exists() {
                let _ = fs::copy(&src, out_dir.join(name));
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Grab git values
// Support Github,  is treated (need upgrade if move to gitlab CircleCI, ...)
// ═══════════════════════════════════════════════════════════════════════════════
fn git_status() {
    // Rerun the build script when the commit or branch changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    let git_hash = first_env(&["GITHUB_SHA", "CI_COMMIT_SHA", "CIRCLE_SHA1", "GIT_COMMIT"])
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    let git_branch = first_env(&[
        "GITHUB_HEAD_REF",    // GitHub: PR source branch (empty on push)
        "GITHUB_REF_NAME",    // GitHub: branch/tag on push
        "CI_COMMIT_REF_NAME", // GitLab
        "CIRCLE_BRANCH",      // CircleCI
        "GIT_BRANCH",         // Jenkins
    ])
    .or_else(|| git(&["rev-parse", "--abbrev-ref", "HEAD"]))
    .filter(|b| b != "HEAD") // detached HEAD is not a real branch name
    .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_BRANCH={git_branch}");

    // CI builds: rerun if the injected env changes
    for k in [
        "GITHUB_SHA",
        "GITHUB_HEAD_REF",
        "GITHUB_REF_NAME",
        "CI_COMMIT_SHA",
        "CI_COMMIT_REF_NAME",
        "CIRCLE_SHA1",
        "CIRCLE_BRANCH",
        "GIT_COMMIT",
        "GIT_BRANCH",
    ] {
        println!("cargo:rerun-if-env-changed={k}");
    }

    println!("cargo:warning=OxiCloud built with git hash: {git_hash} and branch: {git_branch}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then_some(())?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| env::var(k).ok())
        .filter(|s| !s.is_empty())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Release pipeline
// ═══════════════════════════════════════════════════════════════════════════════

fn process_release(manifest_dir: &Path, static_dir: &Path, out_dir: &Path) {
    let dist_dir = manifest_dir.join("static-dist");

    // Start fresh
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir).expect("clean static-dist");
    }

    // 1. Mirror static/ → static-dist/
    copy_dir_recursive(static_dir, &dist_dir).expect("copy static → static-dist");

    let css_dir = static_dir.join("css");

    // Read index.html once — used for both CSS and JS extraction.
    let index_html = fs::read_to_string(static_dir.join("index.html")).expect("read index.html");

    // ── 2. Resolve main.css @imports ─────────────────────────────────────────
    let resolved_main = resolve_css_imports(&css_dir.join("main.css"), &css_dir);
    let minified_main = css_minify_safe(&resolved_main);
    fs::write(dist_dir.join("css/main.css"), &minified_main).expect("write main.css");

    // ── 3. Build CSS bundle for index.html ───────────────────────────────────
    // Derive the list of view CSS files directly from the <link> tags in index.html
    // so build.rs never needs to be updated when a new stylesheet is added.
    let mut css_all = resolved_main;
    for view in extract_css_links(&index_html) {
        let p = css_dir.join(&view);
        if p.exists() {
            css_all.push_str(&fs::read_to_string(&p).unwrap_or_default());
            css_all.push('\n');
        } else {
            eprintln!(
                "cargo:warning=CSS link in index.html not found: {}",
                p.display()
            );
        }
    }
    let css_bundle = css_minify_safe(&css_all);
    let css_hash = fnv_hash(css_bundle.as_bytes());
    let css_name = format!("app.{css_hash}.css");
    fs::write(dist_dir.join("css").join(&css_name), &css_bundle).expect("write css bundle");

    // ── 4. Minify ALL individual CSS in static-dist/ ─────────────────────────
    minify_tree_css(&dist_dir.join("css"));

    // ── 5. Bundle all ES modules into one IIFE ───────────────────────────────
    // Walk the import graph starting from every <script type="module"> in index.html,
    // strip import/export syntax, wrap in an IIFE, then minify as a classic script.
    let module_scripts = extract_module_scripts(&index_html);
    let js_raw = build_js_module_bundle(&module_scripts, static_dir);
    // Validate the raw bundle with OXC before minifying — catches re-declaration
    // collisions and other syntax errors that would silently survive minification.
    js_bundle_validate(&js_raw);
    let js_bundle = js_minify_script_safe(&js_raw);
    let js_hash = fnv_hash(js_bundle.as_bytes());
    let js_name = format!("app.{js_hash}.js");
    fs::create_dir_all(dist_dir.join("js")).expect("js dir");
    fs::write(dist_dir.join("js").join(&js_name), &js_bundle).expect("write js bundle");

    // ── 6. Minify ALL individual JS files in static-dist/ ────────────────────
    minify_tree_js(&dist_dir.join("js"));

    // ── 7. Rewrite index.html ────────────────────────────────────────────────
    // Inline the tiny render-blocking theme-init.js (classic script) so the
    // critical path drops a request. Other pages keep the external reference.
    let theme_init_min = js_minify_script_safe(
        &fs::read_to_string(static_dir.join("js/core/theme-init.js")).unwrap_or_default(),
    );
    let rewritten_index = rewrite_index_html(
        &index_html,
        &format!("/css/{css_name}"),
        &format!("/js/{js_name}"),
        &theme_init_min,
    );
    fs::write(dist_dir.join("index.html"), &rewritten_index).expect("write dist index.html");

    // ── 8. Minify locale JSONs ───────────────────────────────────────────────
    minify_tree_json(&dist_dir.join("locales"));

    // ── 9. Update & minify sw.js ─────────────────────────────────────────────
    let sw = fs::read_to_string(dist_dir.join("sw.js")).unwrap_or_default();
    let sw_updated = update_sw_cache(&sw, &css_name, &js_name);
    let sw_minified = js_minify_safe(&sw_updated);
    fs::write(dist_dir.join("sw.js"), &sw_minified).expect("write sw.js");

    // ── 10. Write HTML for include_str!() to OUT_DIR ─────────────────────────
    for name in HTML_INCLUDE {
        let src = dist_dir.join(name);
        if src.exists() {
            let _ = fs::copy(&src, out_dir.join(name));
        }
    }
    // index.html too (future use / embedded route)
    fs::write(out_dir.join("index.html"), &rewritten_index).expect("write out index.html");

    eprintln!("cargo:warning=OxiCloud static-dist built ✓  CSS: {css_name}  JS: {js_name}");
}

// ═══════════════════════════════════════════════════════════════════════════════
// CSS processing
// ═══════════════════════════════════════════════════════════════════════════════

/// Resolve `@import url("…")` one level deep, returning concatenated CSS.
fn resolve_css_imports(entry: &Path, css_dir: &Path) -> String {
    let content = fs::read_to_string(entry).unwrap_or_default();
    let mut out = String::with_capacity(content.len() * 20);

    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("@import") {
            if let Some(rel) = extract_import_path(t) {
                let resolved = css_dir.join(rel.trim_start_matches("./"));
                if resolved.exists() {
                    println!("cargo:warning=CSS importing: {}", resolved.display());
                    out.push_str(&fs::read_to_string(&resolved).unwrap_or_default());
                    out.push('\n');
                } else {
                    eprintln!("cargo:warning=CSS import not found: {}", resolved.display());
                }
            }
        } else if !t.is_empty() && !t.starts_with("/*") {
            // Keep non-import, non-comment lines
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Extract the path from `@import url("./foo.css");` or `@import "./foo.css";`
fn extract_import_path(line: &str) -> Option<String> {
    let s = line.find('"')? + 1;
    let e = line[s..].find('"')? + s;
    Some(line[s..e].to_string())
}

/// Minify CSS via lightningcss — returns original on failure.
fn css_minify_safe(source: &str) -> String {
    css_minify(source).unwrap_or_else(|e| {
        eprintln!("cargo:warning=CSS minify failed: {e}");
        source.to_string()
    })
}

fn css_minify(source: &str) -> Result<String, String> {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    let mut sheet =
        StyleSheet::parse(source, ParserOptions::default()).map_err(|e| format!("{e}"))?;

    sheet
        .minify(Default::default())
        .map_err(|e| format!("{e}"))?;

    let res = sheet
        .to_css(PrinterOptions {
            minify: true,
            ..Default::default()
        })
        .map_err(|e| format!("{e}"))?;

    Ok(res.code)
}

/// Walk a directory and minify every `.css` in-place (skips generated bundles).
fn minify_tree_css(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            minify_tree_css(&p);
        } else if p.extension().is_some_and(|e| e == "css") {
            let fname = p.file_name().unwrap().to_string_lossy();
            // Skip the generated bundle and already-processed main.css
            if fname.starts_with("app.") || fname == "main.css" {
                continue;
            }
            println!("cargo:warning=CSS importing: {}", p.display());
            if let Ok(src) = fs::read_to_string(&p) {
                let _ = fs::write(&p, css_minify_safe(&src));
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// JS bundling (ES module → single IIFE)
// ═══════════════════════════════════════════════════════════════════════════════

/// Collect `<link rel="stylesheet" href="/css/…">` paths from HTML as paths
/// relative to the CSS directory (e.g. `views/mySharesView.css`).
/// Skips `main.css` (resolved separately via @import chain).
fn extract_css_links(html: &str) -> Vec<String> {
    html.lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.starts_with("<link") && t.contains("stylesheet") && t.contains("href=\"/css/") {
                let s = t.find("href=\"/css/")? + 11; // skip `href="/css/`
                let e = t[s..].find('"')? + s;
                let rel = t[s..e].to_string();
                if rel == "main.css" || rel.starts_with("app.") {
                    None
                } else {
                    Some(rel)
                }
            } else {
                None
            }
        })
        .collect()
}

/// Collect `<script type="module" src="…">` paths from HTML.
fn extract_module_scripts(html: &str) -> Vec<String> {
    html.lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.starts_with("<script") && t.contains("type=\"module\"") && t.contains("src=\"") {
                let s = t.find("src=\"")? + 5;
                let e = t[s..].find('"')? + s;
                Some(t[s..e].to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Build a single IIFE from all ES-module entry points.
///
/// Algorithm:
///   1. DFS from each entry point, following `import … from '…'` edges.
///   2. Post-order traversal ensures every dependency is emitted before its importer.
///   3. Cycles are broken by marking files as visited before recursing.
///   4. **Deconflict pass**: any top-level binding that is private (not exported)
///      and shared across two or more modules is renamed `NAME_<idx>` in every
///      module that declares it.  This prevents `SyntaxError: already declared`
///      when all modules land in the same IIFE scope.
///   5. Each file has its import/export syntax stripped before being appended.
///   6. The result is wrapped in `(function(){"use strict"; …})();`.
fn build_js_module_bundle(entry_scripts: &[String], static_dir: &Path) -> String {
    use std::collections::HashSet;

    let mut order: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for script in entry_scripts {
        let path = static_dir.join(script.trim_start_matches('/'));
        collect_module_deps(&path, &mut order, &mut seen);
    }

    println!(
        "cargo:warning=bundle: {} files in dependency order:",
        order.len()
    );

    // Read all sources upfront — the deconflict pass needs the full set.
    let mut sources: Vec<String> = order
        .iter()
        .map(|f| match fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cargo:warning=bundle: cannot read {}: {e}", f.display());
                String::new()
            }
        })
        .collect();

    // Deconflict: rename private top-level bindings that collide across modules.
    deconflict_module_sources(&order, &mut sources);

    // Concatenate into a single IIFE.
    let mut bundle = String::with_capacity(2 * 1024 * 1024);
    bundle.push_str("(function(){\n\"use strict\";\n");
    let mut declared_namespaces = HashSet::new();
    for (i, (file, src)) in order.iter().zip(sources.iter()).enumerate() {
        println!(
            "cargo:warning=bundle [{:>3}/{}] {}",
            i + 1,
            order.len(),
            file.display()
        );
        bundle.push_str(&strip_esm_syntax(src, file, &mut declared_namespaces));
        bundle.push('\n');
    }
    bundle.push_str("})();\n");
    bundle
}

// ─────────────────────────────────────────────────────────────────────────────
// Deconflict pass
// ─────────────────────────────────────────────────────────────────────────────

/// Rename private top-level bindings that appear in more than one module so
/// they don't collide when all modules are concatenated into one IIFE scope.
///
/// Only **private** (non-exported) names are renamed.  Exported names are the
/// public API — other modules reference them directly by name after
/// import-stripping and must not be touched.
///
/// The renamed form is `NAME_<module_index>` where the index is the position
/// of the module in the bundle order — guaranteed unique within the bundle.
fn deconflict_module_sources(order: &[PathBuf], sources: &mut [String]) {
    use std::collections::{HashMap, HashSet};

    // Per-module: private (non-exported) top-level binding names.
    let private_bindings: Vec<Vec<String>> = sources
        .iter()
        .map(|src| {
            let all = top_level_bindings(src);
            let exported: HashSet<String> = extract_exported_names(src).into_iter().collect();
            all.into_iter().filter(|n| !exported.contains(n)).collect()
        })
        .collect();

    // Count how many modules declare each private name.
    let mut name_count: HashMap<String, usize> = HashMap::new();
    for bindings in &private_bindings {
        for name in bindings {
            *name_count.entry(name.clone()).or_insert(0) += 1;
        }
    }

    // Collision set: names declared privately in more than one module.
    let collisions: HashSet<String> = name_count
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();

    if collisions.is_empty() {
        return;
    }

    let mut sorted: Vec<&str> = collisions.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    println!(
        "cargo:warning=bundle: deconflicting {} name(s): {}",
        sorted.len(),
        sorted.join(", ")
    );

    // Rename each colliding binding within every module that declares it.
    for (idx, src) in sources.iter_mut().enumerate() {
        for name in &private_bindings[idx] {
            if collisions.contains(name) {
                let new_name = format!("{name}_{idx}");
                *src = rename_binding(src, name, &new_name);
                println!(
                    "cargo:warning=bundle:   [{idx}] {name} -> {new_name}  ({})",
                    order[idx].file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }
}

/// Return the names of all top-level bindings in an ES-module source file that
/// are **locally declared AND not exported**, using OXC for accurate analysis.
///
/// Two categories are excluded so the deconflict pass never touches them:
///
/// 1. **Import bindings** (`import { ui } from '…'`): after import-stripping,
///    these names resolve directly to the exporting module's declaration already
///    present in the IIFE scope — renaming them would break those references.
///
/// 2. **Exported bindings**: other modules import these by name after stripping,
///    so they must keep their original names.  `ParseReturn::module_record` is
///    used instead of the text-based `extract_exported_names` helper because
///    multi-line `export { … }` blocks would otherwise be missed.
fn top_level_bindings(source: &str) -> Vec<String> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::mjs()).parse();
    if !ret.errors.is_empty() {
        return Vec::new(); // parse failed — skip deconflict for this file
    }

    // Collect exported local names from the module record (handles single-line
    // and multi-line export blocks, re-exports, export-declarations, etc.).
    let exported: std::collections::HashSet<&str> = ret
        .module_record
        .local_export_entries
        .iter()
        .filter_map(|e| e.local_name.name())
        .map(|s| s.as_str())
        .collect();

    let semantic = SemanticBuilder::new().build(&ret.program).semantic;
    let scoping = semantic.scoping();
    let root = scoping.root_scope_id();
    scoping
        .get_bindings(root)
        .iter()
        .filter_map(|(ident, &symbol_id)| {
            let name = ident.as_str();
            // Skip import bindings and exported bindings.
            let flags = scoping.symbol_flags(symbol_id);
            if flags.is_import() || exported.contains(name) {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Replace every whole-word occurrence of `old` with `new` in `source`.
///
/// "Whole word" means the characters immediately before and after the match
/// are not JavaScript identifier characters (`[a-zA-Z0-9_$]`).  This prevents
/// `LOAD_MORE_ID` from being accidentally renamed when the source contains
/// `LOAD_MORE_ID_EXTRA`.
fn rename_binding(source: &str, old: &str, new: &str) -> String {
    let old_len = old.len();
    let mut out = String::with_capacity(source.len());
    let mut start = 0;

    while let Some(rel) = source[start..].find(old) {
        let pos = start + rel;
        let after = pos + old_len;

        let boundary_before = pos == 0
            || source[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !is_js_ident_char(c));

        let boundary_after = after >= source.len()
            || source[after..]
                .chars()
                .next()
                .is_none_or(|c| !is_js_ident_char(c));

        out.push_str(&source[start..pos]);
        if boundary_before && boundary_after {
            out.push_str(new);
        } else {
            out.push_str(old);
        }
        start = after;
    }
    out.push_str(&source[start..]);
    out
}

#[inline]
fn is_js_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// DFS post-order: push `file` to `order` after all its imports.
/// Marks files as seen before recursing to break circular dependencies.
fn collect_module_deps(
    file: &Path,
    order: &mut Vec<PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
) {
    let canonical = match file.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("cargo:warning=JS import not found: {}", file.display());
            return;
        }
    };
    if !seen.insert(canonical.clone()) {
        return; // already visited (or in-progress cycle)
    }

    let src = fs::read_to_string(file).unwrap_or_default();
    let base = file.parent().unwrap_or(Path::new("."));

    for rel in extract_esm_import_paths(&src) {
        if rel.starts_with('.') {
            let target = base.join(&rel);
            // Skip vendor bundles: they may use top-level await or other ESM
            // patterns that are incompatible with IIFE wrapping. They must be
            // loaded via dynamic import() at runtime instead.
            // Skip also workers path
            if !target
                .components()
                .any(|c| c.as_os_str() == "vendors" || c.as_os_str() == "workers")
            {
                collect_module_deps(&target, order, seen);
            }
        }
        // Non-relative (bare specifiers like 'react') are ignored — not used here.
    }

    order.push(file.to_path_buf());
}

/// Return all relative paths found in `import … from '…'` / `export … from '…'` lines.
fn extract_esm_import_paths(source: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut multiline = false;

    for line in source.lines() {
        let t = line.trim();

        if multiline {
            // Waiting for the `from '…'` of a multi-line import
            if let Some(p) = extract_from_clause(t) {
                paths.push(p);
                multiline = false;
            } else if t.ends_with(';') {
                multiline = false; // malformed, give up on this import
            }
            continue;
        }

        if !t.starts_with("import ") && !t.starts_with("export ") {
            continue;
        }

        if let Some(p) = extract_from_clause(t) {
            paths.push(p);
        } else if t.starts_with("import ") && !t.ends_with(';') && !t.contains("//") {
            // Multi-line: `import {\n  X,\n  Y\n} from '…'`
            multiline = true;
        }
    }
    paths
}

/// Extract the path string from the `from '…'` or `from "…"` tail of a line.
fn extract_from_clause(s: &str) -> Option<String> {
    let from = s.rfind(" from ")?;
    let rest = s[from + 6..].trim();
    let q = rest.chars().next()?;
    if q != '\'' && q != '"' {
        return None;
    }
    let end = rest[1..].find(q)? + 1;
    Some(rest[1..end].to_string())
}

/// Extract all names that a JS module source exports.
///
/// Handles:
/// - `export { X, Y };`  and  `export { X as Z };`
/// - `export function f`, `export async function f`, `export class C`
/// - `export const X`, `export let X`, `export var X`
///
/// Does NOT follow `export { X } from '...'` re-exports.
fn extract_exported_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();

    for line in source.lines() {
        let t = line.trim();

        // export { X, Y } — skip re-exports from other modules
        if (t.starts_with("export {") || t.starts_with("export{")) && !t.contains(" from ") {
            if let (Some(start), Some(end)) = (t.find('{'), t.find('}')) {
                for binding in t[start + 1..end].split(',') {
                    let b = binding.trim();
                    let exported = if let Some(pos) = b.find(" as ") {
                        b[pos + 4..].trim()
                    } else {
                        b
                    };
                    if !exported.is_empty() {
                        names.push(exported.to_string());
                    }
                }
            }
            continue;
        }

        const DECL_PREFIXES: &[&str] = &[
            "export async function ",
            "export function ",
            "export class ",
            "export const ",
            "export let ",
            "export var ",
        ];
        for prefix in DECL_PREFIXES {
            if let Some(rest) = t.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                if !name.is_empty() {
                    names.push(name);
                }
                break;
            }
        }
    }

    names
}

/// Strip ES-module syntax from a single file so it can be inlined into an IIFE.
///
/// | Input                                    | Output                                      |
/// |------------------------------------------|---------------------------------------------|
/// | `import { X } from './y.js';`            | *(empty line)*                              |
/// | `import { X as Y } from './y.js';`       | `const Y = X;`                              |
/// | `import * as ns from './y.js';`          | `const ns = { export1, export2, … };`       |
/// | `export { X, Y };`                       | *(empty line)*                              |
/// | `export { X } from './y.js';`            | *(empty line)*                              |
/// | `export const X = …`                     | `const X = …`                               |
/// | `export function f() {…}`               | `function f() {…}`                          |
/// | `export async function f() {…}`         | `async function f() {…}`                    |
/// | `export class C {…}`                    | `class C {…}`                               |
/// | `export default expr;`                   | `const _default = expr;`                    |
fn strip_esm_syntax(
    source: &str,
    file: &Path,
    declared_namespaces: &mut std::collections::HashSet<String>,
) -> String {
    let mut out = String::with_capacity(source.len());
    // True while we are inside a multi-line import/export-list that has not yet
    // seen its terminating `;`.
    let mut skipping = false;

    for line in source.lines() {
        let t = line.trim();

        if skipping {
            // Keep skipping until the statement ends
            if t.ends_with(';') || t.contains(" from ") {
                skipping = false;
            }
            out.push('\n'); // preserve line count for source maps / debugging
            continue;
        }

        // ── import * as ns from './path.js' ───────────────────────────────────
        // Build a synthetic namespace object from the module's exports so that
        // `ns.foo()` calls resolve correctly inside the IIFE scope.
        // If multiple files import the same namespace name, only the first
        // declaration is emitted — subsequent ones become empty lines to avoid
        // `SyntaxError: Identifier already declared`.
        if t.starts_with("import * as ") {
            let stmt = (|| -> Option<String> {
                // Extract the namespace identifier
                let after_as = t.strip_prefix("import * as ")?;
                let name_end = after_as.find(' ')?;
                let ns_name = &after_as[..name_end];

                // Already declared earlier in the bundle — skip re-declaration.
                if declared_namespaces.contains(ns_name) {
                    return Some(String::new());
                }

                // Extract the module path from the `from '…'` clause
                let module_path = extract_from_clause(t)?;
                if !module_path.starts_with('.') {
                    return None; // bare specifier — not bundled
                }

                // Skip vendor/worker bundles (dynamically loaded at runtime)
                let base = file.parent().unwrap_or(Path::new("."));
                let target = base.join(&module_path);
                if target
                    .components()
                    .any(|c| c.as_os_str() == "vendors" || c.as_os_str() == "workers")
                {
                    return None;
                }

                let module_src = fs::read_to_string(&target).ok()?;
                let exports = extract_exported_names(&module_src);
                if exports.is_empty() {
                    return None;
                }

                declared_namespaces.insert(ns_name.to_string());
                let indent = &line[..line.len() - line.trim_start().len()];
                Some(format!(
                    "{}const {} = {{ {} }};",
                    indent,
                    ns_name,
                    exports.join(", ")
                ))
            })();

            match stmt {
                Some(s) => out.push_str(&s),
                None => {
                    println!("cargo:warning=bundle: could not resolve namespace import: {t}");
                }
            }
            out.push('\n');
            continue;
        }

        // ── import … ──────────────────────────────────────────────────────────
        // Emit `const Y = X;` for any `import { X as Y }` aliases so that code
        // using the aliased name still resolves inside the IIFE scope.
        if t.starts_with("import ") {
            if !t.ends_with(';') && !t.contains(" from ") {
                skipping = true; // multi-line import
            }
            let aliases = collect_import_aliases(t, declared_namespaces);
            if aliases.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&aliases);
                out.push('\n');
            }
            continue;
        }

        // ── export { … } or export { … } from '…' ────────────────────────────
        if t.starts_with("export {") || t.starts_with("export{") {
            if !t.ends_with(';') {
                skipping = true;
            }
            out.push('\n');
            continue;
        }

        // ── export const/let/var/function/async function/class ─────────────────
        if let Some(stripped) = try_strip_export_prefix(line) {
            out.push_str(&stripped);
            out.push('\n');
            continue;
        }

        // ── export default expr ────────────────────────────────────────────────
        // Rare in our codebase; keep the value as a named variable.
        if let Some(rhs) = t.strip_prefix("export default ") {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(&format!("{indent}const _default = {rhs}"));
            out.push('\n');
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    out
}

/// If `line` (with leading whitespace preserved) begins with `export <decl-keyword>`,
/// return the same line with `export ` (7 chars) removed.
fn try_strip_export_prefix(line: &str) -> Option<String> {
    const PREFIXES: &[&str] = &[
        "export const ",
        "export let ",
        "export var ",
        "export function ",
        "export async function ",
        "export class ",
    ];
    let t = line.trim();
    for prefix in PREFIXES {
        if t.starts_with(prefix) {
            let indent_len = line.len() - line.trim_start().len();
            // Remove "export " (7 chars) right after the indent
            return Some(format!(
                "{}{}",
                &line[..indent_len],
                &line[indent_len + 7..]
            ));
        }
    }
    None
}

/// For `import { A, B as C, D as E } from '…'` return `"const C = B;\nconst E = D;"`.
/// Returns an empty string when there are no aliases.
/// Aliases already present in `declared` are skipped; newly emitted aliases are
/// inserted into `declared` so that subsequent files don't re-declare them.
fn collect_import_aliases(stmt: &str, declared: &mut std::collections::HashSet<String>) -> String {
    let brace_start = match stmt.find('{') {
        Some(i) => i + 1,
        None => return String::new(),
    };
    let brace_end = match stmt.find('}') {
        Some(i) => i,
        None => return String::new(),
    };
    let bindings = &stmt[brace_start..brace_end];

    let mut out = String::new();
    for binding in bindings.split(',') {
        let b = binding.trim();
        if let Some(as_pos) = b.find(" as ") {
            let orig = b[..as_pos].trim();
            let alias = b[as_pos + 4..].trim();
            // Already declared earlier in the bundle — skip to avoid
            // `SyntaxError: Identifier already declared`.
            if declared.contains(alias) {
                continue;
            }
            declared.insert(alias.to_string());
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("const {alias} = {orig};"));
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════════
// JS minification
// ═══════════════════════════════════════════════════════════════════════════════

/// Parse the JS bundle with OXC and hard-fail the build on any error.
///
/// Called on the **raw** (pre-minification) bundle so error messages still
/// reference readable source.  Uses `cargo:error=` so Cargo surfaces the
/// problem immediately and stops the build — no silent fallback.
fn js_bundle_validate(source: &str) {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    // The bundle is a classic IIFE script, not an ES module.
    let ret = Parser::new(&allocator, source, SourceType::cjs()).parse();
    if !ret.errors.is_empty() {
        for e in &ret.errors {
            println!("cargo:error=JS bundle parse error: {e}");
        }
        std::process::exit(1);
    }
}

/// Minify an ES-module file (contains import/export) — returns original on failure.
fn js_minify_safe(source: &str) -> String {
    js_minify_inner(source, true)
}

/// Minify a classic script / IIFE bundle (no import/export) — returns original on failure.
fn js_minify_script_safe(source: &str) -> String {
    js_minify_inner(source, false)
}

fn js_minify_inner(source: &str, is_module: bool) -> String {
    if source.trim().is_empty() {
        return String::new();
    }
    js_minify(source, is_module).unwrap_or_else(|e| {
        eprintln!("cargo:warning=JS minify failed: {e}");
        source.to_string()
    })
}

fn js_minify(source: &str, is_module: bool) -> Result<String, String> {
    use oxc_allocator::Allocator;
    use oxc_codegen::{Codegen, CodegenOptions, CommentOptions};
    use oxc_minifier::{CompressOptions, CompressOptionsUnused, Minifier, MinifierOptions};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let source_type = if is_module {
        SourceType::mjs()
    } else {
        SourceType::cjs()
    };
    let ret = Parser::new(&allocator, source, source_type).parse();

    if !ret.errors.is_empty() {
        let msgs: Vec<_> = ret.errors.iter().take(3).map(|e| format!("{e}")).collect();
        return Err(format!("parse errors: {}", msgs.join("; ")));
    }

    let mut program = ret.program;

    Minifier::new(MinifierOptions {
        mangle: None,
        compress: Some(CompressOptions {
            unused: CompressOptionsUnused::Keep,
            ..CompressOptions::default()
        }),
    })
    .minify(&allocator, &mut program);

    let output = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            comments: CommentOptions {
                normal: false,
                jsdoc: false,
                ..CommentOptions::default()
            },
            ..Default::default()
        })
        .build(&program);

    Ok(output.code)
}

/// Walk a directory and minify every `.js` in-place (skips generated `app.*` bundles).
fn minify_tree_js(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            minify_tree_js(&p);
        } else if p.extension().is_some_and(|e| e == "js") {
            let fname = p.file_name().unwrap().to_string_lossy();
            if fname.starts_with("app.") {
                continue;
            }
            if let Ok(src) = fs::read_to_string(&p) {
                let _ = fs::write(&p, js_minify_safe(&src));
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// JSON minification (no external deps)
// ═══════════════════════════════════════════════════════════════════════════════

fn minify_tree_json(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "json")
            && let Ok(src) = fs::read_to_string(&p)
        {
            let _ = fs::write(&p, json_minify(&src));
        }
    }
}

/// Strip insignificant whitespace outside JSON strings.
fn json_minify(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_string = false;
    let mut escape = false;
    for ch in source.chars() {
        if escape {
            out.push(ch);
            escape = false;
            continue;
        }
        if in_string {
            out.push(ch);
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => {
                    in_string = true;
                    out.push(ch);
                }
                ' ' | '\n' | '\r' | '\t' => {} // drop whitespace
                _ => out.push(ch),
            }
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════════
// HTML rewriting
// ═══════════════════════════════════════════════════════════════════════════════

/// Rewrite index.html for release:
///   - Collapse all `<link stylesheet href="/css/…">` into the single CSS bundle.
///   - Replace all `<script type="module" src="…">` with the single JS bundle.
///   - Leave `theme-init.js` and `sw-register.js` as external src references.
fn rewrite_index_html(html: &str, css_path: &str, js_path: &str, theme_init_js: &str) -> String {
    let mut out: Vec<String> = Vec::with_capacity(html.lines().count() + 3);
    let mut css_done = false;
    let mut js_done = false;

    for line in html.lines() {
        let t = line.trim();

        // ── Early resource hints, injected right after <meta charset> ────────
        // The render-blocking classic theme-init <script> below would otherwise
        // delay discovery of the parser-blocked stylesheet/module bundles.
        // Preloading them near the top of <head> lets the preload scanner fetch
        // both critical bundles in parallel with (not behind) theme-init.
        // Placed after <meta charset> so charset stays the first element.
        // Costs two tags; saves a serialized RTT.
        if t.starts_with("<meta charset") {
            out.push(line.to_string());
            out.push(
                "    <!-- Early resource hints (build.rs): fetch critical bundles up front -->"
                    .to_string(),
            );
            out.push(format!(
                "    <link rel=\"preload\" href=\"{css_path}\" as=\"style\">"
            ));
            out.push(format!(
                "    <link rel=\"modulepreload\" href=\"{js_path}\">"
            ));
            continue;
        }

        // ── Inline the tiny render-blocking theme-init script ────────────────
        // It must run before paint (avoids a theme flash) and is only ~400B, so
        // inlining drops one request off the critical path. Falls back to the
        // external <script src> if the source couldn't be read/minified.
        if t.starts_with("<script") && t.contains("theme-init.js") {
            if theme_init_js.trim().is_empty() {
                out.push(line.to_string());
            } else {
                out.push(format!("    <script>{}</script>", theme_init_js.trim()));
            }
            continue;
        }

        // ── Replace all stylesheet <link>s with single bundle ────────────────
        if t.starts_with("<link") && t.contains("stylesheet") && t.contains("href=\"/css/") {
            if !css_done {
                out.push(format!("    <link rel=\"stylesheet\" href=\"{css_path}\">"));
                css_done = true;
            }
            continue;
        }

        // ── Replace all type="module" scripts with single bundle ─────────────
        if t.starts_with("<script") && t.contains("type=\"module\"") && t.contains("src=\"") {
            if !js_done {
                out.push(format!(
                    "    <script defer type=\"module\" src=\"{js_path}\"></script>"
                ));
                js_done = true;
            }
            continue;
        }

        // ── Drop "Styles" / "Scripts" section comments ───────────────────────
        if t.starts_with("<!--") && (t.contains("Styles") || t.contains("Scripts")) {
            continue;
        }

        out.push(line.to_string());
    }

    out.join("\n")
}

// ═══════════════════════════════════════════════════════════════════════════════
// Service Worker cache-list update
// ═══════════════════════════════════════════════════════════════════════════════

fn update_sw_cache(sw: &str, css_bundle: &str, js_bundle: &str) -> String {
    let marker_start = "const ASSETS_TO_CACHE = [";
    let marker_end = "];";

    let Some(start) = sw.find(marker_start) else {
        return sw.to_string();
    };
    let Some(end_off) = sw[start..].find(marker_end) else {
        return sw.to_string();
    };

    let before = &sw[..start];
    let after = &sw[start + end_off + marker_end.len()..];

    format!(
        "{before}const ASSETS_TO_CACHE = [\n\
         \x20 '/css/{css_bundle}',\n\
         \x20 '/js/{js_bundle}',\n\
         \x20 '/locales/en.json',\n\
         \x20 '/locales/es.json',\n\
         \x20 '/favicon.ico'\n\
         ]{after}"
    )
}

// ═══════════════════════════════════════════════════════════════════════════════
// Utilities
// ═══════════════════════════════════════════════════════════════════════════════

fn penv(key: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| panic!("{key} not set")))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// FNV-1a hash → 8 hex chars.  Fast, non-crypto, perfect for cache-busting.
fn fnv_hash(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
