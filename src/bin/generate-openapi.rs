use oxicloud::common::config::AppConfig;
use oxicloud::common::di::AppServiceFactory;
use oxicloud::common::spec_tag::require_release_tag_or_exit;
use oxicloud::infrastructure::db::DbPools;
use oxicloud::interfaces::api::{ApiDoc, mark_policies};
use sqlx::postgres::PgPoolOptions;
use std::fs;
use std::path::PathBuf;
use utoipa::OpenApi;

/// Writes `resources/gen/openapi.json`.
///
/// # Why this builds the whole service graph
///
/// The per-operation security markers are DERIVED from the compiled policy
/// table, not from a list kept alongside it — so the published spec and the
/// enforced gate are one thing rather than two that agree. The table is
/// produced by `create_api_routes`, because policies are declared at their
/// routes (`policy_get(h).policy([...])`), and a `Router` cannot be asked
/// what it holds.
///
/// So this needs an `AppState` VALUE. It does NOT need a database:
///
/// * `DbPools` holds two `PgPool`s, and `connect_lazy` builds one without
///   contacting a server.
/// * `build_app_state` issues exactly two queries while constructing —
///   `load_ops_banners` and `load_migration_readonly` — and both tolerate
///   failure by design, logging a warning and returning a default. That is
///   what makes a dead pool survivable here.
/// * Nothing in `di.rs` WRITES to the filesystem during construction. It
///   does READ one thing — the locale directory, which `with_config`
///   turns into a registry and panics without — so that path is anchored
///   on `CARGO_MANIFEST_DIR` rather than the config default.
///
/// It spawns a few background tickers that die with the process, and logs
/// those two warnings. Nothing persists, and no real instance is touched.
#[tokio::main]
async fn main() {
    // `info.version` carries OXICLOUD_TAG (see the utoipa `info()` in
    // src/interfaces/api/mod.rs). Refuse to write a spec stamped with a
    // bare commit SHA — it would pass locally and fail CI's drift job for
    // everyone. See src/common/spec_tag.rs.
    require_release_tag_or_exit("generate-openapi", env!("OXICLOUD_TAG"));

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut openapi = ApiDoc::openapi();

    // A pool that never connects. The router is only CONSTRUCTED here, so
    // no query runs through it.
    let lazy = |url: &str| {
        PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy(url)
            .expect("a parseable connection string")
    };
    let config = AppConfig::default();
    let url = &config.database.connection_string;
    let pools = DbPools {
        primary: lazy(url),
        maintenance: lazy(url),
    };

    // The locales path IS read — `with_config` builds the locale registry
    // from it and panics if the directory is missing. Anchored on
    // `CARGO_MANIFEST_DIR` so `just openapi` works from any cwd. The
    // storage path is never touched: no request is served here.
    let factory = AppServiceFactory::with_config(
        config.storage_path.clone(),
        manifest_dir.join("frontend").join("static").join("locales"),
        config.clone(),
    );
    let state = factory
        .build_app_state(Some(pools))
        .await
        .expect("service graph builds without a database");
    // The SAME function the server installs its registry from, so the
    // spec cannot be marked from a different table than the gate
    // enforces. Nothing here is decorated with middleware, which is sound
    // because a layer cannot change a declaration.
    let registry = oxicloud::interfaces::api::routes::app_policies(&state.into());

    // Spec generation validates the declarations: `security(...)` is
    // derived from the policy, so an annotation that disagrees is stale or
    // a declaration is missing. Reported, not fatal, while the two are
    // being reconciled.
    let findings = oxicloud::interfaces::api::audit_declared_security(&openapi, &registry);
    if findings.is_empty() {
        println!("auth declarations: macro and policy agree on every operation");
    } else {
        println!(
            "auth declarations: {} operation(s) where the macro and the policy disagree",
            findings.len()
        );
        for (method, path, macro_says, policy_says) in &findings {
            println!("  {method:7} {path:52} macro={macro_says:8} policy={policy_says}");
        }
    }

    mark_policies(&mut openapi, &registry);

    let json =
        serde_json::to_string_pretty(&openapi).expect("Failed to serialize OpenAPI spec to JSON");

    let resources_gen_dir = manifest_dir.join("resources").join("gen");
    fs::create_dir_all(&resources_gen_dir).expect("Failed to create resources/gen directory");

    let output_path = resources_gen_dir.join("openapi.json");
    fs::write(&output_path, json).expect("Failed to write OpenAPI spec to file");

    println!("OpenAPI spec written to {}", output_path.display());
}
