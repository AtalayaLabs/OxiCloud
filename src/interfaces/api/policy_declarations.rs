//! What each folded layer declares — the TRANSCRIPTION, and the risky
//! part of PR 1 step 4.
//!
//! Still inert: nothing enforces this table. It exists so the compiled
//! result can be REVIEWED and snapshotted before any layer moves, which
//! is the only order in which the fold can be checked at all — the
//! acceptance criterion ("the table identical before and after") compares
//! this table against itself, so it proves the fold matches the table and
//! never that the table matches the layers.
//!
//! Every entry cites the layer it transcribes. A reviewer should be able
//! to check each line against that layer without reading the git log,
//! because a mis-transcription here is a silent widening that the dump
//! diff cannot see.
//!
//! The behavioural half is `tests/api/gate_matrix.hurl`, green before any
//! of this moves.

use crate::application::ports::scope_ports::{Condition, MinRole, Op};
use crate::application::services::scope_table::{PolicyRegistry, Site};

/// DAV methods the single `any` handler dispatches
/// (`webdav_handler::handle_webdav_methods`). These ARE the operation
/// identities for all three DAV surfaces — there is no route to key on,
/// which is why the AWARENESS frame kind and the CardDAV door to
/// `create_address_book` were both ungated.
///
/// One declaration per method covers `/webdav`, `/caldav` and `/carddav`
/// together, which is correct because all three carry the SAME condition
/// today: `require_internal_user_layer`, applied to each nest in
/// `main.rs`. If they ever diverge, the `Op` has to carry the surface.
pub const DAV_METHODS: [&str; 12] = [
    "COPY",
    "DELETE",
    "GET",
    "HEAD",
    "LOCK",
    "MKCOL",
    "MOVE",
    "OPTIONS",
    "PROPFIND",
    "PROPPATCH",
    "PUT",
    "UNLOCK",
];

/// The seven routes in `ANONYMOUS_ALLOWLIST`
/// (`middleware/anonymous_allowlist.rs`), copied verbatim.
///
/// **The allowlist CONTENTS are the policy**, not the subtree it is
/// attached to. Declaring `visitor_allowed()` on `/api/files` or
/// `/api/folders` instead would widen every other route beneath them —
/// which is why `gate_matrix.hurl` pins an allowlisted route against a
/// non-allowlisted sibling.
pub const ANONYMOUS_ALLOWLISTED_ROUTES: [&str; 7] = [
    "/api/folders/{id}",
    "/api/folders/{id}/resources",
    "/api/folders/{id}/ancestors",
    "/api/folders/{id}/download",
    "/api/files/{id}",
    "/api/files/{id}/thumbnail/{size}",
    "/api/files/{id}/metadata",
];

const INTERNAL_ONLY: [Condition; 1] = [Condition::role(MinRole::Internal)];
const ADMIN_ONLY: [Condition; 1] = [Condition::role(MinRole::Admin)];
const OWNER_ONLY: [Condition; 1] = [Condition::role(MinRole::Owner)];
const VISITOR_OK: [Condition; 1] = [Condition::visitor_allowed()];

/// Everything the four folded gates say, in one place.
///
/// Returns the registry rather than a compiled table: compilation needs
/// the authoritative operation list, which comes from the specs.
pub fn declarations() -> PolicyRegistry {
    let mut reg = PolicyRegistry::new();

    // ── require_internal_user_layer (main.rs, 3 DAV nests) ──
    //
    // "External users have no calendar, no address book, and no home
    // folder — locking them out of these protocol subtrees in one place
    // avoids leaking the protocol surface to a principal kind that can do
    // nothing with it."
    //
    // `MinRole::Internal` reproduces it exactly, including the part the
    // bool could not express: the layer asks `!kind.is_internal()`, so it
    // refuses a VISITOR as well as an external user.
    for method in DAV_METHODS {
        reg.declare(Site::Exact(Op::Dav(method)), INTERNAL_ONLY.to_vec());
    }

    // ── admin middleware (/api/admin/*) ──
    //
    // One prefix for the whole subtree, which is what the middleware is.
    reg.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY.to_vec());

    // ── the hand-rolled owner check in transfer_ownership ──
    //
    // `auth_application_service::transfer_ownership`:
    // `if caller.role != UserRole::Owner`. Verified to have exactly ONE
    // door (`admin_handler.rs`), so one declaration leaves nothing
    // uncovered — see AGENTS.md § Grant vs scope on counting doors.
    //
    // This TIGHTENS the `/api/admin` prefix above: the chain folds to
    // `Owner` by `max`. Declaring `Internal` here instead would be a boot
    // error rather than a silent discard.
    reg.declare(
        Site::RestPrefix("/api/admin/transfer-ownership"),
        OWNER_ONLY.to_vec(),
    );

    // ── anonymous_allowlist_layer (1 site, 7 routes) ──
    //
    // The layer itself STAYS: it needs `MatchedPath` and must remain a
    // `route_layer`. What folds is the DECISION — see § Fold in.
    for route in ANONYMOUS_ALLOWLISTED_ROUTES {
        reg.declare(Site::RestPrefix(route), VISITOR_OK.to_vec());
    }

    reg
}

