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

use crate::application::ports::scope_ports::{Access, Condition, MinRole, Op, Scope};
use crate::application::services::scope_table::{PolicyRegistry, Site};

/// The DAV areas and the surfaces that reach them.
///
/// Keyed by AREA rather than protocol surface, because a protocol
/// vocabulary "would name the same permissions twice" (§ Areas, not
/// protocols) — and because the method contributes only read-vs-write, so
/// six declarations replace thirty-six.
///
/// **The NextCloud doors are deliberately ABSENT**, and that is a
/// correction rather than an omission. `/remote.php/dav/files` reaches the
/// same files area, so keying by area tempted a declaration covering both
/// doors at once — but PR 1 is a FOLD, and no folded layer covers
/// `/remote.php/*`: the NC router carries only `access_log` in `main.rs`,
/// and `require_internal_user_layer` appears nowhere under
/// `interfaces/nextcloud/`. Declaring `MinRole::Internal` for an NC
/// operation would therefore ADD a gate it does not have today, which is
/// a behaviour change wearing a transcription's clothes.
///
/// The asymmetry it exposes is real and wants its own diff: native
/// `/webdav` refuses an external user and `/remote.php/dav/files` appears
/// not to. See the plan, § Fold in.
pub const DAV_AREAS: [(Scope, &[&str]); 3] = [
    (Scope::Files, &["/webdav"]),
    (Scope::Calendar, &["/caldav"]),
    (Scope::Contacts, &["/carddav"]),
];

/// Which DAV methods READ. Everything else is treated as a write.
///
/// Fail-closed by omission: an unlisted or future method is classified
/// `ReadWrite`, so it demands the stronger scope. The opposite default
/// would let a method nobody classified through on a read-only
/// credential.
///
/// `LOCK` and `UNLOCK` are writes — they mutate lock state — which is
/// the classification most likely to be got wrong by reading the verb
/// alone.
pub const DAV_READ_METHODS: [&str; 5] = ["GET", "HEAD", "OPTIONS", "PROPFIND", "REPORT"];

/// Read or write, for scope purposes. See [`DAV_READ_METHODS`] on why an
/// unknown method is a write.
pub fn dav_access(method: &str) -> Access {
    if DAV_READ_METHODS.contains(&method) {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    }
}

/// The area a DAV path belongs to, or `None` if it is not a DAV path.
///
/// SEGMENT-aware, like the REST prefix match in `scope_table`: a plain
/// `starts_with` would let a hypothetical `/webdavious` inherit the DAV
/// area, which is the same class of mistake as `/api/admin` capturing
/// `/api/administrators` — a widening with no error anywhere.
///
/// No route like that exists today, so this is hardening rather than a
/// fix. It costs one character comparison, and the alternative is relying
/// on nobody ever adding such a route.
pub fn dav_area(path: &str) -> Option<Scope> {
    DAV_AREAS
        .iter()
        .find(|(_, prefixes)| prefixes.iter().any(|p| segment_prefix(path, p)))
        .map(|(area, _)| *area)
}

/// `path` is `prefix`, or `prefix` followed by a segment break.
fn segment_prefix(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// The visitor-reachable routes come from `ANONYMOUS_ALLOWLIST` itself,
/// not a copy of it. An earlier version of this file duplicated the seven
/// paths and cross-checked the two lists with a test — which defended the
/// duplication instead of removing it.
const INTERNAL_ONLY: [Condition; 1] = [Condition::role(MinRole::Internal)];
const OWNER_ONLY: [Condition; 1] = [Condition::role(MinRole::Owner)];

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
    // Six declarations, not thirty-six: the method contributes exactly
    // one authorization-relevant bit (read or write), and the gate is the
    // same for every area.
    for (area, _) in DAV_AREAS {
        for access in [Access::ReadOnly, Access::ReadWrite] {
            reg.declare(Site::Exact(Op::Dav(area, access)), INTERNAL_ONLY.to_vec());
        }
    }

    // ── require_internal_user_layer, the three NON-DAV sites ──
    //
    // `Site::Exact`, not `RestPrefix`, because two of these gate ONE
    // METHOD on a path whose other methods stay open — the layer is on a
    // sub-router holding only the create route (`routes.rs`:
    // `music_create`, and the address-book equivalent). A path-level
    // declaration would widen the gate to GET and cut external users off
    // from listing the books shared WITH them, which the comment at that
    // site calls out as deliberate: "gating the nest would make that
    // branch dead code and cut guests off from books shared with them".
    //
    // This is the per-method case I earlier recorded as absent: the two
    // per-method layers in the tree are rate limiters, but these two
    // sub-routers achieve per-method AUTHZ by routing, which a grep for
    // `.layer(` on a `MethodRouter` does not find.
    for op in [
        Op::Rest("POST", "/api/playlists"),
        Op::Rest("POST", "/api/address-books"),
    ] {
        reg.declare(Site::Exact(op), INTERNAL_ONLY.to_vec());
    }

    // The `/api/admin` subtree is NOT declared here: it is declared at
    // its nest in `routes.rs` — `admin_router.policy([role(Admin)])` —
    // because route, layer and policy belong in one place.
    //
    // What stays here is what has no route to declare against: the DAV
    // operations, and the per-METHOD gates below.

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

    // The seven share-visitor GETs are NOT declared here any more:
    // each is `policy_get(handler).policy([visitor_allowed()])` at its
    // route in `routes.rs`, per VERB — three of those paths also serve a
    // write, and a path-level declaration handed a visitor the DELETE.
    //
    // `GET /api/files/recent-changes` moved for the same reason.

    reg
}

