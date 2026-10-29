//! CSS parser (subset): rules, declarations, @media / @supports / @layer.
//! Selectors are handled by the `selector` module.

use std::sync::Arc;

use crate::selector::{Selector, parse_selector_list};

/// Viewport width assumed for `vw` units.
pub const VIEWPORT_WIDTH: f32 = 1024.0;
const MAX_RULES: usize = 50_000;
const MAX_NESTING: usize = 4;

#[derive(Debug, Clone)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    /// Enclosing @media queries; all must match. Evaluated at style time against the real
    /// window width, so layouts switch as the window is resized.
    pub media: Option<Arc<[String]>>,
}

impl Rule {
    pub fn applies_at(&self, viewport_width: f32) -> bool {
        self.media.as_ref().is_none_or(|m| m.iter().all(|q| media_matches(q, Some(viewport_width))))
    }
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

pub fn parse_stylesheet(src: &str) -> Stylesheet {
    let src = strip_comments(src);
    let mut rules = Vec::new();
    parse_rules(&src, &mut rules, 0, None);
    Stylesheet { rules }
}

fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        rest = match rest[i + 2..].find("*/") {
            Some(j) => &rest[i + 2 + j + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn parse_rules(src: &str, out: &mut Vec<Rule>, depth: usize, media: Option<&Arc<[String]>>) {
    let mut rest = src;
    while out.len() < MAX_RULES {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let brace = rest.find('{');
        // Skip statements such as `@import ...;`
        if let Some(semi) = rest.find(';') {
            if brace.is_none_or(|b| semi < b) {
                rest = &rest[semi + 1..];
                continue;
            }
        }
        let Some(open) = brace else { break };
        let close = matching_brace(rest, open);
        let prelude = rest[..open].trim();
        let block = &rest[open + 1..close];
        rest = rest.get(close + 1..).unwrap_or("");

        if let Some(at) = prelude.strip_prefix('@') {
            if depth >= MAX_NESTING {
                continue;
            }
            let at = at.to_ascii_lowercase();
            if let Some(query) = at.strip_prefix("media") {
                // Drop queries that can never match (print, speech, ...) right away
                if media_matches(query, None) {
                    let mut queries: Vec<String> = media.map(|m| m.to_vec()).unwrap_or_default();
                    queries.push(query.trim().to_string());
                    parse_rules(block, out, depth + 1, Some(&Arc::from(queries)));
                }
            } else if at.starts_with("supports") || at.starts_with("layer") {
                parse_rules(block, out, depth + 1, media);
            }
            continue;
        }
        let selectors = parse_selector_list(prelude);
        if !selectors.is_empty() {
            out.push(Rule { selectors, declarations: parse_declarations(block), media: media.cloned() });
        }
    }
}

/// Position of the '}' matching the '{' at `open`, or the end of input if unmatched.
fn matching_brace(s: &str, open: usize) -> usize {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, c) in s[open..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth == 0 {
                    return open + i;
                }
            }
            _ => {}
        }
    }
    s.len()
}

/// Evaluates a media query for a screen in light mode. With `viewport_width` = None,
/// width conditions are ignored (used to discard queries that can never match).
pub fn media_matches(query: &str, viewport_width: Option<f32>) -> bool {
    query.split(',').any(|part| {
        let p = part.trim();
        if p.starts_with("not ") || p.contains("speech") || (p.contains("print") && !p.contains("screen")) {
            return false;
        }
        p.split('(').skip(1).all(|cond| {
            let cond = cond.split(')').next().unwrap_or("");
            let Some((feature, value)) = cond.split_once(':') else { return true };
            let value = value.trim();
            let px = || match parse_length(value, 16.0) {
                Some(crate::style::Len::Px(n)) => Some(n),
                _ => None,
            };
            match feature.trim() {
                "min-width" => viewport_width.is_none_or(|w| px().is_none_or(|n| w >= n)),
                "max-width" => viewport_width.is_none_or(|w| px().is_none_or(|n| w <= n)),
                "prefers-color-scheme" => value == "light",
                "orientation" => value == "landscape",
                _ => true,
            }
        })
    })
}

pub fn parse_declarations(block: &str) -> Vec<Declaration> {
    split_top_level(block, ';')
        .into_iter()
        .filter_map(|part| {
            let (name, value) = part.split_once(':')?;
            let name = name.trim();
            let mut value = value.trim();
            if name.is_empty() || value.is_empty() {
                return None;
            }
            let mut important = false;
            if let Some(bang) = value.rfind('!') {
                if value[bang + 1..].trim().eq_ignore_ascii_case("important") {
                    important = true;
                    value = value[..bang].trim();
                }
            }
            // Custom property names are case-sensitive
            let name = if name.starts_with("--") { name.to_string() } else { name.to_ascii_lowercase() };
            Some(Declaration { name, value: value.to_string(), important })
        })
        .collect()
}

/// Splits on separators outside parentheses and quotes.
pub fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, c) if depth <= 0 && (c == sep || (sep == ' ' && c.is_whitespace())) => {
                if i > start {
                    parts.push(&s[start..i]);
                }
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Parses a length; em is relative to `em_base`.
pub fn parse_length(v: &str, em_base: f32) -> Option<crate::style::Len> {
    use crate::style::Len;
    let v = v.trim();
    if v == "auto" {
        return Some(Len::Auto);
    }
    if v == "0" {
        return Some(Len::Px(0.0));
    }
    // Approximate clamp() / min() / max() by their first argument
    for f in ["clamp(", "min(", "max("] {
        if let Some(inner) = v.strip_prefix(f).and_then(|r| r.strip_suffix(')')) {
            return split_top_level(inner, ',').into_iter().find_map(|a| parse_length(a, em_base));
        }
    }
    if let Some(inner) = v.strip_prefix("calc(").and_then(|r| r.strip_suffix(')')) {
        return parse_length(inner, em_base);
    }
    let units: [(&str, f32); 7] = [
        ("rem", 16.0),
        ("em", em_base),
        ("px", 1.0),
        ("pt", 4.0 / 3.0),
        ("vw", VIEWPORT_WIDTH / 100.0),
        ("vh", 7.68),
        ("%", f32::NAN),
    ];
    for (unit, scale) in units {
        if let Some(num) = v.strip_suffix(unit) {
            let n: f32 = num.trim().parse().ok()?;
            if !n.is_finite() {
                return None;
            }
            return Some(if scale.is_nan() { Len::Percent(n) } else { Len::Px(n * scale) });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_and_skips_unsupported() {
        let sheet = parse_stylesheet(
            "/* c */ h1, .a > p#b { color: red !important; margin: 0 }
             a:hover { color: blue }
             p::placeholder { color: gray }
             @media print { p { color: green } }
             @media (min-width: 600px) { p { color: navy } }
             @font-face { font-family: x }",
        );
        assert_eq!(sheet.rules.len(), 3);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert_eq!(sheet.rules[0].selectors[1].specificity, 10_101);
        assert!(sheet.rules[0].declarations[0].important);
        let last = &sheet.rules[2];
        assert_eq!(last.declarations[0].value, "navy");
        assert!(last.applies_at(1024.0));
        assert!(!last.applies_at(500.0));
    }

    #[test]
    fn values_with_semicolons_in_parens() {
        let decls = parse_declarations("background: url(data:a;b); color: red");
        assert_eq!(decls.len(), 2);
    }
}
