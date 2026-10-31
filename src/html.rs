//! HTML parser (a simplified take on the HTML5 algorithm).
//!
//! Defenses against hostile input:
//! - No recursion; nesting depth (MAX_DEPTH) and node count (MAX_NODES) are capped,
//!   so deeply nested markup cannot exhaust the stack.
//! - Character reference lookahead is bounded, so input like "&&&&..." stays linear.
//! - <template>, <svg>, <iframe> and similar elements are dropped with their contents.
//! - <script> is kept as an element holding its source; js.rs decides whether and how to run it.

use crate::dom::{Element, Node};

pub const MAX_DEPTH: usize = 256;
const MAX_NODES: usize = 200_000;
const MAX_ATTRS: usize = 64;

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const RAW_TEXT_TAGS: &[&str] = &["script", "style", "title", "textarea"];
/// Elements dropped together with their contents (embedded content).
const DROPPED_TAGS: &[&str] = &[
    "template", "svg", "math", "iframe", "object", "embed", "canvas", "video", "audio",
    "frameset",
];
/// Start tags that implicitly close an open <p>.
const P_CLOSERS: &[&str] = &[
    "address", "article", "aside", "blockquote", "details", "div", "dl", "dd", "dt", "fieldset",
    "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "li", "main",
    "nav", "ol", "p", "pre", "section", "table", "ul",
];

/// Parses markup that is inserted into an existing element (`innerHTML`).
pub fn parse_fragment(input: &str) -> Vec<Node> {
    match parse(input) {
        Node::Element(doc) => doc.children,
        Node::Text(_) => Vec::new(),
    }
}

pub fn parse(input: &str) -> Node {
    let mut tokenizer = Tokenizer::new(input);
    let mut builder = Builder {
        stack: vec![Element::new("#document", Vec::new())],
        nodes: 0,
        discard_raw: false,
    };
    while let Some(token) = tokenizer.next_token() {
        match token {
            Token::Start { name, attrs } => builder.start(name, attrs),
            Token::End(name) => builder.end(&name),
            Token::Text(text) => builder.text(&text),
            Token::RawText(text) => builder.raw_text(text),
        }
    }
    while builder.stack.len() > 1 {
        builder.pop();
    }
    Node::Element(builder.stack.pop().expect("document root"))
}

enum Token {
    Start { name: String, attrs: Vec<(String, String)> },
    End(String),
    Text(String),
    RawText(String),
}

struct Tokenizer {
    chars: Vec<char>,
    pos: usize,
    raw_tag: Option<String>,
}

impl Tokenizer {
    fn new(input: &str) -> Self {
        Tokenizer { chars: input.chars().collect(), pos: 0, raw_tag: None }
    }

    fn next_token(&mut self) -> Option<Token> {
        if let Some(tag) = self.raw_tag.take() {
            return Some(Token::RawText(self.read_raw(&tag)));
        }
        loop {
            let c = *self.chars.get(self.pos)?;
            if c != '<' {
                return Some(Token::Text(decode_entities(&self.read_text())));
            }
            match self.chars.get(self.pos + 1).copied() {
                Some('!') if self.starts_with_ci(self.pos, "<!--") => {
                    self.pos += 4;
                    self.skip_past("-->");
                }
                Some('!') | Some('?') => {
                    self.pos += 2;
                    self.skip_past(">");
                }
                Some('/') if self.chars.get(self.pos + 2).is_some_and(|c| c.is_ascii_alphabetic()) => {
                    self.pos += 2;
                    let name = self.read_name();
                    self.skip_past(">");
                    return Some(Token::End(name));
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    self.pos += 1;
                    return Some(self.read_start_tag());
                }
                _ => {
                    self.pos += 1;
                    return Some(Token::Text("<".into()));
                }
            }
        }
    }

