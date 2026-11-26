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
/// The NextCloud mirror is listed HERE rather than given a mechanism of
/// its own, which is § Areas, not protocols paying off: `/webdav` and
/// `/remote.php/dav/files` are the same area reached by two protocols, so
/// they map to the SAME `Op::Dav(Files, access)` and need no second
/// declaration. That is also what closes the hole — those NC paths are
/// where an app password actually goes, and until they were listed the
/// scope axis never ran on them.
///
/// Note what is NOT here: a bare `/remote.php/dav`. It would capture
/// `/remote.php/dav/avatars` by segment prefix, and an avatar is a
/// profile image — `account`, not `files`. Listing the leaves explicitly
/// is what keeps that distinction.
pub const DAV_AREAS: [(Scope, &[&str]); 4] = [
    (
        Scope::Files,
        &[
            "/webdav",
            "/remote.php/webdav",
            "/remote.php/dav/files",
            "/remote.php/dav/trashbin",
            "/remote.php/dav/uploads",
        ],
    ),
    (Scope::Calendar, &["/caldav"]),
    (Scope::Contacts, &["/carddav"]),
    // Avatars are profile images, so `account` — and `account:ro` is in
    // the app-password ceiling for exactly this reason: NC desktop
    // fetches them, and a sync client losing avatars is a visible
    // regression for no security gain.
    (Scope::Account, &["/remote.php/dav/avatars"]),
];

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

