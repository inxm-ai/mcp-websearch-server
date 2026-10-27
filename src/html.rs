//! HTML to readable text, and the links a page points to.

use std::collections::HashSet;

use ego_tree::iter::Edge;
use scraper::{ElementRef, Html, Node};
use url::Url;

/// Elements that never hold the page's prose.
const NEVER_PROSE: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "head", "nav", "aside", "form", "iframe",
    "button", "select", "dialog", "canvas", "object", "embed",
];
/// Page chrome, dropped unless it sits inside the main content (an
/// article's own `<header>` holds its headline).
const PAGE_CHROME: &[&str] = &["header", "footer"];
/// Elements that hold the main content, in order of preference.
const CONTENT_ROOTS: &[&str] = &["main", "article"];
const CONTENT_ROOT_ROLE: &str = "main";
/// Elements whose boundaries become line breaks.
const BLOCKS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "tr",
    "td",
    "th",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "pre",
    "blockquote",
    "table",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "figcaption",
    "hr",
    "address",
];
/// A content root shorter than this is not trusted to be the main content.
const MIN_CONTENT_ROOT_CHARS: usize = 200;

fn is_hidden(element: &ElementRef<'_>) -> bool {
    let value = element.value();
    value.attr("hidden").is_some() || value.attr("aria-hidden") == Some("true")
}

/// The text under `root`: prose elements only, block boundaries as line
/// breaks, whitespace collapsed, blank and repeated lines dropped.
fn text_under(root: ElementRef<'_>, inside_content: bool) -> String {
    let mut raw = String::new();
    // The element whose subtree is being skipped, if any.
    let mut skip_until: Option<ego_tree::NodeId> = None;
    let mut content_depth = usize::from(inside_content);
    for edge in root.traverse() {
        match edge {
            Edge::Open(node) => {
                if skip_until.is_some() {
                    continue;
                }
                match node.value() {
                    Node::Text(text) => raw.push_str(text),
                    Node::Element(element) => {
                        let name = element.name();
                        let skipped = NEVER_PROSE.contains(&name)
                            || (content_depth == 0 && PAGE_CHROME.contains(&name))
                            || ElementRef::wrap(node).is_some_and(|element| is_hidden(&element));
                        if skipped {
                            skip_until = Some(node.id());
                            continue;
                        }
                        if CONTENT_ROOTS.contains(&name) {
                            content_depth += 1;
                        }
                        if BLOCKS.contains(&name) {
                            raw.push('\n');
                        } else {
                            raw.push(' ');
                        }
                    }
                    _ => {}
                }
            }
            Edge::Close(node) => {
                if skip_until == Some(node.id()) {
                    skip_until = None;
                    continue;
                }
                if skip_until.is_some() {
                    continue;
                }
                if let Node::Element(element) = node.value() {
                    let name = element.name();
                    if CONTENT_ROOTS.contains(&name) {
                        content_depth = content_depth.saturating_sub(1);
                    }
                    if BLOCKS.contains(&name) {
                        raw.push('\n');
                    }
                }
            }
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() && lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn is_content_root(element: &ElementRef<'_>) -> bool {
    CONTENT_ROOTS.contains(&element.value().name())
        || element.value().attr("role") == Some(CONTENT_ROOT_ROLE)
}

/// Readable text of an HTML document: the main content when the page marks
/// one (`<main>`, `<article>`, `role="main"`), otherwise the body without
/// navigation, headers, footers, scripts, styles and forms.
pub fn readable_text(html: &str) -> String {
    let document = Html::parse_document(html);
    let root = document.root_element();
    let content_root_text = root
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(is_content_root)
        .map(|element| text_under(element, true))
        .find(|text| text.chars().count() >= MIN_CONTENT_ROOT_CHARS);
    content_root_text.unwrap_or_else(|| text_under(root, false))
}

/// Absolute http(s) links on the page, without fragments, deduplicated in
/// document order.
pub fn links(html: &str, base: &Url) -> Vec<Url> {
    let document = Html::parse_document(html);
    let mut seen = HashSet::new();
    document
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|element| element.value().name() == "a")
        .filter_map(|element| element.value().attr("href"))
        .filter_map(|href| base.join(href.trim()).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|mut url| {
            url.set_fragment(None);
            url
        })
        .filter(|url| seen.insert(url.to_string()))
        .collect()
}
