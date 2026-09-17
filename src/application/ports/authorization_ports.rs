//! Authorization port — the trait every service depends on for permission
//! decisions. Implementations: `PgAclEngine` (v1 default), `OpenFgaEngine`
//! (future). A `CachedAuthorizationEngine` decorator over either is planned
//! as a future optimization.
//!
//! Architectural rule (see CLAUDE.md):
//! **AuthZ is enforced exclusively in the application service layer.**
//! Handlers authenticate the caller and pass `caller_id` to the service;
//! they never call this trait directly.

use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::entities::user::UserRole;
use crate::domain::services::authorization::{
    Grant, GrantCursor, IncomingGrantSummary, OutgoingResourceSummary, Permission, Resource,
    ResourceKind, Role, Subject,
};

/// Discriminates the two denial shapes surfaced by
/// [`AuthorizationEngine::require_visible`] in the `authz.denied` audit line.
/// Log-aggregation consumers key off the string form via `as_str`; keep the
/// values stable — a new denial shape means a new variant, never a renamed
/// existing one.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AuthzDenialVisibility {
    /// Caller has `Read` on the resource — 403 Forbidden.
    Visible,
    /// Caller has no `Read` — 404 anti-enum.
    Hidden,
}

fn system_admin_denial_reason(
    subject: Subject,
    role: UserRole,
    is_external: bool,
    active: bool,
) -> Option<&'static str> {
    match subject {
        Subject::User(_) if !active => Some("inactive"),
        Subject::User(_) if is_external => Some("external_account"),
        // `at_least`, not `!=`: an admin gate must admit anything ranked
        // above admin too, or a more privileged role would be refused
        // admin surfaces it strictly outranks.
        Subject::User(_) if !role.at_least(UserRole::Admin) => Some("not_admin"),
        Subject::User(_) => None,
        _ => Some("unsupported_subject"),
    }
}

impl AuthzDenialVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
        }
    }
}

