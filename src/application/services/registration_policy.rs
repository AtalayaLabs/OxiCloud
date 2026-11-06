//! Who may become an internal account on this deployment.
//!
//! Two callers, one rule — deliberately:
//!
//!   * `POST /api/auth/register` — a stranger signing up.
//!   * `POST /api/auth/upgrade-to-internal` — an invited guest
//!     promoting themselves, which is self-registration under another
//!     name: it ends with a full account, a personal drive and a quota.
//!
//! They were copies once, and copies drift. A subdomain rule or a
//! normalisation fix applied to signup but not to promotion would
//! reopen the hole the upgrade gate exists to close — an invitation
//! becoming a way around the operator's policy. This module is the
//! single definition; `auth_handler` wraps it with the audit line and
//! the HTTP error, and `build_self_user_dto_for_id` asks it the same
//! question to tell the SPA whether to offer the upgrade at all.

/// Does `email`'s domain satisfy `OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS`?
///
/// An EMPTY allowlist means no restriction — the operator has not
/// limited signup by domain.
///
/// Matching, mirroring the magic-link allowlist:
///   * post-`@` part extracted, trimmed, lowercased
///   * exact match (entries are lowercased at config load, so the
///     comparison is genuinely case-insensitive)
///   * NO wildcard or subdomain expansion — `evil.example.com` is a
///     domain an attacker may control, so a suffix match would hand
///     them signup on a deployment that allowed `example.com`. List
///     every domain explicitly, per the config docstring.
///   * an address with no `@`, or nothing after it, yields an empty
///     domain and is refused — unparseable is a refusal, not a pass.
pub fn domain_allowed(allow_list: &[String], email: &str) -> bool {
    if allow_list.is_empty() {
        return true;
    }
    let domain = email
        .split('@')
        .nth(1)
        .map(|d| d.trim().to_ascii_lowercase())
        .unwrap_or_default();
    !domain.is_empty() && allow_list.iter().any(|d| d == &domain)
}

/// Would `POST /api/auth/upgrade-to-internal` succeed for this caller?
///
/// Surfaced to the SPA as `SelfUserDto::is_upgradable` so it can hide
/// the "Get your own storage" banner rather than offer a button that
/// answers 403. The four terms mirror the endpoint's own guards, in the
/// same order — keep them in step: a guard added there without a term
/// here turns the banner into a dead end, and a term here without a
/// guard there hides an upgrade that would have worked.
///
///   * `is_external` — internal accounts have nothing to upgrade.
///   * `!is_oidc_user` — IdP-managed; the endpoint answers
///     `ManagedByIdP`.
///   * `registration_open` — self-promotion IS self-registration, so
///     `OXICLOUD_DISABLE_REGISTRATION` closes it too.
///   * [`domain_allowed`] — the operator's domain allowlist.
///
/// This is the only place the conjunction lives. Computing it inline in
/// the DTO builder made it reachable only through a DB fixture, which
/// is why the term order above went untested for as long as it did.
pub fn upgrade_offer_allowed(
    is_external: bool,
    is_oidc_user: bool,
    registration_open: bool,
    allow_list: &[String],
    email: &str,
) -> bool {
    is_external && !is_oidc_user && registration_open && domain_allowed(allow_list, email)
}

#[cfg(test)]
mod tests {
    use super::{domain_allowed, upgrade_offer_allowed};

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_allowlist_accepts_anything() {
        // Unset env = no restriction. Callers rely on this to skip work
        // entirely when no policy is configured.
        assert!(domain_allowed(&list(&[]), "anyone@wherever.test"));
        assert!(domain_allowed(&list(&[]), "no-at-sign"));
    }

    #[test]
    fn exact_domain_matches() {
        assert!(domain_allowed(&list(&["example.com"]), "bob@example.com"));
        assert!(!domain_allowed(&list(&["example.com"]), "bob@other.com"));
    }

    #[test]
    fn matching_is_case_insensitive_on_the_address() {
        // Config entries are lowercased at load (`config.rs`), so only
        // the address side needs folding here.
        assert!(domain_allowed(&list(&["example.com"]), "Bob@EXAMPLE.CoM"));
    }

    #[test]
    fn subdomains_are_not_expanded() {
        // ⚠ The security-relevant case: a suffix match would accept a
        // domain the attacker controls.
        assert!(!domain_allowed(
            &list(&["example.com"]),
            "mallory@evil.example.com"
        ));
        assert!(!domain_allowed(
            &list(&["example.com"]),
            "mallory@notexample.com"
        ));
    }

    #[test]
    fn an_address_without_a_usable_domain_is_refused() {
        assert!(!domain_allowed(&list(&["example.com"]), "no-at-sign"));
        assert!(!domain_allowed(&list(&["example.com"]), "trailing@"));
    }

    #[test]
    fn whitespace_around_the_domain_is_trimmed() {
        assert!(domain_allowed(&list(&["example.com"]), "bob@ example.com "));
    }

    #[test]
    fn one_of_several_allowed_domains_matches() {
        let l = list(&["example.com", "example.test"]);
        assert!(domain_allowed(&l, "bob@example.test"));
        assert!(!domain_allowed(&l, "bob@example.org"));
    }

    #[test]
    fn the_upgrade_offer_needs_all_four_terms() {
        let l = list(&["example.com"]);
        // The one case that offers the upgrade.
        assert!(upgrade_offer_allowed(
            true,
            false,
            true,
            &l,
            "g@example.com"
        ));
        // Each term, flipped alone, withdraws it.
        assert!(
            !upgrade_offer_allowed(false, false, true, &l, "g@example.com"),
            "an internal account has nothing to upgrade"
        );
        assert!(
            !upgrade_offer_allowed(true, true, true, &l, "g@example.com"),
            "an IdP-managed account is refused with ManagedByIdP"
        );
        assert!(
            !upgrade_offer_allowed(true, false, false, &l, "g@example.com"),
            "registration closed closes self-promotion with it"
        );
        assert!(
            !upgrade_offer_allowed(true, false, true, &l, "g@elsewhere.com"),
            "the domain allowlist applies to promotion, not just signup"
        );
    }

    #[test]
    fn an_unrestricted_deployment_offers_the_upgrade_to_any_guest() {
        // The common case: no allowlist configured, registration open.
        assert!(upgrade_offer_allowed(
            true,
            false,
            true,
            &list(&[]),
            "guest@anywhere.test"
        ));
    }
}
