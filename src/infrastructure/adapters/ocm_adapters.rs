use crate::domain::repositories::user_repository::UserRepository;
use crate::domain::repositories::user_repository::UserRepositoryError;
use ocm_server_axum::drivers::users::UserRepo;
use ocm_server_axum::drivers::users::User as OcmUser;
use ocm_server_axum::drivers::users::UserRepoError as OcmUserRepoError;

#[derive(Clone)]
struct OcmUserRepo<T: UserRepository>(T);

impl<T: UserRepository> From<T> for OcmUserRepo<T> {
    fn from(value: T) -> Self {
        OcmUserRepo(value)
    }
}

impl<T> UserRepo for OcmUserRepo<T>
where
    T: UserRepository + Clone,
{
    async fn get(&self, user_id: &str) -> Result<OcmUser, OcmUserRepoError> {
        let id = user_id
            .parse::<uuid::Uuid>()
            .map_err(|_| OcmUserRepoError::NotFound(user_id.to_owned()))?;

        let user = self.0
            .get_user_by_id(id)
            .await
            .map_err(OcmUserRepoError::from)?;

        Ok(OcmUser {
            id: user.id().to_string(),
            name: user.display_full(false),
        })
    }
}

impl From<UserRepositoryError> for OcmUserRepoError {
    fn from(error: UserRepositoryError) -> Self {
        match error {
            UserRepositoryError::NotFound(id) => OcmUserRepoError::NotFound(id),

            // These are not really "not found", but the narrower UserRepo
            // interface has no corresponding error variants. Treat them as
            // repository failures rather than leaking implementation details.
            UserRepositoryError::AlreadyExists(_)
            | UserRepositoryError::DatabaseError(_)
            | UserRepositoryError::ValidationError(_)
            | UserRepositoryError::Timeout(_)
            | UserRepositoryError::OperationNotAllowed(_) => {
                OcmUserRepoError::RepoAccessFailed
            }
        }
    }
}
