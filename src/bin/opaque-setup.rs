//! `opaque-setup` — one-shot operator helper that mints a fresh
//! [`opaque_ke::ServerSetup`] and prints its base64 encoding to stdout.
//!
//! The output goes into `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP` (env var or secrets
//! manager) and MUST be persisted verbatim. Rotating it invalidates every
//! user's registration — treat it like the JWT secret, only more so.
//!
//! Usage:
//! ```text
//! cargo run --bin opaque-setup > opaque_setup.b64
//! # or paste directly into your env / .env file:
//! echo "OXICLOUD_AUTH_OPAQUE_SERVER_SETUP=$(cargo run --bin opaque-setup)" >> .env
//! ```
//!
//! The generated value is a small (~64 byte) Ristretto255 keypair
//! serialised for storage. Nothing else — no config file, no key
//! rotation state. Idempotent per invocation (each run generates a
//! DIFFERENT value; only run it once per deployment).

use oxicloud::infrastructure::services::opaque_service::OpaqueService;

fn main() {
    let b64 = OpaqueService::generate_server_setup_b64();
    // Print JUST the value — no trailing newline commentary — so shell
    // pipelines (`OXICLOUD_AUTH_OPAQUE_SERVER_SETUP=$(cargo run --bin opaque-setup)`)
    // capture cleanly without needing `tr -d '\n'` afterwards.
    println!("{b64}");
    // Guidance goes to stderr so it doesn't contaminate the pipeline.
    eprintln!();
    eprintln!("=== OPAQUE server setup generated. ===");
    eprintln!("Persist the line above in OXICLOUD_AUTH_OPAQUE_SERVER_SETUP.");
    eprintln!("NEVER rotate: rotating invalidates every user's registration.");
    eprintln!("Treat this value like your JWT secret.");
}