pub trait AuthorizationEngine: Send + Sync + 'static {
    /// Require the authenticated principal to hold the deployment-wide admin
    /// role.  System administration has no resource UUID, so it cannot be
    /// represented by [`Resource`]; it still belongs in this policy port rather
    /// than in an HTTP handler or an application-service role shortcut.
    ///
    /// The application authentication service supplies its already cached,
    /// image-free live flags.  This avoids a second database query/cache for the
    /// same caller while keeping the authorization decision and denial audit in
    /// the engine's single policy surface.
    fn require_system_admin(
        &self,
        subject: Subject,
        role: UserRole,
        is_external: bool,
        active: bool,
    ) -> Result<(), DomainError> {
        let reason = system_admin_denial_reason(subject, role, is_external, active);
        let Some(reason) = reason else {
            return Ok(());
        };

        tracing::info!(
            target: "audit",
            event = "authz.admin_denied",
            reason,
            subject_type = subject.type_str(),
            caller_id = %subject.id(),
            role = role.as_str(),
            is_external,
            active,
            "👮🏻‍♂️ system-administrator permission denied"
        );
        Err(DomainError::access_denied(
            "System",
            "Admin access required",
        ))
    }

    /// Returns true if `subject` has `permission` on `resource`, considering
    /// owner short-circuit AND cascading from folder ancestors.
    ///
    /// `check` never errors for "permission denied" — that's a `false` return.
    /// `Err` is reserved for infrastructure failures (DB down, etc.).
    async fn check(
        &self,
        subject: Subject,
        permission: Permission,
        resource: Resource,
    ) -> Result<bool, DomainError>;

    /// Batched `check(subject, Read, File(id))` over a result page: returns
    /// the subset of `file_ids` the subject may read. Semantically identical
    /// to looping [`Self::check`] (the default does exactly that); the
    /// `PgAclEngine` override resolves every file's drive in ONE query and
    /// reuses the per-drive role cache, so verifying a 200-hit search page
    /// costs 1 SQL round-trip instead of up to 200 sequential ones
    /// (benches/SEARCH-REBAC.md).
    async fn check_files_read_batch(
        &self,
        subject: Subject,
        file_ids: &[Uuid],
    ) -> Result<std::collections::HashSet<Uuid>, DomainError> {
        let mut allowed = std::collections::HashSet::with_capacity(file_ids.len());
        for id in file_ids {
            if self
                .check(subject, Permission::Read, Resource::File(*id))
                .await?
            {
                allowed.insert(*id);
            }
        }
        Ok(allowed)
    }

    /// Allow when **any** of `subjects` grants `permission` on `resource`.
    ///
    /// A browser can hold more than one credential at once: a logged-in user
    /// who has also unlocked public-share links carries their own identity
    /// plus a *share ring* of token subjects. The request cannot say which
    /// one it means — an `<img>` tag sends every cookie it has and nothing
    /// else — so authorization tries what the caller holds.
    ///
    /// This is not a new authority model. `subject_match_set` already unions
    /// a user with every group they belong to and asks whether *any* of them
    /// grants access; share tokens join that union. The only difference is
    /// that group membership is resolved server-side while the ring arrives
    /// in a (server-signed) cookie.
    ///
    /// **Order matters for cost, not for outcome.** Callers pass the user
    /// subject first, so ordinary browsing is decided on the first check and
    /// the ring is consulted only when that denies. The consequence is that
    /// denials are more expensive than allows — bounded by the ring cap and
    /// largely absorbed by the engine's decision cache.
    ///
    /// On total denial the shape is produced by [`Self::require`] using the
    /// first subject, so graduated visibility and the audit line are not
    /// duplicated here. That re-checks the first subject, which the cache has
    /// just answered.
    ///
    /// Returns the subject that granted. Callers that go on to make a second,
    /// single-subject service call for the same resource must pass this one
    /// back — passing any other member of the set would re-run the search and
    /// could deny what was just allowed.
    ///
    /// Bind it as `authorized_as`, never `granted_by`: `role_grants.granted_by`
    /// is the user who ISSUED the grant (the sharer), so that name here would
    /// denote the opposite party.
    async fn require_any(
        &self,
        subjects: &[Subject],
        permission: Permission,
        resource: Resource,
    ) -> Result<Subject, DomainError> {
        for &subject in subjects {
            if self.check(subject, permission, resource).await? {
                tracing::debug!(
                    target: "oxicloud::authz",
                    event = "authz.allowed",
                    subject_type = subject.type_str(),
                    subject_id = %subject.id(),
                    permission = permission.as_str(),
                    resource_type = resource.type_str(),
                    resource_id = %resource.id(),
                    "👮🏻‍♂️ perms: ✔ '{subject}' has '{permission}' on '{resource}'",
                );
                return Ok(subject);
            }
        }

        match subjects.first() {
            Some(&primary) => self
                .require(primary, permission, resource)
                .await
                .map(|()| primary),
            // No credential at all. Unreachable through the extractors, which
            // refuse a request with no principal — but fail closed rather
            // than treating "nothing to check" as "nothing objected".
            None => {
                tracing::warn!(
                    target: "audit",
                    event = "authz.denied",
                    reason = "no_subjects",
                    permission = permission.as_str(),
                    resource_type = resource.type_str(),
                    resource_id = %resource.id(),
                    "👮🏻‍♂️ authorization attempted with an empty subject set",
                );
                Err(DomainError::not_found(
                    resource.type_str(),
                    resource.id().to_string(),
                ))
            }
        }
    }

    /// Graduated-denial wrapper around `check`. Semantics:
    ///
    /// - `permission` granted → `Ok(())`
    /// - `permission` denied, `Read` also denied → `DomainError::not_found`
    ///   (404, anti-enumeration — same shape as "doesn't exist" so a probing
    ///   caller can't distinguish "wrong id" from "no access")
    /// - `permission` denied, `Read` granted → `DomainError::access_denied`
    ///   (403 — the caller can already see the resource, so hiding existence
    ///   leaks nothing new; a clear 403 beats a confusing 404 for UX and for
    ///   API-first clients like rclone)
    ///
    /// Special case: when `permission == Read`, the visibility gate collapses
    /// onto itself — a `Read` denial IS a "hidden" outcome by definition, so
    /// the method short-circuits to the strict anti-enum 404 without a second
    /// DB round-trip. That's why there's only one method: strict Read-denial
    /// and graduated write-denial fall out of the same signature.
    ///
    /// Do NOT use this in search / enumeration paths where existence itself is
    /// the attack vector — those must filter at the SQL/index layer, never
    /// touch this method with per-row ids. Cross-tenant probes on ids the
    /// caller has no prior read handle for degrade to the 404 shape naturally
    /// (Read denied → `Hidden`).
    ///
    /// [`Self::require_any`] is the multi-credential form and produces its
    /// denial shape by delegating here, so the semantics above are the single
    /// definition for both.
    async fn require(
        &self,
        subject: Subject,
        permission: Permission,
        resource: Resource,
    ) -> Result<(), DomainError> {
        if self.check(subject, permission, resource).await? {
            // Granted path: high-traffic (every authorized request hits
            // this), so kept at `debug` and structured for grep-friendly
            // filtering. Not an audit event — the audit trail focuses
            // on denials and explicit mutations elsewhere.
            tracing::debug!(
                target: "oxicloud::authz",
                event = "authz.allowed",
                subject_type = subject.type_str(),
                subject_id = %subject.id(),
                permission = permission.as_str(),
                resource_type = resource.type_str(),
                resource_id = %resource.id(),
                "👮🏻‍♂️ perms: ✔ Subject '{}' has permission to '{}' on resource '{}'",
                subject,
                permission,
                resource
            );
            return Ok(());
        }

        // Visibility probe. Short-circuit: when the target permission IS
        // `Read` and the check above returned false, we already know Read is
        // denied — visibility is `Hidden` by definition, no second DB hop.
        // Otherwise probe Read; a DB-hop failure here degrades to `Hidden` so
        // the caller sees the strict anti-enum shape (safe default).
        let visibility = if permission == Permission::Read {
            AuthzDenialVisibility::Hidden
        } else if self
            .check(subject, Permission::Read, resource)
            .await
            .unwrap_or(false)
        {
            AuthzDenialVisibility::Visible
        } else {
            AuthzDenialVisibility::Hidden
        };

        let (kind, id) = match resource {
            Resource::Folder(id) => ("Folder", id),
            Resource::File(id) => ("File", id),
            Resource::Drive(id) => ("Drive", id),
            Resource::Calendar(id) => ("Calendar", id),
            Resource::AddressBook(id) => ("AddressBook", id),
            Resource::Playlist(id) => ("Playlist", id),
        };

        // Audit-worthy: denials are the interesting signal. Routed through
        // the `audit` tracing target so log aggregators can surface them
        // separately from operational debug traffic. Span context
        // (request_id, client_ip, user_id) comes from the request-scope
        // span set in `interfaces/middleware/trace_span.rs`, so this line
        // doesn't need to duplicate those fields.
        //
        // The `visibility` field discriminates the two denial shapes for
        // operators grepping exists-but-denied vs fully-hidden. `visible`
        // denials are the ones surfaced to the caller as 403 (and safe to
        // detail in the UI); `hidden` denials are the 404 anti-enum path.
        tracing::info!(
            target: "audit",
            event = "authz.denied",
            visibility = visibility.as_str(),
            subject_type = subject.type_str(),
            subject_id = %subject.id(),
            permission = permission.as_str(),
            resource_type = resource.type_str(),
            resource_id = %resource.id(),
            "👮🏻‍♂️ perms: ⛔ Subject '{}' hasn't permission to '{}' on resource '{}' (visibility={})",
            subject,
            permission,
            resource,
            visibility.as_str()
        );

        match visibility {
            AuthzDenialVisibility::Visible => Err(DomainError::access_denied(
                kind,
                format!("Missing '{}' permission on {} {}", permission, kind, id),
            )),
            AuthzDenialVisibility::Hidden => Err(DomainError::not_found(kind, id.to_string())),
        }
    }

    /// Resources explicitly granted to `subject`. Direct grants only — no
    /// cascade expansion. Used by `GET /api/grants/incoming`.
    async fn list_incoming_grants(&self, subject: Subject) -> Result<Vec<Grant>, DomainError>;

    /// Cursor-paginated list of resources explicitly granted to `subject`,
    /// optionally filtered by resource kind. Multiple permission rows for the
    /// same resource are collapsed into one `IncomingGrantSummary`.
    ///
    /// Ordered by `MIN(granted_at) DESC, resource_id DESC` — stable across
    /// concurrent inserts because the cursor encodes both fields.
    ///
    /// Pass `kinds = &[]` to return all resource kinds.
    /// Returns `(summaries, next_cursor)` — `next_cursor` is `None` when the
    /// last page has been reached.
    async fn list_incoming_resources_paged(
        &self,
        subject: Subject,
        kinds: &[ResourceKind],
        limit: u32,
        cursor: Option<GrantCursor>,
        sort_by: &str,
        reverse: bool,
    ) -> Result<(Vec<IncomingGrantSummary>, Option<GrantCursor>), DomainError>;

    /// All grants on a specific resource (for "Manage sharing" UI). Caller
    /// must verify the caller has `Share` on the resource before invoking.
    async fn list_grants_on_resource(&self, resource: Resource) -> Result<Vec<Grant>, DomainError>;

    /// Grants Outgoing — grants created by `granted_by`. Used by
    /// `GET /api/grants/outgoing` ("things I've shared with others").
    async fn list_outgoing_grants(&self, granted_by: Uuid) -> Result<Vec<Grant>, DomainError>;

    /// Cursor-paginated list of resources that `granted_by` has shared with
    /// others. Multiple permission rows for the same (subject, resource) pair
    /// are collapsed into one `OutgoingGrantEntry`; multiple subjects on the
    /// same resource are grouped into one `OutgoingResourceSummary`.
    ///
    /// Returns `(summaries, next_cursor)`.
    async fn list_outgoing_resources_paged(
        &self,
        granted_by: Uuid,
        limit: u32,
        cursor: Option<GrantCursor>,
        sort_by: &str,
        reverse: bool,
    ) -> Result<(Vec<OutgoingResourceSummary>, Option<GrantCursor>), DomainError>;

    /// Update `expires_at` for every role grant belonging to `subject`.
    /// Used by `share_service` when a token-share's expiry is refreshed —
    /// the subject (token) maps to a small fixed set of role grants, so a
    /// single UPDATE covers them. Resource-scoped expiry changes go through
    /// `set_role` (which carries `expires_at` as part of its UPSERT).
    async fn set_expiry_for_subject(
        &self,
        subject: Subject,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), DomainError>;

    /// Delete every row from `storage.role_grants` whose `expires_at` is
    /// more than `grace_days` in the past. Returns the count of rows
    /// removed.
    ///
    /// The engine's `check` / `list_grants_*` paths already ignore
    /// expired rows (they filter on `expires_at > NOW()` in-query), so
    /// this is pure garbage collection — no live authorization decision
    /// changes. The grace window preserves the audit / support answer
    /// to "what happened to my access?" for a couple of weeks past
    /// expiration.
    ///
    /// Grace of `0` means "delete every row whose `expires_at` is in
    /// the past, right now" — used by the admin `?force=true` trigger
    /// endpoint to enable Hurl regression testing without waiting the
    /// configured grace out.
    ///
    /// Rows with `expires_at IS NULL` (permanent grants) are never
    /// touched.
    async fn purge_expired_grants(&self, grace_days: u32) -> Result<u64, DomainError>;

    /// Revoke a single role grant by its UUID. Idempotent — returns `Ok(())`
    /// whether or not the row existed. The id comes from a prior listing
    /// or `find_grant_full_by_id` lookup.
    async fn revoke(&self, grant_id: Uuid) -> Result<(), DomainError>;

    /// Removes every grant whose `resource` matches. Called by lifecycle
    /// hooks when a resource is permanently deleted. Returns the count of
    /// rows removed.
    async fn revoke_all_for_resource(&self, resource: Resource) -> Result<usize, DomainError>;

    /// Removes every grant whose `subject` matches. Called when a user/token
    /// /group is deleted. Returns the count of rows removed.
    async fn revoke_all_for_subject(&self, subject: Subject) -> Result<usize, DomainError>;

    // ── Role-keyed grant operations ────────────────────────────────────────
    // These are the only grant write path. Lifecycle hook bulk-deletes
    // (`revoke_all_for_*` above) wipe matching rows directly, so callers
    // using those paths don't need to invoke `clear_role` separately.

    /// Set the role for a `(subject, resource)` pair. Idempotent via the
    /// UNIQUE `(subject_type, subject_id, resource_type, resource_id)`
    /// constraint — `ON CONFLICT` updates the role + expires_at if they
    /// changed, which is exactly the right semantics for an atomic role
    /// change (e.g. promoting Viewer → Editor in one UPDATE with no race
    /// window, no DELETE+INSERT).
    async fn set_role(
        &self,
        granted_by: Uuid,
        subject: Subject,
        role: Role,
        resource: Resource,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Grant, DomainError>;

    /// Remove the role for a `(subject, resource)` pair. Idempotent —
    /// succeeds whether or not the row existed. Called after `revoke`
    /// succeeds to keep the two tables in sync during dual-write; after
    /// cleanup this is the canonical role-revocation entry point.
    async fn clear_role(&self, subject: Subject, resource: Resource) -> Result<(), DomainError>;
}