    fn starts_with_ci(&self, at: usize, pat: &str) -> bool {
        let mut i = at;
        for p in pat.chars() {
            match self.chars.get(i) {
                Some(c) if c.to_ascii_lowercase() == p => i += 1,
                _ => return false,
            }
        }
        true
    }

    fn skip_past(&mut self, pat: &str) {
        let first = pat.chars().next().unwrap_or('>');
        while self.pos < self.chars.len() {
            if self.chars[self.pos] == first && self.starts_with_ci(self.pos, pat) {
                self.pos += pat.chars().count();
                return;
            }
            self.pos += 1;
        }
    }

    fn skip_ws(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn read_while(&mut self, keep: impl Fn(char) -> bool) -> String {
        let start = self.pos;
        while self.chars.get(self.pos).is_some_and(|&c| keep(c)) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn read_text(&mut self) -> String {
        self.read_while(|c| c != '<')
    }

    fn read_name(&mut self) -> String {
        self.read_while(|c| !c.is_whitespace() && c != '/' && c != '>').to_ascii_lowercase()
    }

    fn read_start_tag(&mut self) -> Token {
        let name = self.read_name();
        let mut attrs: Vec<(String, String)> = Vec::new();
        loop {
            self.skip_ws();
            match self.chars.get(self.pos) {
                None => break,
                Some('>') => {
                    self.pos += 1;
                    break;
                }
                Some('/') => self.pos += 1,
                _ => {
                    let attr = self
                        .read_while(|c| !c.is_whitespace() && c != '=' && c != '>' && c != '/')
                        .to_ascii_lowercase();
                    if attr.is_empty() {
                        self.pos += 1; // always make progress
                        continue;
                    }
                    self.skip_ws();
                    let value = if self.chars.get(self.pos) == Some(&'=') {
                        self.pos += 1;
                        self.skip_ws();
                        self.read_attr_value()
                    } else {
                        String::new()
                    };
                    if attrs.len() < MAX_ATTRS && !attrs.iter().any(|(n, _)| *n == attr) {
                        attrs.push((attr, decode_entities(&value)));
                    }
                }
            }
        }
        if RAW_TEXT_TAGS.contains(&name.as_str()) {
            self.raw_tag = Some(name.clone());
        }
        Token::Start { name, attrs }
    }

    fn read_attr_value(&mut self) -> String {
        match self.chars.get(self.pos).copied() {
            Some(q @ ('"' | '\'')) => {
                self.pos += 1;
                let value = self.read_while(|c| c != q);
                self.pos += 1;
                value
            }
            _ => self.read_while(|c| !c.is_whitespace() && c != '>'),
        }
    }

    /// Reads verbatim up to </tag (contents of <script>, <style>, ...).
    fn read_raw(&mut self, tag: &str) -> String {
        let end_pat = format!("</{tag}");
        let start = self.pos;
        while self.pos < self.chars.len() {
            if self.chars[self.pos] == '<' && self.starts_with_ci(self.pos, &end_pat) {
                break;
            }
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }
}

struct Builder {
    stack: Vec<Element>,
    nodes: usize,
    /// Set when a raw text element was not opened due to limits, so its contents are discarded.
    discard_raw: bool,
}

impl Builder {
    fn top(&mut self) -> &mut Element {
        self.stack.last_mut().expect("document root")
    }

    fn start(&mut self, name: String, attrs: Vec<(String, String)>) {
        let is_raw = RAW_TEXT_TAGS.contains(&name.as_str());
        if self.nodes >= MAX_NODES {
            self.discard_raw |= is_raw;
            return;
        }
        while self.stack.len() > 1 && closes(&self.stack[self.stack.len() - 1].tag, &name) {
            self.pop();
        }
        let is_void = VOID_TAGS.contains(&name.as_str());
        if self.stack.len() >= MAX_DEPTH && !is_void {
            // Too deep: skip the element (its children become siblings)
            self.discard_raw |= is_raw;
            return;
        }
        self.nodes += 1;
        let el = Element::new(name, attrs);
        if is_void {
            if !DROPPED_TAGS.contains(&el.tag.as_str()) {
                self.top().children.push(Node::Element(el));
            }
        } else {
            self.stack.push(el);
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            // Keep these open until EOF so content after </body> is still shown
            "html" | "body" => {}
            "br" => self.start("br".into(), Vec::new()),
            _ => {
                if let Some(i) = self.stack.iter().rposition(|e| e.tag == name) {
                    if i > 0 {
                        while self.stack.len() > i {
                            self.pop();
                        }
                    }
                }
            }
        }
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Node::Text(last)) = self.top().children.last_mut() {
            last.push_str(text);
        } else if self.nodes < MAX_NODES {
            self.nodes += 1;
            self.top().children.push(Node::Text(text.to_string()));
        }
    }

    fn raw_text(&mut self, text: String) {
        if std::mem::take(&mut self.discard_raw) {
            return;
        }
        match self.top().tag.as_str() {
            "title" | "textarea" => self.text(&decode_entities(&text)),
            _ => self.text(&text),
        }
    }

    fn pop(&mut self) {
        let Some(el) = self.stack.pop() else { return };
        if !DROPPED_TAGS.contains(&el.tag.as_str()) {
            self.top().children.push(Node::Element(el));
        }
    }
}

/// Whether the open element `top` is implicitly closed by the start tag `new`.
fn closes(top: &str, new: &str) -> bool {
    match top {
        "p" => P_CLOSERS.contains(&new),
        "li" => new == "li",
        "dt" | "dd" => matches!(new, "dt" | "dd"),
        "option" => matches!(new, "option" | "optgroup"),
        "tr" => matches!(new, "tr" | "tbody" | "thead" | "tfoot"),
        "td" | "th" => matches!(new, "td" | "th" | "tr" | "tbody" | "thead" | "tfoot"),
        "thead" | "tbody" | "tfoot" => matches!(new, "thead" | "tbody" | "tfoot"),
        "head" => new == "body",
        _ => false,
    }
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // Look for ';' within 12 chars only, to stay linear on long input
        let semi = rest[1..].char_indices().take(12).find(|&(_, c)| c == ';').map(|(j, _)| j + 1);
        if let Some(c) = semi.and_then(|j| decode_entity(&rest[1..j])) {
            out.push(c);
            rest = &rest[semi.unwrap() + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        return Some(char::from_u32(code).filter(|&c| c != '\0').unwrap_or('\u{FFFD}'));
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "yen" => '¥',
        "euro" => '€',
        "times" => '×',
        "divide" => '÷',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(node: &Node) -> &Element {
        match node {
            Node::Element(e) => e,
            Node::Text(_) => panic!("not an element"),
        }
    }

    #[test]
    fn scripts_are_kept_with_their_source() {
        let doc = parse("<p>a<script>alert('x')</script>b</p>");
        let p = el(&el(&doc).children[0]);
        assert_eq!(p.children.len(), 3);
        let script = el(&p.children[1]);
        assert_eq!(script.tag, "script");
        assert!(matches!(&script.children[0], Node::Text(t) if t == "alert('x')"));
    }

    #[test]
    fn implicit_paragraph_close() {
        let doc = parse("<p>one<p>two");
        assert_eq!(el(&doc).children.len(), 2);
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let input = "<div>".repeat(100_000);
        let doc = parse(&input);
        let mut depth = 0;
        let mut node = &doc;
        while let Some(child) = el(node).children.first() {
            node = child;
            depth += 1;
        }
        assert!(depth < MAX_DEPTH);
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a &amp; b &#x3042; &unknown; &"), "a & b あ &unknown; &");
    }

    #[test]
    fn many_ampersands_are_linear() {
        let s = "&".repeat(1_000_000);
        assert_eq!(decode_entities(&s).len(), s.len());
    }
}
