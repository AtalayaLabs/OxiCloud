//! build.rs — injects git build metadata into the binary.
//!
//! Exposes `GIT_HASH` and `GIT_BRANCH` (consumed via `env!()` in `main.rs`).
//! There is no Rust-side asset pipeline: the frontend is built by Vite into
//! `static-dist/` and served directly by the web layer (`interfaces::web`).

use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    git_status();
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

    // Only nag on CI builds — the warning fires every time build.rs
    // runs (branch switch, commit on current branch, first build).
    // On a local dev loop it becomes noise. CI is where "which commit
    // built this artifact" is load-bearing (release provenance,
    // release-note automation).
    if env::var("CI").is_ok() {
        println!("cargo:warning=OxiCloud built with git hash: {git_hash} and branch: {git_branch}");
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
