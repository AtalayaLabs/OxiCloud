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

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ─── HTML files embedded via include_str!() in Rust source ───────────────────
const HTML_INCLUDE: &[&str] = &[
    "login.html",
    "profile.html",
    "admin.html",
    "device-verify.html",
    "nextcloud-login.html",
    "share.html",
];

// ─── View CSS files linked directly in index.html (not via @import) ──────────
const INDEX_VIEW_CSS: &[&str] = &[
    "views/inlineViewer.css",
    "views/favorites.css",
    "views/recent.css",
    "views/shared.css",
    "views/trash.css",
    "views/photos.css",
    "views/photosLightbox.css",
    "views/music.css",
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

    // ── 2. Resolve main.css @imports ─────────────────────────────────────────
    let resolved_main = resolve_css_imports(&css_dir.join("main.css"), &css_dir);
    let minified_main = css_minify_safe(&resolved_main);
    fs::write(dist_dir.join("css/main.css"), &minified_main).expect("write main.css");

    // ── 3. Build CSS bundle for index.html ───────────────────────────────────
    let mut css_all = resolved_main;
    for view in INDEX_VIEW_CSS {
        let p = css_dir.join(view);
        if p.exists() {
            css_all.push_str(&fs::read_to_string(&p).unwrap_or_default());
            css_all.push('\n');
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
    let index_html = fs::read_to_string(static_dir.join("index.html")).expect("read index.html");
    let module_scripts = extract_module_scripts(&index_html);
    let js_raw = build_js_module_bundle(&module_scripts, static_dir);
    let js_bundle = js_minify_script_safe(&js_raw);
    let js_hash = fnv_hash(js_bundle.as_bytes());
    let js_name = format!("app.{js_hash}.js");
    fs::create_dir_all(dist_dir.join("js")).expect("js dir");
    fs::write(dist_dir.join("js").join(&js_name), &js_bundle).expect("write js bundle");

    // ── 6. Minify ALL individual JS files in static-dist/ ────────────────────
    minify_tree_js(&dist_dir.join("js"));

    // ── 7. Rewrite index.html ────────────────────────────────────────────────
    let rewritten_index = rewrite_index_html(
        &index_html,
        &format!("/css/{css_name}"),
        &format!("/js/{js_name}"),
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
            if let Ok(src) = fs::read_to_string(&p) {
                let _ = fs::write(&p, css_minify_safe(&src));
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// JS bundling (ES module → single IIFE)
// ═══════════════════════════════════════════════════════════════════════════════

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
///   4. Each file has its import/export syntax stripped before being appended.
///   5. The result is wrapped in `(function(){"use strict"; …})();`.
fn build_js_module_bundle(entry_scripts: &[String], static_dir: &Path) -> String {
    use std::collections::HashSet;

    let mut order: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for script in entry_scripts {
        let path = static_dir.join(script.trim_start_matches('/'));
        collect_module_deps(&path, &mut order, &mut seen);
    }

    let mut bundle = String::with_capacity(2 * 1024 * 1024);
    bundle.push_str("(function(){\n\"use strict\";\n");
    for file in &order {
        match fs::read_to_string(file) {
            Ok(src) => {
                bundle.push_str(&strip_esm_syntax(&src));
                bundle.push('\n');
            }
            Err(e) => eprintln!("cargo:warning=bundle: cannot read {}: {e}", file.display()),
        }
    }
    bundle.push_str("})();\n");
    bundle
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
            if !target.components().any(|c| c.as_os_str() == "vendors") {
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

/// Strip ES-module syntax from a single file so it can be inlined into an IIFE.
///
/// | Input                                    | Output                       |
/// |------------------------------------------|------------------------------|
/// | `import { X } from './y.js';`            | *(empty line)*               |
/// | `import { X as Y } from './y.js';`       | `const Y = X;`               |
/// | `export { X, Y };`                       | *(empty line)*               |
/// | `export { X } from './y.js';`            | *(empty line)*               |
/// | `export const X = …`                     | `const X = …`                |
/// | `export function f() {…}`               | `function f() {…}`           |
/// | `export async function f() {…}`         | `async function f() {…}`     |
/// | `export class C {…}`                    | `class C {…}`                |
/// | `export default expr;`                   | `const _default = expr;`     |
fn strip_esm_syntax(source: &str) -> String {
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

        // ── import … ──────────────────────────────────────────────────────────
        // Emit `const Y = X;` for any `import { X as Y }` aliases so that code
        // using the aliased name still resolves inside the IIFE scope.
        if t.starts_with("import ") {
            if !t.ends_with(';') && !t.contains(" from ") {
                skipping = true; // multi-line import
            }
            let aliases = collect_import_aliases(t);
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
fn collect_import_aliases(stmt: &str) -> String {
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
fn rewrite_index_html(html: &str, css_path: &str, js_path: &str) -> String {
    let mut out: Vec<String> = Vec::with_capacity(html.lines().count());
    let mut css_done = false;
    let mut js_done = false;

    for line in html.lines() {
        let t = line.trim();

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
