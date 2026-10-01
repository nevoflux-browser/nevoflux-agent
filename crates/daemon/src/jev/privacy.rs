//! Sensitive sites (spec §5.8, P2 minimal): for these, Jev gets only metadata
//! (domain, title, element count) — never page text or tool results.

use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Full,
    MetadataOnly,
}

/// Banking, payments, medical, mail and government services; a domain covers
/// its subdomains. Users add more in `[jev] sensitive_domains`.
pub const BUILTIN_SENSITIVE_DOMAINS: &[&str] = &[
    // payments
    "paypal.com",
    "stripe.com",
    "alipay.com",
    "wise.com",
    "venmo.com",
    "pay.weixin.qq.com",
    // banking
    "chase.com",
    "citi.com",
    "bankofamerica.com",
    "wellsfargo.com",
    "hsbc.com",
    "barclays.co.uk",
    "icbc.com.cn",
    "ccb.com",
    "boc.cn",
    "abchina.com",
    "cmbchina.com",
    // mail
    "mail.google.com",
    "outlook.live.com",
    "outlook.office.com",
    "mail.yahoo.com",
    "mail.qq.com",
    "mail.163.com",
    "proton.me",
    "icloud.com",
    // medical
    "mychart.com",
    "patient.info",
    "healthcare.gov",
    // government
    "irs.gov",
    "ssa.gov",
    "gov.uk",
    "gov.cn",
    "usa.gov",
];

/// Suffixes that only resolve inside a private network.
const INTRANET_SUFFIXES: &[&str] = &["local", "internal", "lan", "corp", "home.arpa", "intranet"];

/// What Jev may see of a page at `url`. Unparseable URLs are treated as
/// sensitive: when unsure, send nothing private.
pub fn scope_for(url: &str, extra_domains: &[String]) -> Scope {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Scope::MetadataOnly;
    };
    let Some(host) = parsed.host_str() else {
        return Scope::MetadataOnly;
    };
    // `www.paypal.com.` is the same host as `www.paypal.com`.
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.parse::<IpAddr>().is_ok() {
        return if is_private_ip(&host) {
            Scope::MetadataOnly
        } else {
            Scope::Full
        };
    }
    // `localhost`, single-label intranet names (`http://jira/`) and
    // private-network suffixes.
    if !host.contains('.') || has_suffix(&host, "localhost") {
        return Scope::MetadataOnly;
    }
    if INTRANET_SUFFIXES.iter().any(|d| has_suffix(&host, d)) {
        return Scope::MetadataOnly;
    }
    let listed = BUILTIN_SENSITIVE_DOMAINS
        .iter()
        .map(|d| d.to_string())
        .chain(extra_domains.iter().map(|d| normalise_domain(d)))
        .any(|d| !d.is_empty() && has_suffix(&host, &d));
    if listed {
        Scope::MetadataOnly
    } else {
        Scope::Full
    }
}

/// `host` is `domain` or one of its subdomains.
fn has_suffix(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// A user-typed domain entry as a bare host: `*.acme.com`, `.acme.com`,
/// `acme.com.`, `https://acme.com/` and ` ACME.com ` all mean `acme.com`.
fn normalise_domain(entry: &str) -> String {
    let mut d = entry.trim().to_ascii_lowercase();
    if let Some(i) = d.find("://") {
        d = d[i + 3..].to_string();
    }
    if let Some(i) = d.find('/') {
        d.truncate(i);
    }
    d.trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_string()
}

fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        // CGNAT 100.64.0.0/10 (Tailscale and carrier-grade NAT).
        || (a == 100 && (64..128).contains(&b))
}

fn is_private_ip(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => is_private_v4(v4),
        Ok(IpAddr::V6(v6)) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_sites_and_their_subdomains_send_metadata_only() {
        for url in [
            "https://www.paypal.com/myaccount",
            "https://mail.google.com/mail/u/0/",
            "https://online.citi.com/",
            "https://user@login.chase.com:443/x",
            "https://www.irs.gov/",
            "https://my.gov.uk/",
            "http://localhost:3000/",
            "http://127.0.0.1/",
            "http://10.1.2.3/admin",
            "http://192.168.1.1/",
            "http://172.20.0.5/",
            "http://[::1]/",
            "http://[fd00::1]/",
        ] {
            assert_eq!(scope_for(url, &[]), Scope::MetadataOnly, "{url}");
        }
    }

    #[test]
    fn host_spellings_that_reach_the_same_place_are_still_sensitive() {
        for url in [
            "https://www.paypal.com./",
            "http://[::ffff:192.168.1.1]/",
            "http://0.0.0.0:8080/",
            "http://[::]/",
            "http://100.101.102.103/",
            "http://jira/browse/X-1",
            "http://printer.local/",
            "https://git.corp.internal/",
            "http://nas.lan/",
            "http://wiki.corp/",
            "http://router.home.arpa/",
        ] {
            assert_eq!(scope_for(url, &[]), Scope::MetadataOnly, "{url}");
        }
    }

    #[test]
    fn user_domains_are_normalised() {
        for entry in [
            "*.acme.com",
            ".acme.com",
            "acme.com.",
            "https://acme.com/",
            " ACME.com ",
        ] {
            assert_eq!(
                scope_for("https://wiki.acme.com/x", &[entry.to_string()]),
                Scope::MetadataOnly,
                "{entry}"
            );
        }
        assert_eq!(
            scope_for("https://notacme.com/", &["acme.com".into()]),
            Scope::Full
        );
    }

    #[test]
    fn ordinary_and_lookalike_sites_are_full() {
        for url in [
            "https://en.wikipedia.org/wiki/Rust",
            "https://notpaypal.com/",
            "https://paypal.com.evil.example/",
            "https://172.32.0.1/",
        ] {
            assert_eq!(scope_for(url, &[]), Scope::Full, "{url}");
        }
    }

    #[test]
    fn user_domains_extend_the_list_and_bad_urls_are_safe() {
        assert_eq!(
            scope_for("https://intranet.acme.example/", &["acme.example".into()]),
            Scope::MetadataOnly
        );
        assert_eq!(
            scope_for("not a url", &[]),
            Scope::MetadataOnly,
            "unknown → do not send content"
        );
    }
}
