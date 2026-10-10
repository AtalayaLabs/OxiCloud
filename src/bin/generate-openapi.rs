use oxicloud::common::spec_tag::require_release_tag_or_exit;
use oxicloud::interfaces::api::ApiDoc;
use std::fs;
use std::path::PathBuf;
use utoipa::OpenApi;

fn main() {
    // `info.version` carries OXICLOUD_TAG (see the utoipa `info()` in
    // src/interfaces/api/mod.rs). Refuse to write a spec stamped with a
    // bare commit SHA — it would pass locally and fail CI's drift job for
    // everyone. See src/common/spec_tag.rs.
    require_release_tag_or_exit("generate-openapi", env!("OXICLOUD_TAG"));

    let openapi = ApiDoc::openapi();

    let json =
        serde_json::to_string_pretty(&openapi).expect("Failed to serialize OpenAPI spec to JSON");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let resources_gen_dir = manifest_dir.join("resources").join("gen");
    fs::create_dir_all(&resources_gen_dir).expect("Failed to create resources/gen directory");

    let output_path = resources_gen_dir.join("openapi.json");
    fs::write(&output_path, json).expect("Failed to write OpenAPI spec to file");

    println!(
        "OpenAPI spec generated successfully at: {}",
        output_path.display()
    );
}