/// Every operation the declarations above are meant to cover, for the
/// orphan cross-check. NOT the full 279 — that list comes from
/// `openapi.json`, and the test below reads it.
pub fn dav_ops() -> Vec<Op> {
    DAV_AREAS
        .into_iter()
        .flat_map(|(area, _)| {
            [Access::ReadOnly, Access::ReadWrite]
                .into_iter()
                .map(move |a| Op::Dav(area, a))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::scope_ports::Policy;

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
        ops.extend([
            Op::Rest("POST", "/api/admin/transfer-ownership"),
            Op::Rest("GET", "/api/folders"),
            // The three non-DAV internal-user sites, plus the GET
            // siblings that must stay OPEN.
            Op::Rest("POST", "/api/playlists"),
            Op::Rest("GET", "/api/playlists"),
            Op::Rest("POST", "/api/address-books"),
            Op::Rest("GET", "/api/address-books"),
            Op::Rest("GET", "/api/files/recent-changes"),
        ]);
        let table = declarations().compile_all(&ops).expect("compiles");

        // require_internal_user_layer
        assert_eq!(
            table
                .get(&Op::Dav(Scope::Files, Access::ReadOnly))
                .unwrap()
                .role,
            Some(MinRole::Internal)
        );

        // The `/api/admin` subtree is NOT asserted here any more: it is
        // declared at its nest in `routes.rs`, so only a test that built
        // the real router could see it — and that needs a full
        // `AppState`. `admin_role_hierarchy.hurl` covers it instead,
        // which is the right layer for production wiring.

        // the owner check, TIGHTENING the admin prefix rather than
        // replacing it
        assert_eq!(
            table
                .get(&Op::Rest("POST", "/api/admin/transfer-ownership"))
                .unwrap()
                .role,
            Some(MinRole::Owner)
        );

        // The visitor declarations are no longer here — they are at their
        // routes. `policy_router`'s non-leak test covers the shape and
        // `gate_matrix.hurl` the wiring.

        // require_internal_user_layer's non-DAV sites gate ONE METHOD.
        // The GET siblings must stay open, or external users lose the
        // ability to list the address books shared WITH them — which
        // `routes.rs` calls out as deliberate.
        // `recent-changes` is declared at its route now, so it is not in
        // the table a unit test sees.
        for gated in [
            Op::Rest("POST", "/api/playlists"),
            Op::Rest("POST", "/api/address-books"),
        ] {
            assert_eq!(
                table.get(&gated).unwrap().role,
                Some(MinRole::Internal),
                "{gated} must require an internal user"
            );
        }
        for open in [
            Op::Rest("GET", "/api/playlists"),
            Op::Rest("GET", "/api/address-books"),
        ] {
            assert_eq!(
                table.get(&open),
                Some(&Policy::default()),
                "{open} must stay open to externals — a path-level \
                 declaration would have widened the gate onto it"
            );
        }
    }

    // The visitor declarations moved to their routes, so their
    // assertions moved too: the mechanism is pinned by
    // `policy_router::tests::the_visitor_declaration_does_not_leak_up_down_or_sideways`
    // (declared GET admits a visitor; the same path's other verb, the
    // collection above and a subpath below do not), and the production
    // wiring by `gate_matrix.hurl`. A unit test here could not see them —
    // building the real router needs a full `AppState`.
}
