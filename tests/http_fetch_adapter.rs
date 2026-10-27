//! The real HTTP adapter refuses internal targets before any connection is
//! attempted; these checks never leave the machine.

use mcp_websearch_server::address_policy::Refusal;
use mcp_websearch_server::fetch::{FetchError, HttpFetcher};
use mcp_websearch_server::http_fetch_adapter::ReqwestFetcher;
use url::Url;

#[tokio::test]
async fn internal_targets_are_refused_before_connecting() {
    let fetcher = ReqwestFetcher::new().unwrap_or_else(|error| panic!("client: {error}"));
    for (url, refusal) in [
        ("http://127.0.0.1:9/", Refusal::PrivateAddress),
        ("http://localhost:9/", Refusal::PrivateHostName),
        ("http://[::1]:9/", Refusal::PrivateAddress),
    ] {
        let url = Url::parse(url).unwrap_or_else(|error| panic!("{url}: {error}"));
        assert_eq!(
            fetcher.get(&url, 1024).await,
            Err(FetchError::Refused(refusal)),
            "{url}"
        );
    }
}
