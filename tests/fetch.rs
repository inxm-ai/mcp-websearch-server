//! Redirects are followed one hop at a time, each hop under the address
//! policy and the caller's gate, before it is requested.

use async_trait::async_trait;
use mcp_websearch_server::address_policy::Refusal;
use mcp_websearch_server::fetch::{
    get_following_redirects, AnyHop, FetchError, HopGate, MAX_REDIRECTS,
};
use url::Url;

mod common;
use common::ScriptedHttp;

fn url(text: &str) -> Url {
    Url::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

/// Refuses every path under `/closed`.
struct ClosedPaths;

#[async_trait]
impl HopGate for ClosedPaths {
    async fn allow(&self, url: &Url) -> Result<(), String> {
        if url.path().starts_with("/closed") {
            Err("closed by the gate".to_string())
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn a_redirect_chain_ends_at_the_page_with_its_url() {
    let http = ScriptedHttp::new(&[("https://a.example/end", 200, "text/html", "<p>end</p>")])
        .with_redirect("https://a.example/start", "/middle")
        .with_redirect("https://a.example/middle", "https://a.example/end");

    let fetched = get_following_redirects(&http, &url("https://a.example/start"), 1024, &AnyHop)
        .await
        .unwrap_or_else(|error| panic!("fetch failed: {error}"));

    assert_eq!(fetched.final_url.as_str(), "https://a.example/end");
    assert_eq!(fetched.status, 200);
}

#[tokio::test]
async fn hops_the_policy_or_the_gate_refuse_are_never_requested() {
    let http = ScriptedHttp::new(&[])
        .with_redirect("https://a.example/to-private", "http://10.0.0.1/admin")
        .with_redirect("https://a.example/to-closed", "/closed/area");

    assert_eq!(
        get_following_redirects(&http, &url("https://a.example/to-private"), 1024, &AnyHop).await,
        Err(FetchError::Refused(Refusal::PrivateAddress))
    );
    assert_eq!(
        get_following_redirects(
            &http,
            &url("https://a.example/to-closed"),
            1024,
            &ClosedPaths
        )
        .await,
        Err(FetchError::HopRefused("closed by the gate".to_string()))
    );
    assert_eq!(
        http.requested(),
        vec![
            "https://a.example/to-private",
            "https://a.example/to-closed"
        ]
    );
}

#[tokio::test]
async fn a_redirect_loop_stops_after_the_hop_limit() {
    let http = ScriptedHttp::new(&[])
        .with_redirect("https://a.example/ping", "/pong")
        .with_redirect("https://a.example/pong", "/ping");

    let fetched =
        get_following_redirects(&http, &url("https://a.example/ping"), 1024, &AnyHop).await;

    assert_eq!(
        fetched,
        Err(FetchError::Refused(Refusal::TooManyRedirects(
            MAX_REDIRECTS
        )))
    );
    assert_eq!(http.requested().len(), MAX_REDIRECTS + 1);
}
