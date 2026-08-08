-- Identity-lookup email column + b-tree index. Powers
-- `list_users_by_normalized_email`, the auto-link ambiguity detector
-- added alongside the OIDC-linking work
-- (see docs/plan/oidc-account-linking.md § Auto-link).
--
-- Normalization matches `common::text::normalize_email_for_link`:
-- lowercase + strip `+alias` sub-addressing from the local part.
-- Unconditional storage: every row carries the normalized form, even
-- when it equals `email`. The alternative (only populate when the
-- normalized form differs, then `WHERE email = $1 OR
-- identity_lookup_email = $1`) saves a few bytes per row but forces
-- a two-branch lookup on every call. Storage cost is minimal
-- (~30 bytes/user); simplicity of the always-populated lookup wins.
--
-- Storing the normalized form as its own column (rather than doing
-- the computation in the WHERE clause) buys three things:
--   1. Plain-equality SQL — the lookup is a one-liner, not a
--      SPLIT_PART/LOWER expression tree that's easy to mis-copy.
--   2. Debuggable — operators can `SELECT username, email,
--      identity_lookup_email FROM auth.users` and immediately see
--      why two users collide under normalization.
--   3. Automatic — GENERATED ALWAYS AS ... STORED means PostgreSQL
--      itself keeps the column in sync on every INSERT/UPDATE of
--      `email`. No trigger, no application code, no drift risk.
--
-- Table rewrite cost: ALTER TABLE ADD COLUMN with a GENERATED
-- expression forces a full-table rewrite (each row needs the
-- computed value stored). Brief, exclusive lock. Fine at OxiCloud's
-- expected sizes (self-hosted, hundreds to tens of thousands of
-- users); would need a batched backfill on a million-row deployment.
ALTER TABLE auth.users
ADD COLUMN identity_lookup_email TEXT
GENERATED ALWAYS AS (
    LOWER(
        SPLIT_PART(SPLIT_PART(email, '@', 1), '+', 1)
        || '@'
        || SPLIT_PART(email, '@', 2)
    )
) STORED;

-- Regular b-tree index on the stored column. O(log n) probes for the
-- auto-link ambiguity check on the OIDC callback path.
CREATE INDEX idx_users_identity_lookup_email
    ON auth.users(identity_lookup_email);
