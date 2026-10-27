//! The crawler: frontier bounds, path filters, robots.txt, relevance
//! scoring, over a scripted site and a scripted model.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use mcp_websearch_server::crawl::{
    bounded_count, compile_path_patterns, crawl_timeout, next_links, path_allowed, site_of,
    CrawlPlan, Crawler, HostGate, MAX_CRAWL_BREADTH, TRUNCATED_TEXT_CHARS,
};
use mcp_websearch_server::extract::PageReader;
use mcp_websearch_server::fetch::HttpFetcher;
use mcp_websearch_server::model::ChatModel;
use mcp_websearch_server::model::{parse_yes_no, strip_reasoning, ModelError, ModelTier};
use url::Url;

mod common;
use common::{ScriptedHttp, ScriptedModel, StalledHttp, StalledModel};

fn url(text: &str) -> Url {
    Url::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn plan(start_url: &str) -> CrawlPlan {
    CrawlPlan {
        start_url: start_url.to_string(),
        instructions: None,
        select_paths: Vec::new(),
        exclude_paths: Vec::new(),
        allow_external: false,
        full_content: false,
        max_breadth: 10,
        max_depth: 2,
        limit: 50,
    }
}

fn patterns(list: &[&str]) -> Vec<regex::Regex> {
    let list: Vec<String> = list.iter().map(|pattern| pattern.to_string()).collect();
    compile_path_patterns(&list).unwrap_or_else(|error| panic!("{error}"))
}

/// A page with enough prose to be read over HTTP, linking to `links`.
fn page(title: &str, links: &[&str]) -> String {
    let anchors: String = links
        .iter()
        .map(|link| format!(r#"<a href="{link}">{link}</a> "#))
        .collect();
    format!(
        "<html><body><main><h1>{title}</h1><p>{}</p><p>{anchors}</p></main></body></html>",
        "This page describes the company's products, customers and team in detail. ".repeat(4)
    )
}

#[test]
fn next_links_stay_on_site_skip_files_and_seen_links_and_cap_breadth() {
    let links: Vec<Url> = [
        "https://inxm.ai/team",
        "https://www.inxm.ai/pricing",
        "https://other.example/x",
        "https://inxm.ai/brochure.pdf",
        "https://inxm.ai/seen",
        "https://inxm.ai/a",
        "https://inxm.ai/b",
    ]
    .map(url)
    .to_vec();
    let seen = HashSet::from(["https://inxm.ai/seen".to_string()]);
    let narrow = CrawlPlan {
        max_breadth: 3,
        ..plan("https://inxm.ai")
    };

    let followed: Vec<String> = next_links(&links, &narrow, "inxm.ai", &seen)
        .iter()
        .map(Url::to_string)
        .collect();

    assert_eq!(
        followed,
        vec![
            "https://inxm.ai/team",
            "https://www.inxm.ai/pricing",
            "https://inxm.ai/a",
        ]
    );
}

#[test]
fn external_links_are_followed_only_when_allowed() {
    let links = vec![url("https://other.example/x")];
    let external = CrawlPlan {
        allow_external: true,
        ..plan("https://inxm.ai")
    };
    assert_eq!(
        next_links(&links, &external, "inxm.ai", &HashSet::new()).len(),
        1
    );
    assert!(next_links(&links, &plan("https://inxm.ai"), "inxm.ai", &HashSet::new()).is_empty());
}

#[test]
fn path_filters_select_then_exclude() {
    let select = patterns(&["^/about.*", "/pricing.*"]);
    let exclude = patterns(&["/about/careers"]);
    assert!(path_allowed(
        &url("https://a.example/about/team"),
        &select,
        &exclude
    ));
    assert!(path_allowed(
        &url("https://a.example/en/pricing"),
        &select,
        &exclude
    ));
    assert!(!path_allowed(
        &url("https://a.example/about/careers"),
        &select,
        &exclude
    ));
    assert!(!path_allowed(
        &url("https://a.example/blog"),
        &select,
        &exclude
    ));
    assert!(path_allowed(&url("https://a.example/blog"), &[], &[]));
}

#[test]
fn an_invalid_path_pattern_names_itself() {
    let error = compile_path_patterns(&["/about(".to_string()]).map(|_| ());
    assert!(
        matches!(error, Err(error) if error.to_string().starts_with("`/about(` is not a valid path pattern"))
    );
}

#[test]
fn www_and_bare_hosts_are_one_site() {
    assert_eq!(
        site_of(&url("https://WWW.inxm.ai/x")),
        Some("inxm.ai".to_string())
    );
}

#[test]
fn crawl_timeout_grows_with_size_between_a_floor_and_a_ceiling() {
    assert_eq!(crawl_timeout(1, 1), Duration::from_secs(60));
    assert_eq!(crawl_timeout(20, 2), Duration::from_secs(120));
    assert_eq!(crawl_timeout(1_000, 10), Duration::from_secs(300));
}

#[test]
fn yes_no_answers_are_read_past_reasoning_and_punctuation() {
    assert_eq!(parse_yes_no("Yes."), Some(true));
    assert_eq!(parse_yes_no("  no, it is a login page"), Some(false));
    assert_eq!(
        parse_yes_no("<think>it lists prices</think>\nYES"),
        Some(true)
    );
    assert_eq!(parse_yes_no("maybe"), None);
    assert_eq!(strip_reasoning("<think>x</think> answer "), "answer");
}

fn crawler(
    http: ScriptedHttp,
    model: ScriptedModel,
) -> (Crawler, Arc<ScriptedHttp>, Arc<ScriptedModel>) {
    let http = Arc::new(http);
    let model = Arc::new(model);
    let crawler = Crawler {
        reader: Arc::new(PageReader::new(http.clone(), None)),
        http: http.clone(),
        model: model.clone(),
        gate: Arc::new(HostGate::default()),
    };
    (crawler, http, model)
}

fn far_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(60)
}

const HTML: &str = "text/html";

#[tokio::test]
async fn the_crawl_is_breadth_first_bounded_by_depth_and_honours_robots() {
    let home = page("Home", &["/about", "/private/x", "/blog"]);
    let about = page("About", &["/about/deep"]);
    let blog = page("Blog", &[]);
    let deep = page("Deep", &[]);
    let (crawler, http, _) = crawler(
        ScriptedHttp::new(&[
            (
                "https://inxm.ai/robots.txt",
                200,
                "text/plain",
                "User-agent: *\nDisallow: /private\n",
            ),
            ("https://inxm.ai/", 200, HTML, &home),
            ("https://inxm.ai/about", 200, HTML, &about),
            ("https://inxm.ai/blog", 200, HTML, &blog),
            ("https://inxm.ai/about/deep", 200, HTML, &deep),
        ]),
        ScriptedModel::new(&[]),
    );
    let shallow = CrawlPlan {
        max_depth: 1,
        ..plan("https://inxm.ai")
    };

    let outcome = crawler.crawl(&shallow, far_deadline()).await;

    let urls: Vec<&str> = outcome
        .response
        .extracted
        .iter()
        .map(|page| page.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![
            "https://inxm.ai/",
            "https://inxm.ai/about",
            "https://inxm.ai/blog"
        ]
    );
    let failed: Vec<(&str, &str)> = outcome
        .response
        .failed
        .iter()
        .map(|failure| (failure.url.as_str(), failure.error.as_str()))
        .collect();
    assert_eq!(
        failed,
        vec![("https://inxm.ai/private/x", "disallowed by robots.txt")]
    );
    assert!(!http
        .requested()
        .contains(&"https://inxm.ai/private/x".to_string()));
    assert!(!http
        .requested()
        .contains(&"https://inxm.ai/about/deep".to_string()));
    assert_eq!(outcome.pages_read, 3);
    assert!(!outcome.timed_out);
}

#[tokio::test]
async fn the_page_limit_stops_the_crawl_and_text_is_truncated() {
    let home = page("Home", &["/a", "/b", "/c"]);
    let long = format!("<p>{}</p>", "x".repeat(TRUNCATED_TEXT_CHARS * 2));
    let (crawler, _, _) = crawler(
        ScriptedHttp::new(&[
            ("https://site.example/robots.txt", 404, HTML, ""),
            ("https://site.example/", 200, HTML, &home),
            ("https://site.example/a", 200, HTML, &long),
            ("https://site.example/b", 200, HTML, &long),
        ]),
        ScriptedModel::new(&[]),
    );
    let limited = CrawlPlan {
        limit: 2,
        ..plan("https://site.example/")
    };

    let outcome = crawler.crawl(&limited, far_deadline()).await;

    assert_eq!(outcome.pages_read, 2);
    assert_eq!(outcome.response.extracted.len(), 2);
    assert_eq!(
        outcome.response.extracted[1].text.chars().count(),
        TRUNCATED_TEXT_CHARS
    );
}

#[tokio::test]
async fn an_unreachable_robots_txt_stops_the_crawl_and_a_private_start_is_refused() {
    let (crawler, http, _) = crawler(ScriptedHttp::new(&[]), ScriptedModel::new(&[]));

    let unreachable = crawler
        .crawl(&plan("https://down.example/"), far_deadline())
        .await;
    assert_eq!(
        unreachable.response.failed[0].error,
        "robots.txt could not be read; the site is not crawled"
    );

    let private = crawler
        .crawl(&plan("http://10.0.0.1/admin"), far_deadline())
        .await;
    assert_eq!(private.response.failed[0].error, "refused: private address");
    assert!(!http
        .requested()
        .iter()
        .any(|requested| requested.starts_with("http://10.0.0.1")));
}

#[tokio::test]
async fn instructions_drop_pages_scored_irrelevant_and_keep_unscored_ones() {
    let home = page("Home", &["/pricing", "/careers", "/press"]);
    let (crawler, _, model) = crawler(
        ScriptedHttp::new(&[
            ("https://co.example/robots.txt", 404, HTML, ""),
            ("https://co.example/", 200, HTML, &home),
            (
                "https://co.example/pricing",
                200,
                HTML,
                &page("Pricing", &[]),
            ),
            (
                "https://co.example/careers",
                200,
                HTML,
                &page("Careers", &[]),
            ),
            ("https://co.example/press", 200, HTML, &page("Press", &[])),
        ]),
        ScriptedModel::new(&[
            ("Page URL: https://co.example/pricing", Ok("yes")),
            ("Page URL: https://co.example/careers", Ok("No.")),
            (
                "Page URL: https://co.example/press",
                Err(ModelError::Status(503)),
            ),
            ("Page URL: https://co.example/", Ok("yes")),
        ]),
    );
    let scored = CrawlPlan {
        instructions: Some("pricing tiers".into()),
        max_depth: 1,
        ..plan("https://co.example/")
    };

    let outcome = crawler.crawl(&scored, far_deadline()).await;

    let urls: Vec<&str> = outcome
        .response
        .extracted
        .iter()
        .map(|page| page.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec![
            "https://co.example/",
            "https://co.example/pricing",
            "https://co.example/press"
        ]
    );
    assert_eq!(
        (outcome.scored_irrelevant, outcome.scoring_failures),
        (1, 1)
    );
    assert!(model
        .calls()
        .iter()
        .all(|(tier, _)| *tier == ModelTier::Fast));
    assert_eq!(model.calls().len(), 4, "each page is scored once");
}

#[tokio::test]
async fn a_passed_deadline_returns_what_was_read() {
    let (crawler, _, _) = crawler(ScriptedHttp::new(&[]), ScriptedModel::new(&[]));
    let outcome = crawler
        .crawl(&plan("https://site.example/"), tokio::time::Instant::now())
        .await;
    assert!(outcome.timed_out);
    assert!(outcome.response.extracted.is_empty());
}

#[test]
fn caller_bounds_are_clamped_between_one_and_the_ceiling() {
    assert_eq!(bounded_count(0, MAX_CRAWL_BREADTH), 1);
    assert_eq!(bounded_count(7, MAX_CRAWL_BREADTH), 7);
    assert_eq!(
        bounded_count(u32::MAX, MAX_CRAWL_BREADTH),
        MAX_CRAWL_BREADTH
    );
}

#[tokio::test]
async fn a_start_page_redirecting_elsewhere_does_not_move_the_crawl() {
    let landing = page("Elsewhere", &["/other-page"]);
    let (crawler, http, _) = crawler(
        ScriptedHttp::new(&[
            ("https://start.example/robots.txt", 404, HTML, ""),
            ("https://elsewhere.example/", 200, HTML, &landing),
            (
                "https://elsewhere.example/other-page",
                200,
                HTML,
                &page("Other", &[]),
            ),
        ])
        .with_redirect("https://start.example/", "https://elsewhere.example/"),
        ScriptedModel::new(&[]),
    );

    let outcome = crawler
        .crawl(&plan("https://start.example/"), far_deadline())
        .await;

    let urls: Vec<&str> = outcome
        .response
        .extracted
        .iter()
        .map(|page| page.url.as_str())
        .collect();
    assert_eq!(urls, vec!["https://elsewhere.example/"]);
    assert!(!http
        .requested()
        .contains(&"https://elsewhere.example/other-page".to_string()));
}

fn stalled_crawler(http: Arc<dyn HttpFetcher>, model: Arc<dyn ChatModel>) -> Crawler {
    Crawler {
        reader: Arc::new(PageReader::new(http.clone(), None)),
        http,
        model,
        gate: Arc::new(HostGate::default()),
    }
}

#[tokio::test]
async fn a_stalled_robots_txt_cannot_outlast_the_deadline() {
    let crawler = stalled_crawler(Arc::new(StalledHttp), Arc::new(ScriptedModel::new(&[])));
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        crawler.crawl(&plan("https://slow.example/"), deadline),
    )
    .await
    .unwrap_or_else(|_| panic!("the crawl ran past its deadline"));

    assert!(outcome.timed_out);
    assert_eq!(outcome.response.failed[0].error, "crawl timed out");
}

#[tokio::test]
async fn a_stalled_model_leaves_pages_unscored_by_the_deadline() {
    let http = Arc::new(ScriptedHttp::new(&[
        ("https://co.example/robots.txt", 404, HTML, ""),
        ("https://co.example/", 200, HTML, &page("Home", &[])),
    ]));
    let crawler = stalled_crawler(http, Arc::new(StalledModel));
    let scored = CrawlPlan {
        instructions: Some("pricing".into()),
        ..plan("https://co.example/")
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);

    let outcome = tokio::time::timeout(Duration::from_secs(5), crawler.crawl(&scored, deadline))
        .await
        .unwrap_or_else(|_| panic!("scoring ran past the deadline"));

    assert!(outcome.timed_out);
    assert_eq!(
        outcome.response.extracted.len(),
        1,
        "the unscored page is kept"
    );
    assert_eq!(outcome.scoring_failures, 1);
}
