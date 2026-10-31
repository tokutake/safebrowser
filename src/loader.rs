//! Page fetching and parsing (runs on a background thread), plus internal pages.

use std::collections::HashMap;
use std::sync::Arc;

use url::Url;

use crate::css::{self, Stylesheet};
use crate::form::Request;
use crate::dom::{self, Node, StyleSource};
use crate::image::{self, Image};
use crate::layout::Images;
use crate::{html, js, net};

const MAX_STYLESHEETS: usize = 32;
const MAX_TOTAL_CSS_BYTES: usize = 10 * 1024 * 1024;
const MAX_IMAGES: usize = 48;
const MAX_TOTAL_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SCRIPTS: usize = 24;
const MAX_TOTAL_SCRIPT_BYTES: usize = 6 * 1024 * 1024;
/// Requests in flight at once for subresources.
const PARALLEL_FETCHES: usize = 8;
pub const HOME_URL: &str = "about:home";

/// Connection state shown in the toolbar, based on what was actually received, not on the URL.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PageKind {
    /// Fetched over HTTPS.
    Secure,
    /// Fetched unencrypted from a local development server.
    LocalHttp,
    /// Built-in page.
    Internal,
    /// Failed or blocked.
    Error,
}

pub struct Loaded {
    pub url: Url,
    pub kind: PageKind,
    /// Base for relative links (honors <base href>).
    pub base: Url,
    pub dom: Node,
    pub sheets: Vec<Stylesheet>,
    pub title: String,
    pub images: Images,
}

pub fn load(agent: &ureq::Agent, request: &Request) -> Loaded {
    match fetch_page(agent, request) {
        Ok(page) => page,
        Err(message) => error_page(request.url(), &message),
    }
}

fn fetch_page(agent: &ureq::Agent, request: &Request) -> Result<Loaded, String> {
    let form = match request {
        Request::Get(_) => None,
        Request::Post(_, pairs) => Some(pairs.as_slice()),
    };
    let resp = net::fetch_with_form(agent, request.url(), form, net::MAX_PAGE_BYTES)?;
    let source = match resp.content_type.as_str() {
        "text/html" | "application/xhtml+xml" => net::decode_text(&resp.body, resp.charset.as_deref(), true),
        ct if ct.starts_with("text/") => {
            format!("<pre>{}</pre>", escape_html(&net::decode_text(&resp.body, resp.charset.as_deref(), false)))
        }
        ct => return Err(format!("Files of type \"{ct}\" cannot be displayed (this browser has no downloads).")),
    };
    let kind = if resp.url.scheme() == "https" { PageKind::Secure } else { PageKind::LocalHttp };
    Ok(from_html(resp.url, kind, &source, Some(agent)))
}

/// Parses HTML, runs its scripts, and fetches external stylesheets and images when an agent is given.
pub fn from_html(url: Url, kind: PageKind, source: &str, agent: Option<&ureq::Agent>) -> Loaded {
    let mut dom = html::parse(source);
    if let Some(agent) = agent {
        dom = run_scripts(agent, &url, dom);
    }
    let info = dom::doc_info(&dom);
    let base = info.base.as_deref().and_then(|b| url.join(b).ok()).unwrap_or_else(|| url.clone());

    // Each distinct stylesheet is fetched once; sites often link the same file repeatedly
    let mut urls: Vec<Url> = Vec::new();
    if agent.is_some() {
        for source in &info.styles {
            if let StyleSource::External(href) = source {
                if let Ok(css_url) = base.join(href) {
                    if urls.len() < MAX_STYLESHEETS && !urls.contains(&css_url) {
                        urls.push(css_url);
                    }
                }
            }
        }
    }
    let mut fetched = fetch_stylesheets(agent, &urls);

    let mut sheets = Vec::new();
    for source in &info.styles {
        match source {
            StyleSource::Inline(text) => sheets.push(css::parse_stylesheet(text)),
            StyleSource::External(href) => {
                let index = base.join(href).ok().and_then(|u| urls.iter().position(|x| *x == u));
                // take() so a repeated link applies the sheet only once
                if let Some(text) = index.and_then(|i| fetched[i].take()) {
                    sheets.push(css::parse_stylesheet(&text));
                }
            }
        }
    }
    let title = if info.title.is_empty() { url.to_string() } else { info.title };
    let images = fetch_images(agent, &base, &dom);
    Loaded { url, kind, base, dom, sheets, title, images }
}

