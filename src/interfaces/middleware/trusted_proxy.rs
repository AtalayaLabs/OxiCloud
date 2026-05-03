//! Trusted-proxy CIDR list and client IP resolution.
//!
//! Set `OXICLOUD_TRUST_PROXY_CIDR` to a comma-separated list of CIDR blocks
//! whose source IPs are trusted to set `X-Forwarded-For` / `X-Real-Ip`:
//!
//! ```text
//! OXICLOUD_TRUST_PROXY_CIDR=127.0.0.1/32,10.0.0.0/8,172.16.0.0/12,::1/128
//! ```
//!
//! When the TCP peer address falls inside one of those CIDRs the leftmost
//! entry of `X-Forwarded-For` (or `X-Real-Ip`) is used as the effective
//! client IP.  When the peer is **not** trusted, or no proxy headers are
//! present, the raw TCP peer address is returned.
//!
//! IPv4-mapped IPv6 addresses (`::ffff:x.x.x.x`) are automatically
//! normalised to their IPv4 equivalent before CIDR lookup.

use axum::extract::ConnectInfo;
use axum::http::Request;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;

// ─── CIDR list ───────────────────────────────────────────────────────────────

static TRUSTED_CIDRS: OnceLock<Vec<(IpAddr, u8)>> = OnceLock::new();

fn trusted_cidrs() -> &'static [(IpAddr, u8)] {
    TRUSTED_CIDRS.get_or_init(|| {
        let mut cidrs: Vec<(IpAddr, u8)> = std::env::var("OXICLOUD_TRUST_PROXY_CIDR")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| parse_cidr(s.trim()))
            .collect();

        // Backward-compat: OXICLOUD_TRUST_PROXY_HEADERS=true trusts all source IPs.
        let legacy = std::env::var("OXICLOUD_TRUST_PROXY_HEADERS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        if legacy {
            tracing::warn!(
                "OXICLOUD_TRUST_PROXY_HEADERS is deprecated — \
                 use OXICLOUD_TRUST_PROXY_CIDR to restrict trusted proxy source IPs. \
                 Falling back to trust all IPs (0.0.0.0/0 and ::/0)."
            );
            cidrs.push(("0.0.0.0".parse().unwrap(), 0));
            cidrs.push(("::".parse().unwrap(), 0));
        }

        cidrs
    })
}

fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (addr_s, prefix_s) = s.split_once('/')?;
    let addr: IpAddr = addr_s.parse().ok()?;
    let prefix: u8 = prefix_s.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    if prefix > max {
        return None;
    }
    Some((addr, prefix))
}

// ─── CIDR matching ───────────────────────────────────────────────────────────

fn ipv4_in_cidr(base: Ipv4Addr, prefix: u8, ip: Ipv4Addr) -> bool {
    if prefix == 0 {
        return true;
    }
    let shift = 32 - u32::from(prefix);
    let mask = !0u32 << shift;
    (u32::from(base) & mask) == (u32::from(ip) & mask)
}

fn ipv6_in_cidr(base: Ipv6Addr, prefix: u8, ip: Ipv6Addr) -> bool {
    if prefix == 0 {
        return true;
    }
    let shift = 128 - u32::from(prefix);
    let mask = !0u128 << shift;
    (u128::from(base) & mask) == (u128::from(ip) & mask)
}

/// Normalise an IP to IPv4 if it is an IPv4-mapped IPv6 address.
fn normalise(ip: IpAddr) -> IpAddr {
    if let IpAddr::V6(v6) = ip
        && let Some(v4) = v6.to_ipv4_mapped()
    {
        return IpAddr::V4(v4);
    }
    ip
}

fn cidr_contains(base: IpAddr, prefix: u8, target: IpAddr) -> bool {
    let base = normalise(base);
    let target = normalise(target);
    match (base, target) {
        (IpAddr::V4(b), IpAddr::V4(t)) => ipv4_in_cidr(b, prefix, t),
        (IpAddr::V6(b), IpAddr::V6(t)) => ipv6_in_cidr(b, prefix, t),
        _ => false,
    }
}

