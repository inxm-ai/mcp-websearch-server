//! Which URLs and addresses this server may request: the same rules as the
//! agent hub's `fetch_url`. Only public http(s) hosts; no private, loopback,
//! link-local (cloud metadata), CGNAT, cluster or otherwise reserved
//! addresses; no credentials in the URL.
//!
//! The fetch adapters apply [`check_url`] before every request and on every
//! redirect, and [`is_public_ip`] to every address a hostname resolves to.

// `core::net` holds address *values* only; no I/O is reachable from it.
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Host suffixes that only name machines inside a private network.
const PRIVATE_NAME_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".cluster.local"];
const LOCALHOST: &str = "localhost";

/// Why a URL is not requested.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("unsupported scheme `{0}`")]
    UnsupportedScheme(String),
    #[error("credentials in URL")]
    Credentials,
    #[error("no host")]
    MissingHost,
    #[error("private address")]
    PrivateAddress,
    #[error("private host name")]
    PrivateHostName,
    #[error("host does not resolve to a public address")]
    NoPublicAddress,
    #[error("more than {0} redirects")]
    TooManyRedirects(usize),
}

/// Refuses URLs that must not be requested. Hostnames pass here and are
/// checked again when they resolve.
pub fn check_url(url: &Url) -> Result<(), Refusal> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refusal::UnsupportedScheme(url.scheme().to_string()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refusal::Credentials);
    }
    match url.host() {
        None => Err(Refusal::MissingHost),
        Some(Host::Ipv4(ip)) => public_or_refused(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => public_or_refused(IpAddr::V6(ip)),
        Some(Host::Domain(name)) => check_host_name(name),
    }
}

/// Stands in for input that is not a URL, which may hold anything.
const UNPARSEABLE_URL: &str = "<not a URL>";

pub fn loggable_url(url: &str) -> String {
    match Url::parse(url.trim()) {
        Ok(mut parsed) if parsed.host().is_some() => {
            if parsed.set_username("").is_ok() && parsed.set_password(None).is_ok() {
                parsed.to_string()
            } else {
                hostless(&parsed)
            }
        }
        // Without a host there is no authority to strip credentials from,
        // and the rest may hold them (`user:secret@…` parses as a scheme).
        Ok(parsed) => hostless(&parsed),
        Err(_) => UNPARSEABLE_URL.to_string(),
    }
}

fn hostless(url: &Url) -> String {
    format!("<{} URL>", url.scheme())
}

fn public_or_refused(ip: IpAddr) -> Result<(), Refusal> {
    if is_public_ip(ip) {
        Ok(())
    } else {
        Err(Refusal::PrivateAddress)
    }
}

/// Single-label names (`mcp-websearch-server-searxng`) and private suffixes
/// only resolve inside the cluster or a private network.
fn check_host_name(name: &str) -> Result<(), Refusal> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let private = name == LOCALHOST
        || !name.contains('.')
        || PRIVATE_NAME_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix));
    if private {
        Err(Refusal::PrivateHostName)
    } else {
        Ok(())
    }
}

/// Whether `ip` is a globally routable unicast address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

/// Public IPv6 is global unicast (`2000::/3`) outside the special-purpose
/// blocks IANA carves from it. Everything else (loopback, unspecified,
/// unique local, link-local, multicast, discard, reserved) falls outside
/// `2000::/3` and is refused without listing it. IPv4-mapped and NAT64
/// addresses are judged by the IPv4 address they reach.
fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    // NAT64 (64:ff9b::/96) reaches the embedded IPv4 address.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [a, b] = segments[6].to_be_bytes();
        let [c, d] = segments[7].to_be_bytes();
        return is_public_v4(Ipv4Addr::new(a, b, c, d));
    }
    let global_unicast = (segments[0] & 0xe000) == 0x2000;
    global_unicast
        && !((segments[0] == 0x2001 && segments[1] < 0x0200) // IETF protocol assignments: Teredo, benchmarking, ORCHID, …
            || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
            || segments[0] == 0x2002 // 6to4, reaches an embedded IPv4 address through any relay
            || (segments[0] & 0xfff0) == 0x3ff0) // documentation (3fff::/20)
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (b & 0xc0) == 64) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // benchmarking
        || a >= 240) // reserved
}