/// Executes the page's scripts (see js.rs). Script files are fetched first, in parallel.
fn run_scripts(agent: &ureq::Agent, url: &Url, dom: Node) -> Node {
    let base = dom::doc_info(&dom).base.as_deref().and_then(|b| url.join(b).ok()).unwrap_or_else(|| url.clone());
    let mut srcs = js::external_sources(&dom);
    srcs.truncate(MAX_SCRIPTS);
    let urls: Vec<Option<Url>> = srcs.iter().map(|src| base.join(src).ok()).collect();
    let bodies = fetch_all(&urls, |u| {
        let resp = net::fetch_accepting(agent, u, net::MAX_SCRIPT_BYTES, net::ACCEPT_SCRIPT).ok()?;
        Some(net::decode_text(&resp.body, resp.charset.as_deref(), false))
    });
    let mut total = 0;
    let mut external = HashMap::new();
    for (src, body) in srcs.into_iter().zip(bodies) {
        if let Some(body) = body {
            total += body.len();
            if total <= MAX_TOTAL_SCRIPT_BYTES {
                external.insert(src, body);
            }
        }
    }
    js::run(dom, &base, external)
}

/// Fetches every distinct <img> source (data: URLs are decoded in place). Failures are
/// ignored: the image just shows its alt text.
fn fetch_images(agent: Option<&ureq::Agent>, base: &Url, dom: &Node) -> Images {
    let mut sources = Vec::new();
    collect_image_sources(dom, &mut sources);
    let mut images = Images::new();
    let mut remote: Vec<(String, Option<Url>)> = Vec::new();
    for src in sources {
        if images.contains_key(&src) || remote.iter().any(|(s, _)| *s == src) {
            continue;
        }
        if src.starts_with("data:") {
            if let Some(img) = image::decode_data_url(&src) {
                images.insert(src, Arc::new(img));
            }
        } else if agent.is_some() && remote.len() < MAX_IMAGES {
            let resolved = base.join(&src).ok();
            remote.push((src, resolved));
        }
    }
    let Some(agent) = agent else { return images };
    let urls: Vec<Option<Url>> = remote.iter().map(|(_, u)| u.clone()).collect();
    let decoded: Vec<Option<Image>> = fetch_all(&urls, |u| {
        let resp = net::fetch_accepting(agent, u, net::MAX_IMAGE_BYTES, net::ACCEPT_IMAGE).ok()?;
        image::decode(&resp.body).map(|img| (img, resp.body.len()))
    })
    .into_iter()
    .scan(0usize, |total, r| {
        // Past the byte budget the remaining images are dropped
        Some(r.and_then(|(img, bytes)| {
            *total += bytes;
            (*total <= MAX_TOTAL_IMAGE_BYTES).then_some(img)
        }))
    })
    .collect();
    for ((src, _), img) in remote.into_iter().zip(decoded) {
        if let Some(img) = img {
            images.insert(src, Arc::new(img));
        }
    }
    images
}

fn collect_image_sources(node: &Node, out: &mut Vec<String>) {
    let Node::Element(el) = node else { return };
    if el.tag == "img" {
        if let Some(src) = dom::image_source(el) {
            out.push(src.to_string());
        }
    }
    for child in &el.children {
        collect_image_sources(child, out);
    }
}

/// Runs `job` for each URL, `PARALLEL_FETCHES` at a time, keeping the order of `urls`.
fn fetch_all<T: Send>(urls: &[Option<Url>], job: impl Fn(&Url) -> Option<T> + Sync) -> Vec<Option<T>> {
    let mut results = Vec::with_capacity(urls.len());
    for chunk in urls.chunks(PARALLEL_FETCHES) {
        let job = &job;
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk.iter().map(|u| scope.spawn(move || job(u.as_ref()?))).collect();
            results.extend(handles.into_iter().map(|h| h.join().ok().flatten()));
        });
    }
    results
}

