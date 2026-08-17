use axum::{Router, http::Uri};
use ocm_server_axum::{
    drivers::shares::{InMemorySentShareRepo, InMemoryShareRepo},
    http_client::ReqwestClient,
    setup_ocm_router,
};
use std::{str::FromStr, sync::Arc};

use crate::{
    common::di::AppState,
    domain::entities::folder::Folder,
    infrastructure::{adapters::ocm_adapters::OcmUserRepo, repositories::UserPgRepository},
};

pub async fn create_opencloudmesh_routes(app_state: &Arc<AppState>) -> Option<Router> {
    let http_client = ReqwestClient::new(None, false).await;
    let received_shares = InMemoryShareRepo::default();
    let sent_shares: InMemorySentShareRepo<Folder> = InMemorySentShareRepo::new();
    let users: OcmUserRepo<UserPgRepository> =
        crate::infrastructure::repositories::pg::UserPgRepository::new(app_state.db_pool.clone()?)
            .into();
    let supported_protocols = Arc::new(vec![]);
    setup_ocm_router(
        http_client,
        received_shares,
        sent_shares,
        users,
        &["file", "folder"],
        None, // TODO setup signature key
        false,
        Uri::from_str(&app_state.core.config.base_url()).ok()?, // FIXME: this should return an
        // Error instead of None
        supported_protocols,
    )
    .await
    .ok()
}
