//! The SSRF rules: which URLs and addresses may be requested.

use core::net::IpAddr;

use mcp_websearch_server::address_policy::{check_url, is_public_ip, Refusal};
use url::Url;

fn checked(url: &str) -> Result<(), Refusal> {
    let url = Url::parse(url).unwrap_or_else(|error| panic!("{url}: {error}"));
    check_url(&url)
}

fn ip(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|error| panic!("{text}: {error}"))
}

#[test]
fn public_http_and_https_urls_pass() {
    assert_eq!(checked("https://www.bbc.co.uk/sport"), Ok(()));
    assert_eq!(checked("http://93.184.215.14/"), Ok(()));
    assert_eq!(
        checked("https://[2606:2800:21f:cb07:6820:80da:af6b:8b2c]/"),
        Ok(())
    );
}

#[test]
fn private_loopback_link_local_and_cluster_literals_are_refused() {
    for url in [
        "http://10.0.0.1/admin",
        "http://172.16.5.4/",
        "http://192.168.1.1/",
        "http://127.0.0.1:8080/",
        "http://169.254.169.254/latest/meta-data/",
        "http://100.64.0.1/",
        "http://0.0.0.0/",
        "http://[::1]/",
        "http://[fd00::1]/",
        "http://[fe80::1]/",
        "http://[::ffff:10.0.0.1]/",
        "http://[64:ff9b::a00:1]/",
    ] {
        assert_eq!(checked(url), Err(Refusal::PrivateAddress), "{url}");
    }
}

#[test]
fn internal_host_names_are_refused() {
    for url in [
        "http://localhost:8000/",
        "http://mcp-websearch-server-searxng:8080/search",
        "http://app-tgi-core.orchestrator.svc.cluster.local:8000/v1",
        "http://printer.local/",
        "http://metadata.google.internal/",
    ] {
        assert_eq!(checked(url), Err(Refusal::PrivateHostName), "{url}");
    }
}

#[test]
fn other_schemes_and_credentials_are_refused() {
    assert_eq!(
        checked("file:///etc/passwd"),
        Err(Refusal::UnsupportedScheme("file".into()))
    );
    assert_eq!(
        checked("ftp://example.com/x"),
        Err(Refusal::UnsupportedScheme("ftp".into()))
    );
    assert_eq!(
        checked("https://user:secret@example.com/"),
        Err(Refusal::Credentials)
    );
}

#[test]
fn only_globally_routable_addresses_are_public() {
    assert!(is_public_ip(ip("8.8.8.8")));
    assert!(is_public_ip(ip("2a00:1450:4001:80b::200e")));
    for address in [
        "10.1.2.3",
        "192.0.0.8",
        "198.18.0.1",
        "203.0.113.5",
        "224.0.0.1",
        "255.255.255.255",
        "240.0.0.1",
        "2001:db8::1",
        "::",
        "::1.2.3.4",
        "2001:2::1",
        "2001:10::1",
        "2001:20::1",
        "2001::1",
        "2002:a00:1::1",
        "3fff::1",
        "100::1",
        "64:ff9b:1::1",
        "4000::1",
    ] {
        assert!(!is_public_ip(ip(address)), "{address}");
    }
}

#[test]
fn refusals_read_as_the_issue_specifies() {
    assert_eq!(Refusal::PrivateAddress.to_string(), "private address");
}