/// Fetches stylesheets in parallel. Failures are ignored (the page is usable without them),
/// and sheets past the total size budget are dropped.
fn fetch_stylesheets(agent: Option<&ureq::Agent>, urls: &[Url]) -> Vec<Option<String>> {
    let Some(agent) = agent else { return vec![None; urls.len()] };
    let results: Vec<Option<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = urls
            .iter()
            .map(|url| {
                scope.spawn(move || {
                    let resp = net::fetch(agent, url, net::MAX_CSS_BYTES).ok()?;
                    Some(net::decode_text(&resp.body, resp.charset.as_deref(), false))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    let mut total = 0;
    results
        .into_iter()
        .map(|text| {
            let text = text?;
            total += text.len();
            (total <= MAX_TOTAL_CSS_BYTES).then_some(text)
        })
        .collect()
}

pub fn error_page(url: &Url, message: &str) -> Loaded {
    let body = format!(
        "<h1>Can't open this page</h1><p class=\"url\">{}</p>{}",
        escape_html(url.as_str()),
        message.lines().map(|l| format!("<p>{}</p>", escape_html(l))).collect::<String>()
    );
    from_html(url.clone(), PageKind::Error, &internal_page("Can't open this page", &body), None)
}

pub fn home_page() -> Loaded {
    let body = r#"
<h1>SafeBrowser</h1>
<p>A browser written from scratch in Rust, with a sandboxed JavaScript interpreter.</p>
<h2>Security by design</h2>
<ul>
<li>The engine is written in Rust with unsafe code forbidden (memory safe)</li>
<li>JavaScript runs once while the page loads, with no network, cookies or navigation available to scripts</li>
<li>HTTPS only: http:// is upgraded to https:// automatically</li>
<li>No cookies are stored or sent, and no Referer is sent</li>
<li>Images (PNG, JPEG, GIF, WebP, BMP) are fetched over HTTPS without cookies or Referer, decoded with size limits, and alt text is shown when they fail</li>
<li>No downloads</li>
<li>Hover a link to see its real destination in the status bar</li>
<li>Internationalized domains are shown in xn-- form, so lookalikes stand out</li>
</ul>
<h2>Try it</h2>
<ul>
<li><a href="https://example.com/">https://example.com/</a></li>
<li><a href="https://en.wikipedia.org/wiki/Rust_(programming_language)">Wikipedia: Rust (programming language)</a></li>
<li><a href="https://html.duckduckgo.com/html/?q=rust">Search DuckDuckGo for "rust"</a></li>
</ul>
<h2>Keys</h2>
<p>⌘L: address bar / ⌘R: reload / ⌘[ ⌘]: back, forward / Space, arrow keys: scroll / ⌘Q: quit</p>
"#;
    from_html(Url::parse(HOME_URL).expect("valid url"), PageKind::Internal, &internal_page("SafeBrowser", body), None)
}

fn internal_page(title: &str, body: &str) -> String {
    format!(
        "<html><head><title>{}</title><style>
body {{ max-width: 720px; margin: 48px auto; padding: 0 24px; color: #222 }}
h1 {{ font-size: 28px; color: #0b57d0 }}
.url {{ color: #666 }}
</style></head><body>{body}</body></html>",
        escape_html(title)
    )
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod network_tests {
    //! These hit the real network; run with `cargo test -- --ignored`.
    use std::collections::HashMap;

    use super::*;
    use crate::dom::Element;

    fn by_name<'a>(node: &'a Node, name: &str) -> Option<&'a Element> {
        let Node::Element(el) = node else { return None };
        if el.attr("name") == Some(name) {
            return Some(el);
        }
        el.children.iter().find_map(|c| by_name(c, name))
    }

    fn search(start: &str, field: &str, query: &str) -> Loaded {
        let agent = net::agent();
        let page = load(&agent, &Request::Get(Url::parse(start).unwrap()));
        assert_eq!(page.kind, PageKind::Secure, "{start} loaded");
        let input = by_name(&page.dom, field).expect("search field").id();
        let values = HashMap::from([(input, query.to_string())]);
        let request = crate::form::submit(&page.dom, &page.base, input, &values).expect("inside a form").expect("submittable");
        load(&agent, &request)
    }

    #[test]
    #[ignore]
    fn wikipedia_search_get() {
        let result = search("https://en.wikipedia.org/wiki/Main_Page", "search", "Rust programming language");
        assert_eq!(result.kind, PageKind::Secure);
        assert!(result.title.contains("Rust"), "{}", result.title);
    }

    #[test]
    #[ignore]
    fn duckduckgo_search_post() {
        let result = search("https://html.duckduckgo.com/html/", "q", "rust lang");
        assert_eq!(result.kind, PageKind::Secure);
        assert!(result.title.to_lowercase().contains("rust"), "{}", result.title);
    }
}
