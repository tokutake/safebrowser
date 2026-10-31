//! HTTPS networking (rustls). Redirects are followed manually and each hop goes through policy::check.
//! No cookies are stored or sent, and no Referer header is sent.

use std::io::Read;
use std::time::Duration;

use url::Url;

use crate::policy;

pub const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CSS_BYTES: u64 = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;
const USER_AGENT: &str = "SafeBrowser/0.1";
const ACCEPT_DOCUMENT: &str = "text/html,application/xhtml+xml,text/css;q=0.9,text/plain;q=0.8";
pub const ACCEPT_IMAGE: &str = "image/png,image/jpeg,image/gif,image/webp,image/bmp;q=0.9";
pub const ACCEPT_SCRIPT: &str = "application/javascript,text/javascript;q=0.9,*/*;q=0.1";

pub struct Response {
    pub url: Url,
    pub content_type: String,
    pub charset: Option<String>,
    pub body: Vec<u8>,
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
}

pub fn fetch(agent: &ureq::Agent, url: &Url, limit: u64) -> Result<Response, String> {
    fetch_with_form(agent, url, None, limit)
}

/// Like `fetch`, but with an `Accept` header suited to subresources (images, scripts).
pub fn fetch_accepting(agent: &ureq::Agent, url: &Url, limit: u64, accept: &str) -> Result<Response, String> {
    fetch_request(agent, url, None, limit, accept)
}

/// Like `fetch`, but sends `form` as an urlencoded POST body on the first request.
/// Redirects are followed with GET, as browsers do for 301/302/303.
pub fn fetch_with_form(agent: &ureq::Agent, url: &Url, form: Option<&[(String, String)]>, limit: u64) -> Result<Response, String> {
    fetch_request(agent, url, form, limit, ACCEPT_DOCUMENT)
}

fn fetch_request(agent: &ureq::Agent, url: &Url, form: Option<&[(String, String)]>, limit: u64, accept: &str) -> Result<Response, String> {
    let mut current = policy::check(url)?;
    let mut form = form;
    for _ in 0..=MAX_REDIRECTS {
        let request = agent
            .request_url(if form.is_some() { "POST" } else { "GET" }, &current)
            .set("Accept", accept)
            .set("Accept-Language", "en-US,en;q=0.9");
        let result = match form.take() {
            Some(pairs) => {
                let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                request.send_form(&pairs)
            }
            None => request.call(),
        };
        let resp = match result {
            Ok(r) => r,
            // Still show the body for 404 and other error statuses
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(describe_transport_error(&t.to_string())),
        };
        if (300..400).contains(&resp.status()) {
            let location = resp.header("location").ok_or("The redirect has no target.")?;
            let next = current.join(location).map_err(|_| "The redirect target is not a valid URL.".to_string())?;
            current = policy::check(&next)?;
            continue;
        }
        let content_type = resp.content_type().to_ascii_lowercase();
        let charset = resp.header("content-type").and_then(charset_param);
        let mut body = Vec::new();
        resp.into_reader()
            .take(limit + 1)
            .read_to_end(&mut body)
            .map_err(|e| format!("Error while receiving data: {e}"))?;
        if body.len() as u64 > limit {
            return Err(format!("The response is too large (limit {} MB).", limit / 1024 / 1024));
        }
        return Ok(Response { url: current, content_type, charset, body });
    }
    Err("Too many redirects.".into())
}

fn describe_transport_error(msg: &str) -> String {
    if msg.contains("certificate") || msg.contains("Certificate") {
        format!("The server's certificate could not be verified. Someone may be intercepting the connection, so it was stopped.\n({msg})")
    } else {
        format!("Could not connect. The site may not support HTTPS.\n({msg})")
    }
}

fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Detects the encoding and decodes (HTTP header, then <meta charset>, then UTF-8).
pub fn decode_text(body: &[u8], header_charset: Option<&str>, sniff_html: bool) -> String {
    let label = header_charset.map(str::to_string).or_else(|| if sniff_html { sniff_meta_charset(body) } else { None });
    let encoding = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.trim().as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(body).0.into_owned()
}

fn sniff_meta_charset(body: &[u8]) -> Option<String> {
    let head = body[..body.len().min(4096)].to_ascii_lowercase();
    let pos = head.windows(8).position(|w| w == b"charset=")? + 8;
    let label: String = head[pos..]
        .iter()
        .skip_while(|&&b| b == b'"' || b == b'\'')
        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        .map(|&b| b as char)
        .collect();
    (!label.is_empty()).then_some(label)
}