#[cfg(test)]
mod require_any_tests {
    use super::*;
    use crate::domain::errors::ErrorKind;
    use std::sync::Mutex;

    /// Engine stub that grants a fixed set of subjects and records the order
    /// `check` was called in, so short-circuiting is observable.
    struct FakeEngine {
        allowed: Vec<Subject>,
        seen: Mutex<Vec<Subject>>,
    }

    impl FakeEngine {
        fn granting(allowed: Vec<Subject>) -> Self {
            Self {
                allowed,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn checked(&self) -> Vec<Subject> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl AuthorizationEngine for FakeEngine {
        async fn check(
            &self,
            subject: Subject,
            _permission: Permission,
            _resource: Resource,
        ) -> Result<bool, DomainError> {
            self.seen.lock().unwrap().push(subject);
            Ok(self.allowed.contains(&subject))
        }

        // Everything below is outside what these tests exercise.
        async fn list_incoming_grants(&self, _: Subject) -> Result<Vec<Grant>, DomainError> {
            unimplemented!()
        }
        async fn list_incoming_resources_paged(
            &self,
            _: Subject,
            _: &[ResourceKind],
            _: u32,
            _: Option<GrantCursor>,
            _: &str,
            _: bool,
        ) -> Result<(Vec<IncomingGrantSummary>, Option<GrantCursor>), DomainError> {
            unimplemented!()
        }
        async fn list_grants_on_resource(&self, _: Resource) -> Result<Vec<Grant>, DomainError> {
            unimplemented!()
        }
        async fn list_outgoing_grants(&self, _: Uuid) -> Result<Vec<Grant>, DomainError> {
            unimplemented!()
        }
        async fn list_outgoing_resources_paged(
            &self,
            _: Uuid,
            _: u32,
            _: Option<GrantCursor>,
            _: &str,
            _: bool,
        ) -> Result<(Vec<OutgoingResourceSummary>, Option<GrantCursor>), DomainError> {
            unimplemented!()
        }
        async fn set_expiry_for_subject(
            &self,
            _: Subject,
            _: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }
        async fn purge_expired_grants(&self, _: u32) -> Result<u64, DomainError> {
            unimplemented!()
        }
        async fn revoke(&self, _: Uuid) -> Result<(), DomainError> {
            unimplemented!()
        }
        async fn revoke_all_for_resource(&self, _: Resource) -> Result<usize, DomainError> {
            unimplemented!()
        }
        async fn revoke_all_for_subject(&self, _: Subject) -> Result<usize, DomainError> {
            unimplemented!()
        }
        async fn set_role(
            &self,
            _: Uuid,
            _: Subject,
            _: Role,
            _: Resource,
            _: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<Grant, DomainError> {
            unimplemented!()
        }
        async fn clear_role(&self, _: Subject, _: Resource) -> Result<(), DomainError> {
            unimplemented!()
        }
    }

    fn file() -> Resource {
        Resource::File(Uuid::new_v4())
    }

    /// The common case: a logged-in user reading their own file. The ring is
    /// never consulted, so ordinary browsing costs exactly one check.
    #[tokio::test]
    async fn the_first_granting_subject_short_circuits() {
        let alice = Subject::User(Uuid::new_v4());
        let ring = Subject::Token(Uuid::new_v4());
        let engine = FakeEngine::granting(vec![alice]);

        engine
            .require_any(&[alice, ring], Permission::Read, file())
            .await
            .expect("alice's own grant should allow");

        assert_eq!(
            engine.checked(),
            vec![alice],
            "the ring must not be consulted once the user's own grant allows",
        );
    }

    /// Alice opening a colleague's share link: her own grants deny, the ring
    /// allows. This is the case that regressed under every single-credential
    /// design we considered.
    #[tokio::test]
    async fn the_ring_is_consulted_when_the_user_is_denied() {
        let alice = Subject::User(Uuid::new_v4());
        let ring = Subject::Token(Uuid::new_v4());
        let engine = FakeEngine::granting(vec![ring]);

        engine
            .require_any(&[alice, ring], Permission::Read, file())
            .await
            .expect("the unlocked share should allow");

        assert_eq!(engine.checked(), vec![alice, ring]);
    }

    #[tokio::test]
    async fn denial_when_nothing_held_grants() {
        let alice = Subject::User(Uuid::new_v4());
        let ring = Subject::Token(Uuid::new_v4());
        let engine = FakeEngine::granting(vec![]);

        let err = engine
            .require_any(&[alice, ring], Permission::Read, file())
            .await
            .expect_err("no credential grants — must deny");
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    /// Fail closed. "Nothing to check" must never read as "nothing objected"
    /// — unreachable through the extractors, but the wrong default here
    /// would be an authorization bypass rather than a bug.
    #[tokio::test]
    async fn an_empty_subject_set_denies() {
        let engine = FakeEngine::granting(vec![]);
        assert!(
            engine
                .require_any(&[], Permission::Read, file())
                .await
                .is_err(),
            "an empty credential set must deny, not allow",
        );
    }
}

#[cfg(test)]
mod system_admin_tests {
    use super::*;

    #[test]
    fn only_active_internal_admin_users_pass_the_system_gate() {
        let id = Uuid::new_v4();
        assert_eq!(
            system_admin_denial_reason(Subject::User(id), UserRole::Admin, false, true),
            None
        );
        assert_eq!(
            system_admin_denial_reason(Subject::User(id), UserRole::User, false, true),
            Some("not_admin")
        );
        assert_eq!(
            system_admin_denial_reason(Subject::User(id), UserRole::Admin, true, true),
            Some("external_account")
        );
        assert_eq!(
            system_admin_denial_reason(Subject::User(id), UserRole::Admin, false, false),
            Some("inactive")
        );
        assert_eq!(
            system_admin_denial_reason(Subject::Token(id), UserRole::Admin, false, true),
            Some("unsupported_subject")
        );
    }
}
