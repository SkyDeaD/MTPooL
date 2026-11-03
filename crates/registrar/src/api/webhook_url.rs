//! The rule for webhook URLs, and the name resolver it uses.
//!
//! A URL is accepted when it is `https`, names a host (not an address) that resolves, and
//! every address it resolves to is public. The check runs when a webhook is registered. A
//! name can resolve to something else later, so the sender must resolve the name again at
//! delivery and apply [`is_public_ip`] to the addresses it is about to connect to.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;

/// Longest URL a webhook may have.
pub const MAX_URL_LEN: usize = 2048;

pub type ResolveFuture<'a> = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// Turns a host name into addresses. The system resolver in the service, a table in tests.
pub trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a>;
}

/// The operating system's resolver.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host, port)).await?;
            Ok(addresses.map(|a| a.ip()).collect())
        })
    }
}

/// Why a URL was refused. The text is safe to show to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlRefused(pub &'static str);

impl std::fmt::Display for UrlRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Whether `ip` is an address a webhook may be sent to: not private, loopback, link-local,
/// shared (100.64.0.0/10), unspecified, multicast, reserved, or an IPv6 address that is not
/// global unicast. IPv4 addresses inside IPv6 ones are judged as the IPv4 address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    // NAT64 (64:ff9b::/96) carries an IPv4 address in the last 32 bits.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    // 6to4 (2002::/16) carries one right after the prefix.
    if s[0] == 0x2002 {
        return is_public_v4(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    let global_unicast = s[0] & 0xe000 == 0x2000;
    let documentation = s[0] == 0x2001 && s[1] == 0x0db8;
    let teredo = s[0] == 0x2001 && s[1] == 0;
    global_unicast && !documentation && !teredo
}

/// A name of at least two labels of ASCII letters, digits and hyphens (no leading or trailing
/// hyphen, at most 63 characters a label, 253 in all).
pub fn is_dns_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The host and port of an `https` URL, when its shape is acceptable. Credentials, an address
/// as the host (IPv4 in any notation, IPv6), a host without a dot and non-ASCII names are not.
fn host_and_port(url: &str) -> Result<(String, u16), UrlRefused> {
    if url.len() > MAX_URL_LEN {
        return Err(UrlRefused("The URL is longer than 2048 characters."));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(UrlRefused("The URL has spaces or control characters."));
    }
    let scheme_len = "https://".len();
    if !url
        .get(..scheme_len)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    {
        return Err(UrlRefused("The URL must start with https://."));
    }
    let rest = &url[scheme_len..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(UrlRefused("The URL must not carry credentials."));
    }
    if authority.starts_with('[') {
        return Err(UrlRefused("The host must be a name, not an address."));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or(UrlRefused("The port of the URL is not valid."))?;
            (host, port)
        }
        None => (authority, 443),
    };
    let host = host.to_ascii_lowercase();
    if !is_dns_name(&host) {
        return Err(UrlRefused("The host must be a name with a domain."));
    }
    let labels: Vec<&str> = host.split('.').collect();
    // 127.1, 0x7f.1 and 2130706433 are addresses to a resolver; real top-level domains have a
    // letter in them.
    let last = labels[labels.len() - 1];
    if !last.bytes().any(|b| b.is_ascii_alphabetic()) || last.starts_with("0x") {
        return Err(UrlRefused("The host must be a name, not an address."));
    }
    Ok((host, port))
}

/// Checks the URL and the addresses its host resolves to.
pub async fn check(resolver: &dyn Resolver, url: &str) -> Result<(), UrlRefused> {
    let (host, port) = host_and_port(url)?;
    let addresses = resolver
        .resolve(&host, port)
        .await
        .map_err(|_| UrlRefused("The host name does not resolve."))?;
    if addresses.is_empty() {
        return Err(UrlRefused("The host name does not resolve."));
    }
    if addresses.iter().copied().any(|ip| !is_public_ip(ip)) {
        return Err(UrlRefused(
            "The host name resolves to an address that is not public.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<IpAddr>);

    impl Resolver for Fixed {
        fn resolve<'a>(&'a self, _host: &'a str, _port: u16) -> ResolveFuture<'a> {
            let out = self.0.clone();
            Box::pin(async move { Ok(out) })
        }
    }

    struct Nothing;

    impl Resolver for Nothing {
        fn resolve<'a>(&'a self, _host: &'a str, _port: u16) -> ResolveFuture<'a> {
            Box::pin(async { Err(io::Error::other("no such name")) })
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn address_classes() {
        for public in [
            "93.184.216.34",
            "8.8.8.8",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public_ip(ip(public)), "{public}");
        }
        for private in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "198.18.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2002:7f00:1::",
        ] {
            assert!(!is_public_ip(ip(private)), "{private}");
        }
        // Just outside the shared range.
        assert!(is_public_ip(ip("100.63.255.255")));
        assert!(is_public_ip(ip("100.128.0.1")));
        assert!(is_public_ip(ip("172.32.0.1")));
    }

    #[test]
    fn url_shape() {
        assert_eq!(
            host_and_port("https://Hooks.Example.com/path?x=1#f"),
            Ok(("hooks.example.com".to_owned(), 443))
        );
        assert_eq!(
            host_and_port("HTTPS://hooks.example.com:8443"),
            Ok(("hooks.example.com".to_owned(), 8443))
        );
        for bad in [
            "http://hooks.example.com/",
            "https://",
            "https:///path",
            "https://localhost/",
            "https://127.0.0.1/",
            "https://127.1/",
            "https://2130706433/",
            "https://0x7f.0.0.1/",
            "https://[::1]/",
            "https://user:pass@hooks.example.com/",
            "https://hooks.example.com:0/",
            "https://hooks.example.com:99999/",
            "https://hooks.example.com:abc/",
            "https://hooks .example.com/",
            "https://hooks.example.com./",
            "https://-bad.example.com/",
            "https://exämple.com/",
            "ftp://hooks.example.com/",
            "hooks.example.com",
            "",
        ] {
            assert!(host_and_port(bad).is_err(), "{bad:?}");
        }
        let long = format!("https://hooks.example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(host_and_port(&long).is_err());
    }

    #[tokio::test]
    async fn resolved_addresses_decide() {
        let url = "https://hooks.example.com/hook";
        assert_eq!(check(&Fixed(vec![ip("93.184.216.34")]), url).await, Ok(()));
        assert_eq!(
            check(
                &Fixed(vec![ip("93.184.216.34"), ip("2606:4700::1111")]),
                url
            )
            .await,
            Ok(())
        );
        // One bad address among good ones is enough to refuse.
        assert!(
            check(&Fixed(vec![ip("93.184.216.34"), ip("192.168.1.1")]), url)
                .await
                .is_err()
        );
        assert!(check(&Fixed(vec![]), url).await.is_err());
        assert!(check(&Nothing, url).await.is_err());
        assert!(
            check(&Fixed(vec![ip("10.0.0.1")]), "http://hooks.example.com/")
                .await
                .is_err()
        );
    }
}
