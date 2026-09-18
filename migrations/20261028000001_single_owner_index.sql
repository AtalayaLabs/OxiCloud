-- At most one server owner.
--
-- Separate file from the ADD VALUE that introduces 'owner': this
-- predicate USES the new enum value, which PostgreSQL forbids in the
-- transaction that added it. See 20261028000000_userrole_add_owner.sql.
--
-- A partial unique index on a constant expression is the cheapest way to
-- say "at most one row satisfies this predicate": every owner row indexes
-- the same key, so the second one collides. Cheaper than a
-- CHECK-with-subquery (not allowed) or a trigger (a race unless
-- serialized), and it composes with plain INSERT/UPDATE.
--
-- THIS CONSTRAINT IS POLICY, NOT STRUCTURE. Single-owner is today's rule,
-- not an assumption the rest of the design leans on. Multiple owners —
-- wanted eventually for bus factor, since a sole owner who leaves takes
-- ownership with them — is then `DROP INDEX`: no data migration, no
-- re-modelling, because ownership already lives on the user row.
--
-- One question to settle before that day, deliberately not answered here:
-- whether an owner may demote another owner. The strict rule
-- (caller.rank > target.rank) says no, which protects owners from each
-- other but leaves a departed co-owner removable only via the CLI.
-- Relaxing it lets any owner unilaterally strip the others. With exactly
-- one owner the question cannot arise. See
-- docs/plan/role-hierarchy-owner.md § Multi-owner.
--
-- Ownership transfer must therefore demote-then-promote within one
-- transaction: promoting first would momentarily hold two owners and trip
-- this index.

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_single_owner
    ON auth.users ((role = 'owner'))
    WHERE role = 'owner';

COMMENT ON INDEX auth.idx_users_single_owner IS
    'At most one auth.users row may hold role = owner. Policy, not '
    'structure: DROP this index to allow multiple owners — ownership '
    'lives on the user row either way.';