/// Every operation the declarations above are meant to cover, for the
/// orphan cross-check. NOT the full 279 — that list comes from
/// `openapi.json`, and the test below reads it.
pub fn dav_ops() -> Vec<Op> {
    DAV_METHODS.into_iter().map(Op::Dav).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::scope_ports::{MinPrincipal, Policy};

    /// Compile against the REAL operation list: every REST path in
    /// `resources/gen/openapi.json` crossed with its methods, plus the DAV
    /// operations. This is the cross-check § 2b asks for — it is what
    /// catches a declaration for a path that was renamed, since axum
    /// cannot be asked which paths exist.
    fn all_ops() -> Vec<(String, String)> {
        let spec = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/gen/openapi.json"
        ))
        .expect("openapi.json is committed");
        let v: serde_json::Value = serde_json::from_str(&spec).expect("valid json");
        let paths = v["paths"].as_object().expect("paths object");

        let mut ops = Vec::new();
        for (path, item) in paths {
            for method in item.as_object().into_iter().flatten().map(|(m, _)| m) {
                if matches!(
                    method.as_str(),
                    "get" | "post" | "put" | "delete" | "patch" | "head" | "options"
                ) {
                    ops.push((method.to_uppercase(), path.clone()));
                }
            }
        }
        assert!(
            ops.len() > 200,
            "expected the full REST surface, got {}",
            ops.len()
        );
        ops
    }

    /// The whole table compiles, with no boot refusal and no orphan.
    ///
    /// An orphan here means a declared path matches nothing in the spec —
    /// a typo or a renamed route, gating nothing. Nothing else in the
    /// design would notice.
    #[test]
    fn the_declarations_compile_against_the_real_operation_list() {
        let rest = all_ops();
        let mut ops: Vec<Op> = rest
            .iter()
            .map(|(m, p)| Op::Rest(leak(m), leak(p)))
            .collect();
        ops.extend(dav_ops());

        match declarations().compile_all(&ops) {
            Ok(table) => assert!(table.len() > 200),
            Err(errs) => panic!(
                "the transcription does not compile:\n{}",
                errs.iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }
    }

    /// `Op` holds `&'static str`, and the spec is read at runtime. Leaking
    /// is the honest way to bridge that in a test: the strings live for
    /// the process, which is exactly what a boot-time table would do
    /// anyway.
    fn leak(s: &str) -> &'static str {
        Box::leak(s.to_owned().into_boxed_str())
    }

    /// Spot-check the four gates against the layers they transcribe. A
    /// reviewer can read these four assertions instead of the whole dump.
    #[test]
    fn each_folded_gate_lands_where_its_layer_was() {
        // Every DECLARED operation has to be in the list, or the orphan
        // check fires on the ones this test does not name and the
        // compile fails for an unrelated reason.
        let mut ops = dav_ops();
        ops.extend(
            ANONYMOUS_ALLOWLISTED_ROUTES
                .iter()
                .map(|r| Op::Rest("GET", r)),
        );
        ops.extend([
            Op::Rest("GET", "/api/admin/users"),
            Op::Rest("POST", "/api/admin/transfer-ownership"),
            Op::Rest("GET", "/api/folders"),
        ]);
        let table = declarations().compile_all(&ops).expect("compiles");

        // require_internal_user_layer
        assert_eq!(
            table.get(&Op::Dav("PROPFIND")).unwrap().role,
            Some(MinRole::Internal)
        );

        // admin middleware
        assert_eq!(
            table
                .get(&Op::Rest("GET", "/api/admin/users"))
                .unwrap()
                .role,
            Some(MinRole::Admin)
        );

        // the owner check, TIGHTENING the admin prefix rather than
        // replacing it
        assert_eq!(
            table
                .get(&Op::Rest("POST", "/api/admin/transfer-ownership"))
                .unwrap()
                .role,
            Some(MinRole::Owner)
        );

        // anonymous_allowlist_layer — per ROUTE
        assert_eq!(
            table
                .get(&Op::Rest("GET", "/api/files/{id}"))
                .unwrap()
                .principal,
            MinPrincipal::Visitor
        );

        // ...and its non-allowlisted sibling is untouched. This is the
        // assertion that fails if someone declares the subtree instead of
        // the seven routes.
        assert_eq!(
            table.get(&Op::Rest("GET", "/api/folders")),
            Some(&Policy::default()),
            "a non-allowlisted route must keep the safe default"
        );
    }

    /// The allowlist is copied verbatim from the middleware, so a route
    /// added there and not here would silently lose visitor access. Seven
    /// is the count today; changing it must be deliberate.
    #[test]
    fn the_allowlist_transcription_matches_the_middleware() {
        use crate::interfaces::middleware::anonymous_allowlist::ANONYMOUS_ALLOWLIST;

        assert_eq!(
            ANONYMOUS_ALLOWLISTED_ROUTES.len(),
            ANONYMOUS_ALLOWLIST.len(),
            "the allowlist changed and this transcription did not"
        );
        for route in ANONYMOUS_ALLOWLIST {
            assert!(
                ANONYMOUS_ALLOWLISTED_ROUTES.contains(route),
                "{route} is allowlisted in the middleware but not declared here"
            );
        }
    }
}
