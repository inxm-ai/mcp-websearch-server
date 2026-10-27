//! HTML to readable text, and link discovery for the crawler.

use mcp_websearch_server::html::{links, readable_text};
use url::Url;

const ARTICLE: &str = "Arsenal confirmed on Sunday that the club has agreed terms with a new \
    sponsor. The statement, published on the club website, says the agreement runs for five \
    seasons and covers the men's and women's first teams as well as the academy.";

#[test]
fn page_chrome_scripts_and_styles_are_dropped() {
    let html = format!(
        r#"<html><head><title>T</title><style>.a{{color:red}}</style></head>
        <body>
          <header><a href="/">Home</a> <a href="/news">News</a></header>
          <nav><ul><li>Fixtures</li><li>Tickets</li></ul></nav>
          <div><h1>Club statement</h1><p>{ARTICLE}</p></div>
          <script>window.tracking = true;</script>
          <aside>Related: shop now</aside>
          <footer>© Arsenal</footer>
        </body></html>"#
    );
    assert_eq!(readable_text(&html), format!("Club statement\n{ARTICLE}"));
}

#[test]
fn the_main_content_wins_and_keeps_its_own_header() {
    let html = format!(
        r#"<body><div class="promo">Subscribe to our newsletter</div>
        <main><article><header><h1>Club statement</h1></header>
        <p>{ARTICLE}</p><p>Second   paragraph.</p></article></main>
        <div>Cookie banner text</div></body>"#
    );
    assert_eq!(
        readable_text(&html),
        format!("Club statement\n{ARTICLE}\nSecond paragraph.")
    );
}

#[test]
fn a_short_main_element_falls_back_to_the_body() {
    let html = format!(r#"<body><main>Loading…</main><section><p>{ARTICLE}</p></section></body>"#);
    assert_eq!(readable_text(&html), format!("Loading…\n{ARTICLE}"));
}

#[test]
fn hidden_elements_and_entities_are_handled() {
    let html = r#"<body><p>Fish &amp; chips</p><div hidden>secret</div><p aria-hidden="true">icon</p></body>"#;
    assert_eq!(readable_text(html), "Fish & chips");
}

#[test]
fn an_app_shell_has_no_readable_text() {
    let html = r#"<html><body><div id="root"></div><script src="/app.js"></script></body></html>"#;
    assert_eq!(readable_text(html), "");
}

#[test]
fn links_are_absolute_http_without_fragments_and_unique() {
    let base = Url::parse("https://inxm.ai/about/").unwrap_or_else(|error| panic!("base: {error}"));
    let html = r##"<a href="/team">Team</a><a href="pricing#tiers">Pricing</a>
        <a href="https://inxm.ai/team">Team again</a><a href="mailto:hi@inxm.ai">Mail</a>
        <a href="javascript:void(0)">JS</a><a href="#top">Top</a>"##;
    let found: Vec<String> = links(html, &base).iter().map(Url::to_string).collect();
    assert_eq!(
        found,
        vec![
            "https://inxm.ai/team",
            "https://inxm.ai/about/pricing",
            "https://inxm.ai/about/",
        ]
    );
}
