use axum::{Router, http::Uri};
use ocm_server_axum::{
    drivers::shares::{InMemorySentShareRepo, InMemoryShareRepo},
    http_signatures::OcmSigningKey,
    setup_ocm_router,
};
use rand_core::RngCore;
use std::{str::FromStr, sync::Arc};

use crate::{
    common::di::AppState,
    domain::entities::folder::Folder,
    infrastructure::{adapters::ocm_adapters::OcmUserRepo, repositories::UserPgRepository},
};

pub async fn create_opencloudmesh_routes(app_state: &Arc<AppState>) -> Option<Router> {
    if let Some(opencloudmesh_service) = app_state.opencloudmesh_service.clone() {
        let received_shares = InMemoryShareRepo::default();
        let sent_shares: InMemorySentShareRepo<Folder> = InMemorySentShareRepo::new();
        let users: OcmUserRepo<UserPgRepository> =
            crate::infrastructure::repositories::pg::UserPgRepository::new(
                app_state.db_pool.clone()?,
            )
            .into();
        // let supported_protocols: Arc<Vec<Box<dyn Protocol>>> =
        //     Arc::new(vec![Box::new(Webdav::new("/webdav".parse().unwrap()))]);
        let mut secret_key = [0; 32];
        let mut rng = rand_core::OsRng;
        rng.fill_bytes(secret_key.as_mut_slice());
        assert_ne!(secret_key, [0; 32]);
        let signing_key = OcmSigningKey::new(
            "ed25519",
            format!(
                "{}/.well-known/jwks.json#1",
                &app_state.core.config.base_url().trim_end_matches("/")
            ),
            secret_key.to_vec(),
        )
        .unwrap();
        setup_ocm_router(
            opencloudmesh_service.get_client().clone(),
            received_shares,
            sent_shares,
            users,
            &["file", "folder"],
            Some(&signing_key),
            true,
            Uri::from_str(&app_state.core.config.base_url()).ok()?, // FIXME: this should return an
            // Error instead of None
            opencloudmesh_service.get_protocols(),
        )
        .await
        .ok()
    } else {
        None
    }
}