/// Logs the loaded CIDR list at INFO level.  Call once at startup, after the
/// tracing subscriber is initialised, to make the effective configuration
/// visible in the logs.
pub fn log_config() {
    let cidrs = trusted_cidrs();
    if cidrs.is_empty() {
        tracing::info!(
            "trusted proxy CIDRs: none — X-Forwarded-For/X-Real-Ip headers will be ignored"
        );
    } else {
        let list: Vec<String> = cidrs
            .iter()
            .map(|(addr, prefix)| format!("{addr}/{prefix}"))
            .collect();
        tracing::info!("trusted proxy CIDRs: {}", list.join(", "));
    }
}

/// Returns `true` when `peer` falls inside one of the configured CIDR ranges.
pub fn is_trusted_proxy(peer: IpAddr) -> bool {
    let cidrs = trusted_cidrs();
    if cidrs.is_empty() {
        return false;
    }
    cidrs
        .iter()
        .any(|&(base, prefix)| cidr_contains(base, prefix, peer))
}

// ─── IP extraction ───────────────────────────────────────────────────────────

/// Resolve the effective client IP for a request.
///
/// * `include_port` — when `true` the returned string for direct (non-proxied)
///   connections includes the port, e.g. `"127.0.0.1:12345"`.  Pass `false`
///   for contexts that only need the bare IP (e.g. rate-limiting keys).
pub fn client_ip<B>(req: &Request<B>, include_port: bool) -> String {
    let peer: Option<SocketAddr> = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);

    if let Some(peer_addr) = peer {
        if is_trusted_proxy(peer_addr.ip()) {
            // Try X-Forwarded-For first (leftmost = original client)
            if let Some(xff) = req
                .headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                && let Some(ip) = xff
                    .split(',')
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                return ip.to_string();
            }

            // Then X-Real-Ip
            if let Some(xri) = req
                .headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return xri.to_string();
            }
        }

        return if include_port {
            peer_addr.to_string()
        } else {
            peer_addr.ip().to_string()
        };
    }

    "unknown".to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_cidr_host() {
        let base: Ipv4Addr = "192.168.1.1".parse().unwrap();
        assert!(ipv4_in_cidr(base, 32, "192.168.1.1".parse().unwrap()));
        assert!(!ipv4_in_cidr(base, 32, "192.168.1.2".parse().unwrap()));
    }

    #[test]
    fn ipv4_cidr_subnet() {
        let base: Ipv4Addr = "10.0.0.0".parse().unwrap();
        assert!(ipv4_in_cidr(base, 8, "10.255.255.255".parse().unwrap()));
        assert!(!ipv4_in_cidr(base, 8, "11.0.0.1".parse().unwrap()));
    }

    #[test]
    fn ipv4_cidr_any() {
        let base: Ipv4Addr = "0.0.0.0".parse().unwrap();
        assert!(ipv4_in_cidr(base, 0, "1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn ipv6_cidr_loopback() {
        let base: Ipv6Addr = "::1".parse().unwrap();
        assert!(ipv6_in_cidr(base, 128, "::1".parse().unwrap()));
        assert!(!ipv6_in_cidr(base, 128, "::2".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_matches_v4_cidr() {
        // ::ffff:127.0.0.1 should match 127.0.0.1/8
        let base = IpAddr::V4("127.0.0.0".parse().unwrap());
        let mapped = IpAddr::V6("::ffff:127.0.0.1".parse().unwrap());
        assert!(cidr_contains(base, 8, mapped));
    }

    #[test]
    fn parse_cidr_valid() {
        assert_eq!(
            parse_cidr("10.0.0.0/8"),
            Some((IpAddr::V4("10.0.0.0".parse().unwrap()), 8))
        );
        assert_eq!(
            parse_cidr("::1/128"),
            Some((IpAddr::V6("::1".parse().unwrap()), 128))
        );
    }

    #[test]
    fn parse_cidr_invalid_prefix() {
        assert!(parse_cidr("10.0.0.0/33").is_none());
        assert!(parse_cidr("::1/129").is_none());
        assert!(parse_cidr("notanip/8").is_none());
    }
}
