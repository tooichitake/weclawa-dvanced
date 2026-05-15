//! Shared SSRF guard utilities — extracted from `media::outbound` so
//! `monitor::webhook` (v2.1.A4) can reuse the exact same DNS resolution
//! + IP class check. Any outbound HTTP path from daemon to a user-
//! supplied URL must go through `resolve_and_check_public` before
//! firing the request.

use std::net::IpAddr;

/// DNS-resolve `host` and verify EVERY returned IP is a publicly routable
/// address — refuses if any resolves to a loopback / private / link-local /
/// unspecified IP. Returning `Ok(true)` only when every candidate is safe;
/// `Ok(false)` if any is dangerous; `Err(_)` on DNS failure.
///
/// We check ALL resolved IPs (not just the first) because an attacker
/// might control a DNS record with multiple A records, one public + one
/// pointing at 127.0.0.1; reqwest could pick either.
pub async fn resolve_and_check_public(host: &str, port: u16) -> Result<bool, String> {
    // Fast-path: host is already an IP literal.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(is_public_ip(&ip));
    }

    // DNS resolve via tokio. ToSocketAddrs blocks; spawn it on a blocking
    // worker so we don't stall the runtime.
    let target = format!("{host}:{port}");
    let lookup =
        tokio::task::spawn_blocking(move || std::net::ToSocketAddrs::to_socket_addrs(&target))
            .await
            .map_err(|e| format!("dns join: {e}"))?
            .map_err(|e| format!("dns resolve {host}: {e}"))?;

    let mut found_any = false;
    for addr in lookup {
        found_any = true;
        if !is_public_ip(&addr.ip()) {
            return Ok(false);
        }
    }
    if !found_any {
        return Err(format!("dns resolve {host}: no records"));
    }
    Ok(true)
}

/// Classify an IP as publicly routable. Refuses loopback, unspecified,
/// multicast, IPv4 private (RFC 1918), link-local (incl. cloud metadata
/// 169.254.169.254), CGNAT 100.64/10, IPv4 broadcast, IPv6 ULA fc00::/7,
/// IPv6 link-local fe80::/10.
pub fn is_public_ip(ip: &IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_private() || v4.is_link_local() || v4.is_broadcast() {
                return false;
            }
            // Cloud metadata: 169.254.169.254 covered by link_local but
            // also explicit blocklist for the magic /32:
            if v4.octets() == [169, 254, 169, 254] {
                return false;
            }
            // 100.64.0.0/10 CGNAT (RFC 6598) — used in some cloud private nets
            let o = v4.octets();
            if o[0] == 100 && (o[1] & 0xc0) == 64 {
                return false;
            }
            true
        }
        IpAddr::V6(v6) => {
            if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                return false;
            }
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return false;
            }
            true
        }
    }
}

/// 给日志用的 URL 脱敏：保留 scheme + host，丢 path + query (常含 token)。
/// 灵感来自 `BotToken::Display` 的 redact 风格。
pub fn mask_url_for_log(url: &str) -> String {
    if let Ok(u) = url::Url::parse(url) {
        let scheme = u.scheme();
        let host = u.host_str().unwrap_or("?");
        let port = u
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        let has_query = u.query().map(|q| !q.is_empty()).unwrap_or(false);
        let qmark = if has_query { "?<redacted>" } else { "" };
        format!("{scheme}://{host}{port}/<path>{qmark}")
    } else {
        "<unparseable-url>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn loopback_not_public() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn private_not_public() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn cloud_metadata_not_public() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
    }

    #[test]
    fn cgnat_not_public() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 127, 255, 254))));
    }

    #[test]
    fn public_v4_is_public() {
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn mask_url_keeps_scheme_host_drops_path_query() {
        assert_eq!(
            mask_url_for_log("https://hook.example.com/abc/secret?token=xyz"),
            "https://hook.example.com/<path>?<redacted>"
        );
        assert_eq!(
            mask_url_for_log("http://localhost:8080/x"),
            "http://localhost:8080/<path>"
        );
    }

    #[tokio::test]
    async fn ip_literal_loopback_rejected_fast() {
        assert!(!resolve_and_check_public("127.0.0.1", 80).await.unwrap());
        assert!(!resolve_and_check_public("169.254.169.254", 80).await.unwrap());
    }
}
