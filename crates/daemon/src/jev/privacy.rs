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

/// What Jev may see of a page at `url`. Unparseable URLs are treated as
/// sensitive: when unsure, send nothing private.
pub fn scope_for(url: &str, extra_domains: &[String]) -> Scope {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Scope::MetadataOnly;
    };
    let Some(host) = parsed.host_str() else {
        return Scope::MetadataOnly;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || is_private_ip(&host) {
        return Scope::MetadataOnly;
    }
    let listed = BUILTIN_SENSITIVE_DOMAINS
        .iter()
        .map(|d| d.to_string())
        .chain(extra_domains.iter().map(|d| d.trim().to_ascii_lowercase()))
        .any(|d| !d.is_empty() && (host == d || host.ends_with(&format!(".{d}"))));
    if listed {
        Scope::MetadataOnly
    } else {
        Scope::Full
    }
}

fn is_private_ip(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback()
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
