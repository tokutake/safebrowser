//! Decides which URLs may be opened. Every request (pages, CSS, redirect targets) goes through here.

use url::Url;

/// Search uses DuckDuckGo's HTML version, which works without JavaScript.
const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";

/// Local development hosts allowed over plain HTTP.
pub fn is_local(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
}

/// Returns the URL to fetch: HTTP is upgraded to HTTPS, other unsafe URLs are rejected with a reason.
pub fn check(url: &Url) -> Result<Url, String> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err("This URL contains an embedded username or password, a common trick for disguising phishing sites, so it was blocked.".into());
    }
    match url.scheme() {
        "https" => Ok(url.clone()),
        "http" if is_local(url) => Ok(url.clone()),
        "http" => {
            let mut secure = url.clone();
            secure.set_scheme("https").map_err(|_| "This URL cannot be upgraded to HTTPS.".to_string())?;
            Ok(secure)
        }
        scheme => Err(format!("\"{scheme}:\" URLs cannot be opened for security reasons.")),
    }
}

/// Turns address bar input into a URL, falling back to a search.
pub fn resolve_input(input: &str) -> Option<Url> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if !text.contains(char::is_whitespace) {
        if text.starts_with("localhost") || text.starts_with("127.0.0.1") {
            return Url::parse(&format!("http://{text}")).ok();
        }
        // "example.com:8080" parses with scheme "example.com", so exclude that case
        if let Ok(url) = Url::parse(text) {
            if !url.scheme().contains('.') {
                return Some(url);
            }
        }
        if text.contains('.') {
            if let Ok(url) = Url::parse(&format!("https://{text}")) {
                return Some(url);
            }
        }
    }
    Url::parse_with_params(SEARCH_URL, &[("q", text)]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn http_is_upgraded() {
        assert_eq!(check(&u("http://example.com/a?b")).unwrap().as_str(), "https://example.com/a?b");
    }

    #[test]
    fn localhost_http_is_allowed() {
        assert_eq!(check(&u("http://localhost:3000/")).unwrap().scheme(), "http");
    }

    #[test]
    fn dangerous_schemes_are_blocked() {
        for s in ["javascript:alert(1)", "file:///etc/passwd", "data:text/html,<b>x</b>", "ftp://x.com/"] {
            assert!(check(&u(s)).is_err(), "{s}");
        }
    }

    #[test]
    fn credentials_are_blocked() {
        assert!(check(&u("https://bank.example@evil.example/")).is_err());
    }

    #[test]
    fn idn_hosts_are_shown_as_punycode() {
        // A lookalike domain using Cyrillic "а" is shown as xn--, so it stands out
        assert!(u("https://аpple.com/").as_str().starts_with("https://xn--"));
    }

    #[test]
    fn input_resolution() {
        assert_eq!(resolve_input("example.com").unwrap().as_str(), "https://example.com/");
        assert_eq!(resolve_input("localhost:8080").unwrap().as_str(), "http://localhost:8080/");
        assert_eq!(resolve_input("rust 言語").unwrap().host_str(), Some("html.duckduckgo.com"));
        assert_eq!(resolve_input("javascript:alert(1)").unwrap().scheme(), "javascript");
    }
}
