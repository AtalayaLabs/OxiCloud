pub mod api;
pub mod errors;
pub mod metrics;
pub mod middleware;
pub mod nextcloud;
pub mod range_requests;
pub mod upload_ingest;
pub mod web;

pub use api::create_api_routes;
pub use api::create_health_routes;
pub use api::create_public_api_routes;
