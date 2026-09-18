-- Restate the external-identity ban as "not a plain user" instead of
-- "not admin".
--
-- `users_external_not_admin` was written when `admin` was the only
-- privileged role, so it spells the rule as an equality:
--
--     CHECK (NOT (is_external AND role = 'admin'))
--
-- The rule it means is "a federated principal may not hold a privileged
-- role on this instance". Any role added ABOVE admin satisfies the old
-- CHECK and walks straight through — an IdP-provisioned account could
-- then hold the most privileged role that exists. The failure is silent:
-- nothing rejects, nothing logs, the row is simply accepted.
--
-- Phrased against `'user'` the constraint stays correct for roles that do
-- not exist yet, which is the point — this must not need revisiting every
-- time the roster grows. `role` is `auth.userrole NOT NULL`, so "not a
-- plain user" is total: there is no third state to leak through.
--
-- No behaviour change today: with the roster at ('admin', 'user'),
-- `role <> 'user'` and `role = 'admin'` are the same predicate over every
-- existing row, so no row can violate the new constraint that did not
-- violate the old one. Deliberately landed BEFORE the roster grows, so
-- the guard is already correct when it does.
--
-- Mirrored at the entity layer by `UserRole::is_privileged()` (see
-- `src/domain/entities/user.rs`), which callers hit first for a typed
-- error instead of an opaque constraint rejection.
--
-- See docs/plan/role-hierarchy-owner.md § What the enum must pay for.

ALTER TABLE auth.users
    DROP CONSTRAINT IF EXISTS users_external_not_admin;

ALTER TABLE auth.users
    ADD CONSTRAINT users_external_not_privileged
        CHECK (NOT (is_external AND role <> 'user'));

COMMENT ON CONSTRAINT users_external_not_privileged ON auth.users IS
    'Federated identities may hold only the unprivileged role. Written as '
    '"not user" rather than "not admin" so it covers roles added later '
    'without being revisited; see UserRole::is_privileged().';
