//! `extract` and `download_file` over a scripted HTTP fetcher: the
//! needs-render decision, content types, failures and result shape.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use mcp_websearch_server::download::{download, encode_file, DownloadFailure, MAX_DOWNLOAD_BYTES};
use mcp_websearch_server::extract::{
    content_kind, extract_urls, needs_render, ContentKind, ExtractStage, PageReader, RenderError,
    RenderedPage, Renderer,
};
use mcp_websearch_server::fetch::{FetchError, FetchedBody, HttpFetcher};
use url::Url;

const PROSE: &str = "The quarterly report shows revenue growth across every region, driven by \
    new enterprise contracts and a strong renewal rate. Management expects the trend to \
    continue into next year as the product line expands into adjacent markets.";

/// Answers each URL from a fixed table; unknown URLs time out.
struct ScriptedHttp {
    pages: HashMap<String, (u16, &'static str, Vec<u8>)>,
}

impl ScriptedHttp {
    fn new(pages: &[(&str, u16, &'static str, &str)]) -> Self {
        Self {
            pages: pages
                .iter()
                .map(|(url, status, content_type, body)| {
                    (
                        url.to_string(),
                        (*status, *content_type, body.as_bytes().to_vec()),
                    )
                })
                .collect(),
        }
    }
}

#[async_trait]
impl HttpFetcher for ScriptedHttp {
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<FetchedBody, FetchError> {
        let (status, content_type, body) =
            self.pages.get(url.as_str()).ok_or(FetchError::Timeout)?;
        Ok(FetchedBody {
            final_url: url.clone(),
            status: *status,
            content_type: content_type.to_string(),
            body: body.iter().copied().take(max_bytes).collect(),
            truncated: body.len() > max_bytes,
        })
    }
}

#[test]
fn short_boilerplate_or_script_shells_need_rendering() {
    assert!(needs_render(""));
    assert!(needs_render("Home News Contact"));
    let shell = format!("{PROSE} You need to enable JavaScript to run this app.");
    assert!(needs_render(&shell));
    assert!(!needs_render(PROSE));
    let long_article_mentioning_javascript =
        format!("{PROSE} {PROSE} {PROSE} {PROSE} {PROSE} Enable JavaScript for comments.");
    assert!(!needs_render(&long_article_mentioning_javascript));
}

#[test]
fn content_types_decide_how_a_page_is_read() {
    assert_eq!(content_kind("text/html; charset=utf-8"), ContentKind::Html);
    assert_eq!(content_kind(""), ContentKind::Html);
    assert_eq!(content_kind("text/plain"), ContentKind::PlainText);
    assert_eq!(content_kind("application/pdf"), ContentKind::Unsupported);
}

#[tokio::test]
async fn extract_reports_pages_and_typed_failures_in_request_order() {
    let article = format!("<html><body><nav>Menu</nav><p>{PROSE}</p></body></html>");
    let http = ScriptedHttp::new(&[
        ("https://news.example/a", 200, "text/html", &article),
        (
            "https://news.example/report.pdf",
            200,
            "application/pdf",
            "%PDF-1.7",
        ),
        ("https://news.example/gone", 404, "text/html", "not found"),
        (
            "https://news.example/shell",
            200,
            "text/html",
            "<div id=root></div>",
        ),
        (
            "https://news.example/notes.txt",
            200,
            "text/plain",
            " plain notes ",
        ),
    ]);
    let reader = PageReader::new(Arc::new(http), None);
    let urls: Vec<String> = [
        "https://news.example/a",
        "http://10.0.0.1/admin",
        "https://news.example/report.pdf",
        "https://news.example/gone",
        "https://news.example/shell",
        "https://news.example/notes.txt",
        "not a url",
        "https://news.example/slow",
    ]
    .map(String::from)
    .to_vec();

    let (response, counts) = extract_urls(&reader, &urls).await;

    let extracted: Vec<(&str, &str)> = response
        .extracted
        .iter()
        .map(|page| (page.url.as_str(), page.text.as_str()))
        .collect();
    assert_eq!(
        extracted,
        vec![
            ("https://news.example/a", PROSE),
            ("https://news.example/notes.txt", "plain notes"),
        ]
    );
    let failed: Vec<(&str, &str)> = response
        .failed
        .iter()
        .map(|failure| (failure.url.as_str(), failure.error.as_str()))
        .collect();
    assert_eq!(
        failed,
        vec![
            ("http://10.0.0.1/admin", "refused: private address"),
            (
                "https://news.example/report.pdf",
                "unsupported content type: application/pdf"
            ),
            ("https://news.example/gone", "HTTP status 404"),
            ("https://news.example/shell", "no readable text"),
            ("not a url", "invalid URL: relative URL without a base"),
            ("https://news.example/slow", "timed out"),
        ]
    );
    assert_eq!((counts.http, counts.failed), (2, 6));
}

#[test]
fn text_files_stay_text_and_binary_files_become_base64() {
    let json = encode_file("application/json", br#"{"a":1}"#);
    assert_eq!(json.content, r#"{"a":1}"#);
    let png = encode_file("image/png", &[0x89, b'P', b'N', b'G']);
    assert_eq!(png.content, "iVBORw==");
    assert_eq!(png.content_type, "image/png");
    let unknown = encode_file("", &[0xff]);
    assert_eq!(unknown.content_type, "application/octet-stream");
    assert_eq!(unknown.content, "/w==");
}

#[tokio::test]
async fn downloads_refuse_private_addresses_and_oversized_files() {
    let oversized = "x".repeat(MAX_DOWNLOAD_BYTES + 1);
    let http = ScriptedHttp::new(&[
        (
            "https://files.example/big.txt",
            200,
            "text/plain",
            &oversized,
        ),
        (
            "https://files.example/ok.csv",
            200,
            "text/csv",
            "a,b\n1,2\n",
        ),
    ]);
    assert_eq!(
        download(&http, "https://files.example/big.txt").await,
        Err(DownloadFailure::TooLarge)
    );
    assert!(matches!(
        download(&http, "http://169.254.169.254/latest").await,
        Err(DownloadFailure::Refused(_))
    ));
    let file = download(&http, "https://files.example/ok.csv")
        .await
        .unwrap_or_else(|error| panic!("download failed: {error}"));
    assert_eq!(file.content, "a,b\n1,2\n");
}

/// Renders from a script and counts how often it was asked.
struct ScriptedRenderer {
    answer: Result<String, RenderError>,
    calls: AtomicUsize,
}

impl ScriptedRenderer {
    fn new(answer: Result<&str, RenderError>) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.map(str::to_string),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Renderer for ScriptedRenderer {
    async fn render(&self, url: &Url) -> Result<RenderedPage, RenderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer.clone().map(|html| RenderedPage {
            final_url: url.clone(),
            html,
        })
    }
}

fn reader_with(renderer: &Arc<ScriptedRenderer>, page_html: &str) -> PageReader {
    let http = ScriptedHttp::new(&[("https://app.example/news", 200, "text/html", page_html)]);
    PageReader::new(Arc::new(http), Some(renderer.clone() as Arc<dyn Renderer>))
}

#[tokio::test]
async fn a_script_shell_is_read_from_the_rendered_page() {
    let renderer = ScriptedRenderer::new(Ok(&format!(
        "<html><body><main><p>{PROSE}</p></main></body></html>"
    )));
    let reader = reader_with(&renderer, "<div id=root></div><script src=app.js></script>");

    let page = reader
        .read("https://app.example/news")
        .await
        .unwrap_or_else(|error| panic!("read failed: {error}"));

    assert_eq!(page.stage, ExtractStage::Render);
    assert_eq!(page.text, PROSE);
}

#[tokio::test]
async fn a_readable_page_is_never_rendered() {
    let renderer = ScriptedRenderer::new(Ok("<p>rendered</p>"));
    let reader = reader_with(&renderer, &format!("<p>{PROSE}</p>"));

    let page = reader
        .read("https://app.example/news")
        .await
        .unwrap_or_else(|error| panic!("read failed: {error}"));

    assert_eq!(page.stage, ExtractStage::Http);
    assert_eq!(renderer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_render_keeps_short_http_text_or_reports_why() {
    let renderer = ScriptedRenderer::new(Err(RenderError::Timeout));

    let short = reader_with(&renderer, "<p>Opening hours: 9 to 5.</p>")
        .read("https://app.example/news")
        .await
        .unwrap_or_else(|error| panic!("read failed: {error}"));
    assert_eq!(
        (short.stage, short.text.as_str()),
        (ExtractStage::Http, "Opening hours: 9 to 5.")
    );

    let shell = reader_with(&renderer, "<div id=root></div>")
        .read("https://app.example/news")
        .await;
    assert_eq!(
        shell.map_err(|failure| failure.to_string()),
        Err("no readable text without rendering, and rendering failed: timed out".to_string())
    );
}

#[tokio::test]
async fn a_render_with_less_text_than_http_keeps_the_http_text() {
    let renderer = ScriptedRenderer::new(Ok("<p>Cookie wall</p>"));
    let reader = reader_with(&renderer, "<p>Opening hours: 9 to 5, Monday to Friday.</p>");
    let page = reader
        .read("https://app.example/news")
        .await
        .unwrap_or_else(|error| panic!("read failed: {error}"));
    assert_eq!(page.stage, ExtractStage::Http);
}
