//! build.rs — injects git build metadata into the binary and, under the
//! optional `bundled-assets` feature, guards the compile-time embed
//! precondition.
//!
//! Exposes `GIT_HASH` and `GIT_BRANCH` (consumed via `env!()` in `main.rs`).
//! The frontend is built by Vite into `static-dist/` at the repo root and
//! served directly by the web layer (`interfaces::web`); when
//! `bundled-assets` is on, `src/interfaces/web/embedded.rs` bakes that
//! directory into the binary at compile time via `rust-embed`.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // `sqlx::migrate!()` (src/infrastructure/db.rs) embeds every file under
    // `migrations/` at COMPILE time. Nothing else tells cargo those files
    // are inputs, so editing one and rebuilding silently kept the old
    // embedded copy — the binary and the schema disagreeing, with no error
    // until something downstream hit the difference. The only workaround
    // was touching db.rs, which you have to remember every time.
    //
    // A directory here means "watch everything beneath it", so adding,
    // editing or renaming a .sql re-triggers the embed on its own. Same
    // mechanism `bundled_assets_guard` uses for `static-dist`.
    println!("cargo:rerun-if-changed=migrations");
    git_status();
    bundled_assets_guard();
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bundled-assets precondition guard
//
// When `--features bundled-assets` is on, `rust-embed`'s `#[folder = "static-dist/"]`
// scans that directory at compile time and errors with a not-very-helpful
// "No such file or directory" if it's missing. Users hit this first when they
// try `cargo build --release --features bundled-assets` before running the
// frontend build — we intercept it here with a clear, actionable message.
//
// Also emits `cargo:rerun-if-changed=static-dist/` so a fresh frontend build
// re-triggers the embed step without needing `cargo clean` — matches what a
// dev on the bundled feature would expect after `just fe-build`.
// ═══════════════════════════════════════════════════════════════════════════════
fn bundled_assets_guard() {
    if env::var("CARGO_FEATURE_BUNDLED_ASSETS").is_err() {
        return;
    }
    println!("cargo:rerun-if-changed=static-dist");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest_dir).join("static-dist");
    let index = dist.join("index.html");
    if !index.exists() {
        // `cargo:warning=` prefixes surface these in the terminal even
        // when cargo's default output is quiet; the panic below turns
        // them into a compile-time error so the missing prerequisite
        // can't slip past a distracted dev.
        println!("cargo:warning=`bundled-assets` feature requires static-dist/ at the repo root.");
        println!("cargo:warning=Build the SvelteKit SPA first:  (cd frontend && npm run build)");
        println!("cargo:warning=Or via the workspace shortcut:   just fe-build");
        panic!(
            "build.rs: missing {}/index.html — see the cargo:warning lines above",
            dist.display()
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Grab git values
// Supports GitHub; CI vars are honoured (extend if moving to GitLab/CircleCI/…).
// ═══════════════════════════════════════════════════════════════════════════════
fn git_status() {
    // Resolve the actual git directory. Two layouts to handle:
    //
    //   * Normal checkout — `.git` is a directory; `git_dir` = ".git",
    //     and everything (HEAD, refs, packed-refs) lives inside it.
    //
    //   * `git worktree add` checkout — `.git` is a FILE with contents
    //     `gitdir: /path/to/main/.git/worktrees/<name>`. The per-worktree
    //     HEAD lives at that resolved path; branch refs + packed-refs are
    //     SHARED across worktrees and live in the main repo's `.git/`
    //     (the "common dir"). `.git/HEAD` inside the worktree checkout
    //     literally does not exist.
    //
    // Cargo's documented behaviour for `rerun-if-changed=<path>` when
    // `<path>` doesn't exist: **re-run the build script on every
    // incremental build**. On a worktree the naive `.git/HEAD` watch
    // therefore forces build.rs to run every `cargo build`, re-emits
    // GIT_HASH, invalidates main.rs, and triggers a full re-link. That
    // was the "cargo build always takes 30-60 s even with no changes"
    // symptom on worktrees.
    //
    // `resolve_git_dir` handles both shapes and gives us the ACTUAL
    // paths we should watch. `git_common_dir` (for shared refs) is
    // distinct from `git_dir` (per-worktree HEAD) in the worktree
    // case, identical in the normal-checkout case.
    let git_dir = resolve_git_dir(".git").unwrap_or_else(|| ".git".to_string());
    let git_common_dir = resolve_git_common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());

    // Watch ONLY the files whose contents encode "which commit are we
    // on" — HEAD (branch pointer OR raw SHA when detached) plus the
    // specific ref file for the current branch. Watching a directory
    // (`refs/heads`) misfires on every ref added/removed via `git
    // fetch`, `git gc`, `git branch`, and IDE git integrations —
    // bumping the dir mtime, re-running build.rs, re-emitting
    // GIT_HASH, and forcing a full re-link.
    let head_path = format!("{git_dir}/HEAD");
    if std::path::Path::new(&head_path).exists() {
        println!("cargo:rerun-if-changed={head_path}");
    }
    if let Some(current_branch_ref) = current_branch_ref_path(&git_dir) {
        // Branch refs live in `git_common_dir` (shared across
        // worktrees), not per-worktree `git_dir`.
        let ref_path = format!("{git_common_dir}/{current_branch_ref}");
        if std::path::Path::new(&ref_path).exists() {
            println!("cargo:rerun-if-changed={ref_path}");
        }
    }
    // Packed refs — git occasionally packs loose refs (auto-gc, or
    // `git pack-refs`), moving current-branch content OUT of
    // `refs/heads/<name>` and INTO `packed-refs`. Without watching
    // this file, a `commit` after a pack would go undetected until
    // the branch was re-checked-out.
    let packed_refs = format!("{git_common_dir}/packed-refs");
    if std::path::Path::new(&packed_refs).exists() {
        println!("cargo:rerun-if-changed={packed_refs}");
    }

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

    // OXICLOUD_VERSION — the string that identifies "which build is this?"
    // for humans and machines alike. Four sources, in priority order:
    //
    //   1. `OXICLOUD_VERSION` env at build time — the escape hatch for
    //      Docker builds (host has git + CI env; container doesn't).
    //      Workflow / Dockerfile derives the version once and hands it
    //      in as an ARG.
    //   2. `GITHUB_REF_NAME` when `GITHUB_REF_TYPE == "tag"` — CI tag
    //      build. Uses the ref verbatim so a shipped release binary
    //      reports the clean tag (e.g. `v0.9.2`) without a
    //      describe-suffix.
    //   3. `git describe --tags --always --dirty=-dirty` — every other
    //      build. Yields `v0.9.2` on a clean tag, `v0.9.2-3-g4f12bd25`
    //      on the 3rd commit past that tag, plus `-dirty` when the
    //      working tree has uncommitted changes.
    //   4. `Cargo.toml` version + `-unknown` — last-resort fallback for
    //      a source-tarball build (no `.git`, no CI env, no ARG). The
    //      `-unknown` suffix is the honest signal: this binary can't
    //      identify itself.
    //
    // `Cargo.toml` is pinned at `version = "0.0.0"` so nothing implicitly
    // depends on it. The clean version string flows through every runtime
    // display via `env!("OXICLOUD_VERSION")`.
    let version = env::var("OXICLOUD_VERSION")
        .ok()
        .filter(|v| !v.is_empty() && v != "unknown")
        .or_else(|| {
            matches!(env::var("GITHUB_REF_TYPE").as_deref(), Ok("tag"))
                .then(|| env::var("GITHUB_REF_NAME").ok())
                .flatten()
        })
        .or_else(|| git(&["describe", "--tags", "--always", "--dirty=-dirty"]))
        .unwrap_or_else(|| format!("{}-unknown", env!("CARGO_PKG_VERSION")));

    // Strip the leading `v` so downstream displays read like
    // `curl --version` (`OxiCloud 0.9.2` not `OxiCloud v0.9.2`).
    // The presence/absence of the `v` in git tags is honoured by
    // stripping it here — the tag format stays owner-of-repo's choice.
    let clean_version = version.strip_prefix('v').unwrap_or(&version);
    println!("cargo:rustc-env=OXICLOUD_VERSION={clean_version}");
    println!("cargo:rerun-if-env-changed=OXICLOUD_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_TYPE");

    // OXICLOUD_TAG — just the tag portion, no `-N-gHASH(-dirty)` suffix.
    //
    // Consumed by the OpenAPI / AsyncAPI `info.version` fields, both of
    // which are regenerated into committed JSON under `resources/gen/`
    // and checked by a drift CI job on every PR. If those fields
    // carried the full OXICLOUD_VERSION, every developer's local
    // `git describe` output (`0.9.2-3-g4f12bd25-dirty`) would differ
    // from CI's — the drift check would false-positive on every PR.
    //
    // Stripping to the bare tag makes the value byte-stable across
    // every checkout of the same tagged commit. It only changes when a
    // new tag actually ships — which is exactly when the spec version
    // *should* change.
    //
    // Match pattern: `-<digits>-g<hex>` optionally followed by `-dirty`,
    // anchored at end-of-string. Applied to the already-`v`-stripped
    // form so `0.9.2-3-g4f12bd25-dirty` → `0.9.2` and a clean `0.9.2`
    // stays as-is. `unknown`, `unknown-dirty` and the source-tarball
    // fallback (`0.0.0-unknown`) survive untouched — they're already
    // stable strings.
    let tag = strip_describe_suffix(clean_version);
    println!("cargo:rustc-env=OXICLOUD_TAG={tag}");

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

    // Emit unless every source came up empty. Earlier this line was
    // gated on `CI=true` to spare local dev the noise, but that also
    // silenced Docker builder stages (the container doesn't inherit
    // the runner's `CI` env) — exactly the case where "which version
    // did we bake in?" is load-bearing.
    //
    // The `all-unknown` guard suppresses one specific noise source:
    // the Docker *cacher* stage (Dockerfile stage 2) that compiles a
    // dummy `main.rs` just to warm the dep cache, without any of the
    // GitHub-Actions envs piped through. Its output is `rm -rf`d
    // before the real builder stage runs — the warning it would emit
    // ("v0.0.0-unknown …") only misleads log readers into thinking
    // the real build lost its version stamp.
    //
    // Version first because that's the answer a reader is usually
    // after; tag + hash + branch stay for provenance.
    let all_unknown =
        clean_version.ends_with("-unknown") && git_hash == "unknown" && git_branch == "unknown";
    if !all_unknown {
        println!(
            "cargo:warning=OxiCloud v{clean_version} (tag={tag} hash={git_hash} branch={git_branch})"
        );
    }
}

/// Resolve `.git` (or whatever path was passed) to the ACTUAL git
/// directory. Handles both:
///
///   * Normal checkout — `.git` is a directory → return it verbatim.
///   * Worktree — `.git` is a text file containing
///     `gitdir: /absolute/or/relative/path` → follow the pointer.
///
/// Returns `None` when the path is neither (unusual — permission issue
/// or repository-less build); caller falls back to the literal `.git`
/// name (which won't exist, so no watches fire — fine for CI where
/// build.rs runs once anyway).
fn resolve_git_dir(path: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_dir() {
        return Some(path.to_string());
    }
    // `.git` is a file — parse the `gitdir:` pointer written by
    // `git worktree add`. Format is stable across git versions:
    //   gitdir: /abs/path/to/main/.git/worktrees/<name>\n
    let contents = std::fs::read_to_string(path).ok()?;
    let target = contents.trim().strip_prefix("gitdir: ")?.trim();
    // Path may be absolute or (rarely) relative to the checkout root.
    // std::path handles both transparently for our purposes.
    Some(target.to_string())
}

/// Resolve the "git common dir" — the shared refs store. In a normal
/// checkout it equals `git_dir`. In a worktree, `git_dir` is
/// `main/.git/worktrees/<name>` and the common dir is `main/.git` —
/// where all branch refs and packed-refs actually live.
///
/// The pointer is a `commondir` file inside the worktree's git_dir
/// containing a path (usually relative, `../..` to escape the
/// `worktrees/<name>/` prefix).
fn resolve_git_common_dir(git_dir: &str) -> Option<String> {
    let commondir_marker = format!("{git_dir}/commondir");
    let contents = std::fs::read_to_string(&commondir_marker).ok()?;
    let target = contents.trim();
    // `commondir` is usually relative to git_dir; resolve it.
    let joined = std::path::Path::new(git_dir).join(target);
    // Canonicalise so downstream string-comparisons don't trip on
    // `../..` versus the real path.
    joined
        .canonicalize()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .or_else(|| joined.to_str().map(str::to_owned))
}

/// Parse `<git_dir>/HEAD` to find the specific ref file the current
/// branch points at (e.g. contents `ref: refs/heads/feat/foo` →
/// return `Some("refs/heads/feat/foo")`). Returns `None` for
/// detached HEAD (raw SHA in HEAD, no branch file to watch) or an
/// unreadable HEAD; in either case the bare HEAD watch above still
/// catches the state we care about.
fn current_branch_ref_path(git_dir: &str) -> Option<String> {
    let head = std::fs::read_to_string(format!("{git_dir}/HEAD")).ok()?;
    head.trim().strip_prefix("ref: ").map(str::to_owned)
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

/// Strip `git describe`'s "commits past tag" suffix so only the tag
/// name remains. Input shapes handled:
///
///   - `0.9.2-3-g4f12bd25-dirty` → `0.9.2`
///   - `0.9.2-3-g4f12bd25`       → `0.9.2`
///   - `0.9.2-dirty`             → `0.9.2`  (clean tag with dirty tree)
///   - `0.9.2`                   → `0.9.2`  (already a bare tag)
///   - `4f12bd25`                → `4f12bd25` (repo with no tags)
///   - `0.0.0-unknown`           → `0.0.0-unknown` (source-tarball fallback)
///
/// The match is anchored to end-of-string and requires the specific
/// `-<digits>-g<hex>` shape describe emits — so we do not accidentally
/// strip a real prerelease segment like `-rc.1`. `-dirty` is stripped
/// separately (peeled first) because a clean-tagged dirty tree yields
/// `<tag>-dirty` with no ahead-count in between.
fn strip_describe_suffix(v: &str) -> String {
    // Peel `-dirty` first — reduces the remaining problem to two
    // clean cases: a bare tag, or `<tag>-<ahead>-g<hex>`.
    let peeled = v.strip_suffix("-dirty").unwrap_or(v);

    // Look for the last `-g<hex>` segment (git describe abbreviates
    // to 7+ hex chars; we don't hard-code 7 in case someone tuned
    // core.abbrev). It has to sit right after `-<ahead-count>`.
    let Some(g_at) = peeled.rfind("-g") else {
        return peeled.to_string();
    };
    // Right of `-g` must be all hex.
    let hex = &peeled[g_at + 2..];
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return peeled.to_string();
    }
    // Left of `-g` should end with `-<digits>` (the "N commits past
    // tag" segment). If it doesn't, this `-g` wasn't a describe suffix.
    let left = &peeled[..g_at];
    let Some(dash_at) = left.rfind('-') else {
        return peeled.to_string();
    };
    let ahead = &left[dash_at + 1..];
    if ahead.is_empty() || !ahead.bytes().all(|b| b.is_ascii_digit()) {
        return peeled.to_string();
    }
    left[..dash_at].to_string()
}

#[cfg(test)]
mod tests {
    use super::strip_describe_suffix;

    #[test]
    fn strips_full_describe_suffix() {
        assert_eq!(strip_describe_suffix("0.9.2-3-g4f12bd25-dirty"), "0.9.2");
        assert_eq!(strip_describe_suffix("0.9.2-3-g4f12bd25"), "0.9.2");
    }

    #[test]
    fn strips_dirty_from_clean_tag() {
        assert_eq!(strip_describe_suffix("0.9.2-dirty"), "0.9.2");
    }

    #[test]
    fn preserves_bare_tag_and_prerelease() {
        assert_eq!(strip_describe_suffix("0.9.2"), "0.9.2");
        assert_eq!(strip_describe_suffix("1.0.0-rc.1"), "1.0.0-rc.1");
        assert_eq!(strip_describe_suffix("1.0.0-beta.2"), "1.0.0-beta.2");
    }

    #[test]
    fn preserves_no_tag_and_fallback() {
        assert_eq!(strip_describe_suffix("4f12bd25"), "4f12bd25");
        assert_eq!(strip_describe_suffix("0.0.0-unknown"), "0.0.0-unknown");
        assert_eq!(strip_describe_suffix("unknown"), "unknown");
    }
}