/// The visitor-reachable GETs are declared at their routes, per verb — not
/// here. Three of those paths also serve a write, which no visitor may
/// reach.
const INTERNAL_ONLY: [Condition; 1] = [Condition::role(MinRole::Internal)];

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
    // Both axes, and the SCOPE one writes itself: `Op::Dav` already
    // carries the area and the access, so the declaration is the
    // operation's own identity restated as a requirement. Nothing to
    // choose and nothing to get wrong.
    //
    // These are the operations the scope axis exists for. `/webdav`,
    // `/caldav` and `/carddav` are what app passwords and device codes
    // are issued for, so a credential scoped to `calendar` reaching
    // `/webdav` is the concrete bug § The feature is half-built
    // describes. Until this declaration they were `ScopeReq::Undefined`,
    // and invisible to the generation check for want of a spec entry —
    // which is why `undefined_scopes` takes `dav_ops()` explicitly.
    for (area, _) in DAV_AREAS {
        for access in [Access::ReadOnly, Access::ReadWrite] {
            let mut conds = INTERNAL_ONLY.to_vec();
            conds.push(Condition::scope(area, access));
            reg.declare(Site::Exact(Op::Dav(area, access)), conds);
        }
    }

    // The internal-only POSTs (`/api/playlists`, `/api/address-books`)
    // and `GET /api/files/recent-changes` are declared at their routes,
    // per verb — the LIST on each of those paths stays open, so the verb
    // is the point.

    // The `/api/admin` subtree is NOT declared here: it is declared at
    // its nest in `routes.rs` — `admin_router.policy([role(Admin)])` —
    // because route, layer and policy belong in one place.
    //
    // What stays here is what has no route to declare against: the DAV
    // operations, and the per-METHOD gates below.

    // `transfer-ownership`'s OWNER declaration is NOT here: it is
    // declared per verb at its route in `admin_handler`, tightening the
    // admin nest. Nothing in this file has a route any more — only the
    // DAV operations, which have none.

    // The seven share-visitor GETs are NOT declared here any more:
    // each is `policy_get(handler).policy([visitor_allowed()])` at its
    // route in `routes.rs`, per VERB — three of those paths also serve a
    // write, and a path-level declaration handed a visitor the DELETE.
    //
    // `GET /api/files/recent-changes` moved for the same reason.

    // ── The NextCloud OCS surface ─────────────────────────────────────
    //
    // Declared HERE rather than at the routes, for the same reason as
    // `/api/wopi/editor-url`: `interfaces/nextcloud/routes.rs` builds
    // plain `Router`s merged at the app root, so there is no
    // `GatedRouter` to carry a `.policy()`.
    //
    // They have to be declared at all because the policy gate now runs on
    // that router (PR 2 step 4b). Eleven of them sit inside it, and
    // without a declaration each compiles to `ScopeReq::Undefined` —
    // harmless while the runtime treats that as "no requirement", and a
    // 403 the moment step 10 makes it a denial. The generation check
    // cannot catch it either: `/ocs/*` is in no spec.
    //
    // `scope_by_verb` is NOT used. OCS does not follow the verb: `POST
    // …/notifications/push` registers a push subscription (a write to the
    // caller's own notification settings), while the GETs are reads. Each
    // one states its access.
    for (method, path, conds) in [
        // The caller's own metadata — § Deliberately scopeless names it a
        // probe: any credential may ask who it belongs to.
        ("GET", "/ocs/v1.php/cloud/user", Condition::scopeless()),
        ("GET", "/ocs/v2.php/cloud/user", Condition::scopeless()),
        // ANOTHER user's metadata, and the share-recipient picker. Both
        // reveal the roster, which § Areas calls one capability.
        (
            "GET",
            "/ocs/v1.php/cloud/users/{userid}",
            Condition::scope(Scope::Sharing, Access::ReadOnly),
        ),
        (
            "GET",
            "/ocs/v2.php/cloud/users/{userid}",
            Condition::scope(Scope::Sharing, Access::ReadOnly),
        ),
        (
            "GET",
            "/ocs/v2.php/apps/files_sharing/api/v1/sharees",
            Condition::scope(Scope::Sharing, Access::ReadOnly),
        ),
        // **The one that matters most.** NC's own app-password endpoint:
        // deleting it revokes a CREDENTIAL. `security`, which no
        // non-interactive ceiling grants — so an app password cannot
        // reach the NC credential surface any more than it can reach
        // `/api/auth/app-passwords`. Without this it was `Undefined`, and
        // an app password could revoke another one.
        (
            "DELETE",
            "/ocs/v2.php/core/apppassword",
            Condition::scope(Scope::Security, Access::ReadWrite),
        ),
        // The caller's own notifications.
        (
            "GET",
            "/ocs/v2.php/apps/notifications/api/v2/notifications",
            Condition::scope(Scope::Account, Access::ReadOnly),
        ),
        // A POST that WRITES: registering a push subscription changes the
        // caller's own notification settings.
        (
            "POST",
            "/ocs/v2.php/apps/notifications/api/v2/push",
            Condition::scope(Scope::Account, Access::ReadWrite),
        ),
        // Recommendations and search read files.
        (
            "GET",
            "/ocs/v2.php/apps/recommendations/api/v1/recommendations",
            Condition::scope(Scope::Files, Access::ReadOnly),
        ),
        (
            "GET",
            "/ocs/v2.php/search/providers",
            Condition::scope(Scope::Files, Access::ReadOnly),
        ),
        (
            "GET",
            "/ocs/v2.php/search/providers/{provider_id}/search",
            Condition::scope(Scope::Files, Access::ReadOnly),
        ),
    ] {
        reg.declare(Site::Exact(Op::Rest(method, path)), vec![conds]);
    }

    // `GET /api/wopi/editor-url` — declared HERE, not at its route,
    // because `/api/wopi` is mounted separately in `main.rs` rather than
    // nested into `main_api_router`, so a declaration made on its router
    // never reaches this registry. The route carries the same
    // declaration for the gate; this one is what the SPEC and the
    // generation check see.
    //
    // `files:rw` against the verb: it is a GET, but it mints a WOPI token
    // usable for `POST /wopi/files/{id}/contents`, so a read-only
    // credential would walk away with write capability.
    reg.declare(
        Site::Exact(Op::Rest("GET", "/api/wopi/editor-url")),
        vec![Condition::scope(Scope::Files, Access::ReadWrite)],
    );

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

    /// Compile against the REAL operation list: every REST path in
    /// `resources/gen/openapi.json` crossed with its methods, plus the DAV
    /// operations. This is the cross-check § 2b asks for — it is what
    /// catches a declaration for a path that was renamed, since axum
    /// cannot be asked which paths exist.
    /// The operations declared in this module that appear in no spec.
    ///
    /// `/api/wopi/editor-url` is mounted outside `main_api_router`, and
    /// the OCS routes are plain axum `Router`s merged at the app root —
    /// so neither can carry a `.policy()` at its route, and neither is in
    /// `openapi.json` for `all_ops()` to find. One list, used by both
    /// tests, so they cannot drift apart.
    fn non_spec_ops() -> Vec<Op> {
        vec![
            Op::Rest("GET", "/api/wopi/editor-url"),
            Op::Rest("GET", "/ocs/v1.php/cloud/user"),
            Op::Rest("GET", "/ocs/v2.php/cloud/user"),
            Op::Rest("GET", "/ocs/v1.php/cloud/users/{userid}"),
            Op::Rest("GET", "/ocs/v2.php/cloud/users/{userid}"),
            Op::Rest("GET", "/ocs/v2.php/apps/files_sharing/api/v1/sharees"),
            Op::Rest("DELETE", "/ocs/v2.php/core/apppassword"),
            Op::Rest("GET", "/ocs/v2.php/apps/notifications/api/v2/notifications"),
            Op::Rest("POST", "/ocs/v2.php/apps/notifications/api/v2/push"),
            Op::Rest(
                "GET",
                "/ocs/v2.php/apps/recommendations/api/v1/recommendations",
            ),
            Op::Rest("GET", "/ocs/v2.php/search/providers"),
            Op::Rest("GET", "/ocs/v2.php/search/providers/{provider_id}/search"),
        ]
    }

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
        // Plus the operations this module declares that appear in NO
        // spec, so `all_ops()` cannot find them: `/api/wopi/editor-url`
        // (mounted outside `main_api_router`) and the gated OCS routes.
        // Without them the orphan check fires on our own declarations —
        // which it did, and correctly: a declaration naming no live
        // operation is exactly what it exists to catch.
        ops.extend(non_spec_ops());

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
    /// Every DAV operation declares a SCOPE, not just the role gate.
    ///
    /// They were `ScopeReq::Undefined` until PR 2 step 4, and invisible
    /// to the generation check because they appear in no spec — so this
    /// pins both halves: the declaration exists, and it matches the
    /// operation's own identity. `/webdav`, `/caldav` and `/carddav` are
    /// what app passwords and device codes are issued for, so an
    /// unclassified DAV operation is the exact shape of the bug the scope
    /// axis exists to fix.
    #[test]
    fn every_dav_operation_declares_its_own_area_and_access() {
        // The WOPI operation has to be named too: it is declared in this
        // module (because `/api/wopi` is mounted outside
        // `main_api_router`), so the orphan check fires on any op list
        // that omits it.
        let mut ops = dav_ops();
        ops.extend(non_spec_ops());
        let table = declarations()
            .compile_all(&ops)
            .expect("the DAV declarations compile");

        for op in dav_ops() {
            let Op::Dav(area, access) = op else {
                panic!("dav_ops() yielded a non-DAV op: {op:?}");
            };
            let p = table.get(&op).expect("every DAV op is in the table");

            let required = p
                .scope
                .areas()
                .unwrap_or_else(|| panic!("{op} declares no scope — it would fail generation"));
            assert!(
                required.allows(area, access),
                "{op} must require its OWN area and access, got {:?}",
                p.scope
            );
            // And the role gate is still there — the two axes coexist.
            assert_eq!(p.role, Some(MinRole::Internal), "{op} lost its role gate");
        }
    }

    #[test]
    fn each_folded_gate_lands_where_its_layer_was() {
        // Every DECLARED operation has to be in the list, or the orphan
        // check fires on the ones this test does not name and the
        // compile fails for an unrelated reason.
        let mut ops = dav_ops();
        // Everything this module declares that is in no spec — one
        // shared list, so the two tests cannot drift apart.
        ops.extend(non_spec_ops());
        ops.extend([
            Op::Rest("GET", "/api/folders"),
            // The three non-DAV internal-user sites, plus the GET
            // siblings that must stay OPEN.
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

        // The visitor declarations are no longer here — they are at their
        // routes. `policy_router`'s non-leak test covers the shape and
        // `gate_matrix.hurl` the wiring.

        // require_internal_user_layer's non-DAV sites gate ONE METHOD.
        // The GET siblings must stay open, or external users lose the
        // ability to list the address books shared WITH them — which
        // `routes.rs` calls out as deliberate.
        // The per-verb internal-only declarations live at their routes
        // now. Their shape — one verb gated, the sibling verb on the same
        // path left open — is pinned by
        // `policy_router::tests::the_visitor_declaration_does_not_leak_up_down_or_sideways`,
        // and the wiring by `external_users.hurl`, which asserts an
        // external caller is refused the create AND allowed the list.
    }

    // The visitor declarations moved to their routes, so their
    // assertions moved too: the mechanism is pinned by
    // `policy_router::tests::the_visitor_declaration_does_not_leak_up_down_or_sideways`
    // (declared GET admits a visitor; the same path's other verb, the
    // collection above and a subpath below do not), and the production
    // wiring by `gate_matrix.hurl`. A unit test here could not see them —
    // building the real router needs a full `AppState`.
}
