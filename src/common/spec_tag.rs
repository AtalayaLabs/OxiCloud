//! Release-tag guard for the committed spec generators.
//!
//! `resources/gen/openapi.json` and `resources/gen/asyncapi.json` are
//! committed artefacts, drift-checked by CI on every PR, and both stamp
//! `env!("OXICLOUD_TAG")` into their `info.version`. That value is only
//! byte-stable across checkouts while a git tag is actually reachable
//! from HEAD: `build.rs` derives it from
//! `git describe --tags --always --dirty=-dirty`, and `--always` makes
//! the no-tag case fall back to the bare commit SHA rather than fail.
//! `strip_describe_suffix()` finds no `-<n>-g<hex>` tail on a bare SHA,
//! so it passes straight through into the spec.
//!
//! The result is the one failure mode this value exists to prevent: a
//! contributor whose clone has no tags (a fork created before the last
//! release — GitHub's "Sync fork" syncs branches, not tags — or a
//! `--depth 1` / `--no-tags` clone) silently generates a spec versioned
//! with their own commit hash. It looks fine locally and fails the drift
//! job for everyone. That is exactly what landed in #701, where
//! `"version": "d97bedda"` reached the committed `openapi.json`.
//!
//! So the generators call [`require_release_tag_or_exit`] before writing
//! anything, turning a silently poisoned artefact into a loud failure at
//! generation time. Deliberately no auto-fetch: the diagnostic prints the
//! command and the operator runs it, same posture as the other repair
//! paths in this repo.

/// Abort the calling generator unless `tag` names a real release.
///
/// On success this returns and the caller writes its spec as usual. On
/// failure it prints an actionable diagnostic to stderr and **exits the
/// process with status 1** — nothing is written. The exit lives here, not
/// in each `main`, so adding a third generator costs one line rather than
/// a copy of the error text.
pub fn require_release_tag_or_exit(generator: &str, tag: &str) {
    if is_release_tag(tag) {
        return;
    }

    eprintln!(
        "\n\
         {generator}: OXICLOUD_TAG is {tag:?}, which is not a release tag.\n\
         \n\
         No git tag is reachable from HEAD, so build.rs fell back to the bare\n\
         commit SHA. This spec is a committed artefact and CI regenerates it\n\
         WITH tags fetched — writing it now would stamp your commit hash into\n\
         info.version and fail the drift job for everyone.\n\
         \n\
         Fix one of:\n\
         \n\
         \x20 git fetch --tags <upstream> && touch build.rs && cargo run --bin {generator}\n\
         \x20 OXICLOUD_VERSION=<x.y.z> cargo run --bin {generator}\n\
         \n\
         Nothing was written.\n"
    );
    std::process::exit(1);
}

/// `true` when `tag` opens with an `X.Y.Z` numeric triple.
///
/// Accepts every legitimate shape `build.rs` can hand us: a clean tag
/// (`0.9.2`), and the describe/dirty forms that `strip_describe_suffix()`
/// has already reduced to one (`0.9.2-3-g4f12bd25` and `0.9.2-dirty` both
/// arrive as `0.9.2`). A prerelease tag (`1.0.0-rc1`) passes too — only
/// the leading digits of the patch segment are examined.
///
/// Rejects the two no-tag outcomes: a bare commit SHA (`d97bedda`) and
/// the source-tarball fallback family (`0.0.0-unknown`, `unknown`,
/// `unknown-dirty`) — the latter is version-shaped but names no tag, so
/// it needs an explicit check rather than falling out of the digit test.
fn is_release_tag(tag: &str) -> bool {
    if tag.contains("unknown") {
        return false;
    }

    let mut parts = tag.splitn(3, '.');
    let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };

    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let patch_digits = patch
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();

    all_digits(major) && all_digits(minor) && all_digits(patch_digits)
}

#[cfg(test)]
mod tests {
    use super::is_release_tag;

    #[test]
    fn accepts_tagged_builds() {
        // What build.rs emits once a tag is reachable — a clean tag, and
        // the describe/dirty forms after strip_describe_suffix().
        assert!(is_release_tag("0.9.2"));
        assert!(is_release_tag("0.8.9"));
        assert!(is_release_tag("10.20.30"));
        assert!(is_release_tag("1.0.0-rc1"));
        assert!(is_release_tag("1.0.0-beta.2"));
    }

    #[test]
    fn rejects_bare_commit_sha() {
        // The #701 case: `git describe --tags --always` with no tags.
        assert!(!is_release_tag("d97bedda"));
        assert!(!is_release_tag("5592a8ab"));
        // A SHA that happens to be all digits still has no dot triple.
        assert!(!is_release_tag("12345678"));
    }

    #[test]
    fn rejects_unknown_fallbacks() {
        // Version-shaped but tag-less — the source-tarball path.
        assert!(!is_release_tag("0.0.0-unknown"));
        assert!(!is_release_tag("unknown"));
        assert!(!is_release_tag("unknown-dirty"));
    }

    #[test]
    fn rejects_malformed() {
        assert!(!is_release_tag(""));
        assert!(!is_release_tag("0.9"));
        assert!(!is_release_tag("0.9."));
        assert!(!is_release_tag("v0.9.2"), "build.rs strips the leading `v`");
        assert!(!is_release_tag("0.x.2"));
    }
}
