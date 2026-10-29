//! CSS selectors: parsing and matching.
//!
//! Matching runs right to left. Selectors that mix child or sibling combinators with
//! descendant combinators need backtracking; those are matched with memoization so a
//! hostile stylesheet cannot trigger exponential work. Unsupported syntax drops the whole
//! selector rather than risking a wrong match.

use std::collections::HashMap;

use crate::dom::{Element, Node};

const MAX_COMPOUNDS: usize = 8;
/// Nesting limit for :not() / :is() / :where().
const MAX_NESTING: usize = 3;

#[derive(Debug, Clone)]
pub struct Selector {
    pub parts: Vec<Compound>,
    /// `combinators[i]` joins `parts[i]` and `parts[i + 1]`.
    combinators: Vec<Combinator>,
    pub specificity: u32,
    /// Set when the selector targets a ::before / ::after pseudo-element.
    pub pseudo: Option<PseudoElement>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PseudoElement {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Combinator {
    Descendant,
    Child,
    NextSibling,
    SubsequentSibling,
}

#[derive(Debug, Clone, Default)]
pub struct Compound {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    attrs: Vec<AttrSelector>,
    pseudos: Vec<PseudoClass>,
}

#[derive(Debug, Clone)]
struct AttrSelector {
    name: String,
    op: AttrOp,
    value: String,
    ignore_case: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AttrOp {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Debug, Clone)]
enum PseudoClass {
    Root,
    Empty,
    Link,
    Checked,
    Disabled,
    Enabled,
    /// Interaction and history states (:hover, :focus, :visited, ...). There is no
    /// interaction state here, and :visited must never leak history, so these never match.
    Never,
    Nth { a: i32, b: i32, from_end: bool, of_type: bool },
    Only { of_type: bool },
    Not(Vec<Compound>),
    Is(Vec<Compound>),
}

pub fn parse_selector_list(prelude: &str) -> Vec<Selector> {
    split_commas(prelude).into_iter().filter_map(parse_selector).collect()
}

fn parse_selector(s: &str) -> Option<Selector> {
    let mut p = Parser::new(s);
    let (mut parts, mut combinators) = (Vec::new(), Vec::new());
    let mut pseudo = None;
    let mut specificity = 0;
    p.skip_ws();
    loop {
        let (compound, spec) = p.compound(0, &mut pseudo)?;
        specificity += spec;
        parts.push(compound);
        let had_space = p.skip_ws();
        if p.eof() {
            break;
        }
        if pseudo.is_some() {
            return None; // a pseudo-element must end the selector
        }
        let combinator = match p.peek() {
            Some('>') => Combinator::Child,
            Some('+') => Combinator::NextSibling,
            Some('~') => Combinator::SubsequentSibling,
            _ if had_space => Combinator::Descendant,
            _ => return None,
        };
        if combinator != Combinator::Descendant {
            p.pos += 1;
            p.skip_ws();
        }
        combinators.push(combinator);
        if parts.len() >= MAX_COMPOUNDS {
            return None;
        }
    }
    if pseudo.is_some() {
        specificity += 1;
    }
    Some(Selector { parts, combinators, specificity, pseudo })
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(s: &str) -> Self {
        Parser { chars: s.chars().collect(), pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eof(&self) -> bool {
        self.pos >= self.chars.len()
    }

    /// Skips whitespace; returns whether any was skipped.
    fn skip_ws(&mut self) -> bool {
        let start = self.pos;
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
        self.pos > start
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        if found {
            self.pos += 1;
        }
        found
    }

    fn at_ident_start(&self) -> bool {
        self.peek().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '-' || c == '\\' || !c.is_ascii())
    }

    /// An identifier, decoding escapes such as `md\:flex` or `\31 0`.
    fn ident(&mut self) -> Option<String> {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == '\\' {
                self.pos += 1;
                out.push(self.escape()?);
            } else if c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() {
                out.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// The character after a backslash: up to six hex digits (plus one optional space), or a literal.
    fn escape(&mut self) -> Option<char> {
        let mut hex = String::new();
        while hex.len() < 6 && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            hex.push(self.peek()?);
            self.pos += 1;
        }
        if hex.is_empty() {
            let c = self.peek()?;
            self.pos += 1;
            return Some(c);
        }
        if self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
        let c = char::from_u32(u32::from_str_radix(&hex, 16).ok()?).filter(|&c| c != '\0');
        Some(c.unwrap_or('\u{FFFD}'))
    }

    fn string(&mut self, quote: char) -> Option<String> {
        let mut out = String::new();
        loop {
            match self.peek()? {
                c if c == quote => {
                    self.pos += 1;
                    return Some(out);
                }
                '\\' => {
                    self.pos += 1;
                    out.push(self.escape()?);
                }
                c => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// The text inside balanced parentheses, starting at '('.
    fn parenthesized(&mut self) -> Option<String> {
        if !self.eat('(') {
            return None;
        }
        let start = self.pos;
        let mut depth = 1;
        let mut quote: Option<char> = None;
        while let Some(c) = self.peek() {
            self.pos += 1;
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(c),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(self.chars[start..self.pos - 1].iter().collect());
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// A compound selector such as `a.b#c[d]:first-child`; returns it with its specificity.
    fn compound(&mut self, depth: usize, pseudo: &mut Option<PseudoElement>) -> Option<(Compound, u32)> {
        let mut c = Compound::default();
        let mut spec = 0;
        let start = self.pos;
        if !self.eat('*') && self.at_ident_start() {
            c.tag = Some(self.ident()?.to_ascii_lowercase());
            spec += 1;
        }
        loop {
            match self.peek() {
                Some('.') => {
                    self.pos += 1;
                    c.classes.push(self.ident()?);
                    spec += 100;
                }
                Some('#') => {
                    self.pos += 1;
                    if c.id.replace(self.ident()?).is_some() {
                        return None;
                    }
                    spec += 10_000;
                }
                Some('[') => {
                    self.pos += 1;
                    c.attrs.push(self.attribute()?);
                    spec += 100;
                }
                Some(':') => {
                    self.pos += 1;
                    let double = self.eat(':');
                    let name = self.ident()?.to_ascii_lowercase();
                    if double || matches!(name.as_str(), "before" | "after") {
                        let element = match name.as_str() {
                            "before" => PseudoElement::Before,
                            "after" => PseudoElement::After,
                            _ => return None, // ::marker, ::placeholder, ...
                        };
                        if pseudo.replace(element).is_some() {
                            return None;
                        }
                        continue;
                    }
                    let arg = if self.peek() == Some('(') { Some(self.parenthesized()?) } else { None };
                    let (class, class_spec) = pseudo_class(&name, arg.as_deref(), depth)?;
                    c.pseudos.extend(class);
                    spec += class_spec;
                }
                _ => break,
            }
            if pseudo.is_some() {
                return None; // nothing may follow a pseudo-element
            }
        }
        (self.pos > start).then_some((c, spec))
    }

    /// `[name]`, `[name=value]`, `[name^="value" i]`, ... starting after '['.
    fn attribute(&mut self) -> Option<AttrSelector> {
        self.skip_ws();
        let name = self.ident()?.to_ascii_lowercase();
        self.skip_ws();
        if self.eat(']') {
            return Some(AttrSelector { name, op: AttrOp::Exists, value: String::new(), ignore_case: false });
        }
        let op = match self.peek()? {
            '=' => AttrOp::Equals,
            '~' => AttrOp::Includes,
            '|' => AttrOp::DashMatch,
            '^' => AttrOp::Prefix,
            '$' => AttrOp::Suffix,
            '*' => AttrOp::Substring,
            _ => return None,
        };
        self.pos += 1;
        if op != AttrOp::Equals && !self.eat('=') {
            return None;
        }
        self.skip_ws();
        let value = match self.peek()? {
            q @ ('"' | '\'') => {
                self.pos += 1;
                self.string(q)?
            }
            _ => self.ident()?,
        };
        self.skip_ws();
        let ignore_case = match self.peek()? {
            'i' | 'I' => {
                self.pos += 1;
                true
            }
            's' | 'S' => {
                self.pos += 1;
                false
            }
            _ => false,
        };
        self.skip_ws();
        self.eat(']').then_some(AttrSelector { name, op, value, ignore_case })
    }
}

/// Maps a pseudo-class to its matcher and specificity. None means unsupported, which
/// drops the selector. A matcher of None means "always matches".
fn pseudo_class(name: &str, arg: Option<&str>, depth: usize) -> Option<(Option<PseudoClass>, u32)> {
    let class = |p: PseudoClass| Some((Some(p), 100));
    let nth = |a: i32, b: i32, from_end: bool, of_type: bool| class(PseudoClass::Nth { a, b, from_end, of_type });
    match (name, arg) {
        ("root", None) => class(PseudoClass::Root),
        ("empty", None) => class(PseudoClass::Empty),
        ("link" | "any-link", None) => class(PseudoClass::Link),
        ("checked", None) => class(PseudoClass::Checked),
        ("disabled", None) => class(PseudoClass::Disabled),
        ("enabled", None) => class(PseudoClass::Enabled),
        ("first-child", None) => nth(0, 1, false, false),
        ("last-child", None) => nth(0, 1, true, false),
        ("first-of-type", None) => nth(0, 1, false, true),
        ("last-of-type", None) => nth(0, 1, true, true),
        ("only-child", None) => class(PseudoClass::Only { of_type: false }),
        ("only-of-type", None) => class(PseudoClass::Only { of_type: true }),
        ("nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type", Some(arg)) => {
            let (a, b) = parse_nth(arg)?;
            nth(a, b, name.contains("last"), name.ends_with("of-type"))
        }
        ("not" | "is" | "where" | "matches", Some(arg)) if depth < MAX_NESTING => {
            let list = parse_compound_list(arg, depth + 1)?;
            let spec = if name == "where" { 0 } else { list.iter().map(|(_, s)| *s).max().unwrap_or(0) };
            let compounds = list.into_iter().map(|(c, _)| c).collect();
            let p = if name == "not" { PseudoClass::Not(compounds) } else { PseudoClass::Is(compounds) };
            Some((Some(p), spec))
        }
        (
            "hover" | "active" | "focus" | "focus-visible" | "focus-within" | "visited" | "target" | "target-within"
            | "placeholder-shown" | "autofill" | "invalid" | "user-invalid" | "indeterminate" | "fullscreen" | "modal"
            | "popover-open" | "open" | "playing" | "paused",
            None,
        ) => class(PseudoClass::Never),
        ("lang" | "dir" | "has", Some(_)) => class(PseudoClass::Never),
        ("defined" | "scope", None) => Some((None, 100)),
        _ => None,
    }
}

/// The argument of :not() / :is(): a comma-separated list of compound selectors.
fn parse_compound_list(arg: &str, depth: usize) -> Option<Vec<(Compound, u32)>> {
    split_commas(arg)
        .into_iter()
        .map(|part| {
            let mut p = Parser::new(part);
            let mut pseudo = None;
            p.skip_ws();
            let compound = p.compound(depth, &mut pseudo)?;
            p.skip_ws();
            // Complex selectors (with combinators) and pseudo-elements are not supported here
            (p.eof() && pseudo.is_none()).then_some(compound)
        })
        .collect()
}

/// `an+b` syntax: "odd", "even", "3", "2n+1", "-n+3", ...
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let lower = arg.to_ascii_lowercase();
    if lower.split_whitespace().any(|w| w == "of") {
        return None;
    }
    let s: String = lower.split_whitespace().collect();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let Some(n) = s.find('n') else { return Some((0, s.parse().ok()?)) };
    let a = match &s[..n] {
        "" | "+" => 1,
        "-" => -1,
        a => a.parse().ok()?,
    };
    let b = match &s[n + 1..] {
        "" => 0,
        b => b.parse().ok()?,
    };
    Some((a, b))
}

/// Splits on commas outside parentheses, brackets and quotes.
fn split_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Whether `sel` matches `el`, whose ancestors (root first) are `ancestors`.
pub fn matches(sel: &Selector, el: &Element, ancestors: &[&Element]) -> bool {
    if sel.combinators.iter().all(|c| *c == Combinator::Descendant) {
        return matches_descendants_only(sel, el, ancestors);
    }
    let mut memo = HashMap::new();
    match_at(sel, sel.parts.len() - 1, el, ancestors.len(), ancestors, &mut memo)
}

/// With only descendant combinators, matching each part against the nearest fitting
/// ancestor is always correct, so no backtracking is needed.
fn matches_descendants_only(sel: &Selector, el: &Element, ancestors: &[&Element]) -> bool {
    let Some((last, rest)) = sel.parts.split_last() else { return false };
    if !compound_matches(last, el, ancestors.last().copied()) {
        return false;
    }
    let mut depth = ancestors.len();
    for part in rest.iter().rev() {
        loop {
            if depth == 0 {
                return false;
            }
            depth -= 1;
            let parent = depth.checked_sub(1).map(|p| ancestors[p]);
            if compound_matches(part, ancestors[depth], parent) {
                break;
            }
        }
    }
    true
}

/// Matches `sel.parts[..=i]` with `el` (at `depth`, i.e. `ancestors[..depth]` are its
/// ancestors) as the element for part `i`. Results are memoized per (part, element).
fn match_at(sel: &Selector, i: usize, el: &Element, depth: usize, ancestors: &[&Element], memo: &mut HashMap<(usize, usize), bool>) -> bool {
    let key = (i, el as *const Element as usize);
    if let Some(&hit) = memo.get(&key) {
        return hit;
    }
    let parent = depth.checked_sub(1).map(|p| ancestors[p]);
    let result = compound_matches(&sel.parts[i], el, parent)
        && (i == 0
            || match sel.combinators[i - 1] {
                Combinator::Descendant => (0..depth).rev().any(|d| match_at(sel, i - 1, ancestors[d], d, ancestors, memo)),
                Combinator::Child => depth > 0 && match_at(sel, i - 1, ancestors[depth - 1], depth - 1, ancestors, memo),
                Combinator::NextSibling => parent
                    .and_then(|p| element_children(p).take_while(|s| !std::ptr::eq(*s, el)).last())
                    .is_some_and(|s| match_at(sel, i - 1, s, depth, ancestors, memo)),
                Combinator::SubsequentSibling => parent.is_some_and(|p| {
                    let earlier: Vec<&Element> = element_children(p).take_while(|s| !std::ptr::eq(*s, el)).collect();
                    earlier.into_iter().any(|s| match_at(sel, i - 1, s, depth, ancestors, memo))
                }),
            });
    memo.insert(key, result);
    result
}

fn element_children(parent: &Element) -> impl Iterator<Item = &Element> {
    parent.children.iter().filter_map(|n| match n {
        Node::Element(e) => Some(e),
        Node::Text(_) => None,
    })
}

fn compound_matches(c: &Compound, el: &Element, parent: Option<&Element>) -> bool {
    c.tag.as_ref().is_none_or(|t| *t == el.tag)
        && c.id.as_ref().is_none_or(|id| el.attr("id") == Some(id))
        && c.classes.iter().all(|cls| el.has_class(cls))
        && c.attrs.iter().all(|a| attr_matches(a, el))
        && c.pseudos.iter().all(|p| pseudo_matches(p, el, parent))
}

fn attr_matches(a: &AttrSelector, el: &Element) -> bool {
    let Some(value) = el.attr(&a.name) else { return false };
    let (value, want) = if a.ignore_case {
        (value.to_lowercase(), a.value.to_lowercase())
    } else {
        (value.to_string(), a.value.clone())
    };
    match a.op {
        AttrOp::Exists => true,
        AttrOp::Equals => value == want,
        AttrOp::Includes => value.split_ascii_whitespace().any(|v| v == want),
        AttrOp::DashMatch => value == want || value.starts_with(&format!("{want}-")),
        AttrOp::Prefix => !want.is_empty() && value.starts_with(&want),
        AttrOp::Suffix => !want.is_empty() && value.ends_with(&want),
        AttrOp::Substring => !want.is_empty() && value.contains(&want),
    }
}

fn pseudo_matches(p: &PseudoClass, el: &Element, parent: Option<&Element>) -> bool {
    match p {
        PseudoClass::Root => el.tag == "html",
        PseudoClass::Empty => el.children.iter().all(|c| matches!(c, Node::Text(t) if t.trim().is_empty())),
        PseudoClass::Link => matches!(el.tag.as_str(), "a" | "area") && el.attr("href").is_some(),
        PseudoClass::Checked => el.attr("checked").is_some() || el.attr("selected").is_some(),
        PseudoClass::Disabled => el.attr("disabled").is_some(),
        PseudoClass::Enabled => {
            matches!(el.tag.as_str(), "input" | "button" | "select" | "textarea" | "option") && el.attr("disabled").is_none()
        }
        PseudoClass::Never => false,
        PseudoClass::Nth { a, b, from_end, of_type } => {
            let (index, count) = sibling_position(el, parent, *of_type);
            let pos = if *from_end { count - index } else { index + 1 } as i32;
            if *a == 0 {
                pos == *b
            } else {
                let d = pos - b;
                d % a == 0 && d / a >= 0
            }
        }
        PseudoClass::Only { of_type } => sibling_position(el, parent, *of_type).1 == 1,
        PseudoClass::Not(list) => !list.iter().any(|c| compound_matches(c, el, parent)),
        PseudoClass::Is(list) => list.iter().any(|c| compound_matches(c, el, parent)),
    }
}

/// (0-based index, count) of `el` among its parent's element children (of the same type if asked).
fn sibling_position(el: &Element, parent: Option<&Element>, of_type: bool) -> (usize, usize) {
    let Some(parent) = parent else { return (0, 1) };
    let mut index = 0;
    let mut count = 0;
    for sibling in element_children(parent).filter(|s| !of_type || s.tag == el.tag) {
        if std::ptr::eq(sibling, el) {
            index = count;
        }
        count += 1;
    }
    (index, count.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    /// Finds the element with `id` and returns whether `selector` matches it.
    fn check(markup: &str, id: &str, selector: &str) -> bool {
        fn find<'a>(node: &'a Node, id: &str, path: &mut Vec<&'a Element>) -> Option<&'a Element> {
            let Node::Element(el) = node else { return None };
            if el.attr("id") == Some(id) {
                return Some(el);
            }
            path.push(el);
            for child in &el.children {
                if let Some(found) = find(child, id, path) {
                    return Some(found);
                }
            }
            path.pop();
            None
        }
        let dom = html::parse(markup);
        let mut ancestors = Vec::new();
        let el = find(&dom, id, &mut ancestors).expect("element");
        let sels = parse_selector_list(selector);
        assert_eq!(sels.len(), 1, "selector should parse: {selector}");
        matches(&sels[0], el, &ancestors)
    }

    const LIST: &str = "<ul id=u><li id=a class=x>1</li><li id=b lang=en-US>2</li><li id=c data-k='Foo Bar'>3</li></ul>";

    #[test]
    fn combinators() {
        assert!(check(LIST, "b", "ul > li"));
        assert!(!check("<div><ul><li id=b></li></ul></div>", "b", "div > li"));
        assert!(check(LIST, "b", ".x + li"));
        assert!(!check(LIST, "c", ".x + li"));
        assert!(check(LIST, "c", ".x ~ li"));
        assert!(check("<div class=p><section><ul><li id=t></li></ul></section></div>", "t", ".p section > ul li"));
    }

    #[test]
    fn structural_pseudo_classes() {
        assert!(check(LIST, "a", "li:first-child"));
        assert!(check(LIST, "c", "li:last-child"));
        assert!(check(LIST, "b", "li:nth-child(2n)"));
        assert!(check(LIST, "c", "li:nth-child(odd)"));
        assert!(check(LIST, "a", "li:nth-last-child(3)"));
        assert!(check(LIST, "b", "li:not(:first-child):not(:last-child)"));
        assert!(check(LIST, "b", "li:is(.x, [lang])"));
        assert!(check("<p id=e></p>", "e", "p:empty"));
    }

    #[test]
    fn attributes() {
        assert!(check(LIST, "b", "[lang|=en]"));
        assert!(check(LIST, "c", "[data-k~=Bar]"));
        assert!(check(LIST, "c", "[data-k^='foo' i]"));
        assert!(!check(LIST, "c", "[data-k^='foo']"));
        assert!(check(LIST, "c", "[data-k$=ar][data-k*='o B']"));
    }

    #[test]
    fn states_and_escapes() {
        assert!(!check("<a id=l href=/>x</a>", "l", "a:hover"));
        assert!(!check("<a id=l href=/>x</a>", "l", "a:visited"));
        assert!(check("<a id=l href=/>x</a>", "l", "a:link"));
        assert!(check("<input id=i type=checkbox checked>", "i", "input:checked"));
        assert!(check("<div id=t class='md:flex'></div>", "t", ".md\\:flex"));
        assert!(parse_selector_list("p::placeholder").is_empty());
        assert!(parse_selector_list("a:unknown-thing").is_empty());
    }

    #[test]
    fn specificity() {
        let spec = |s: &str| parse_selector_list(s)[0].specificity;
        assert_eq!(spec("li:first-child"), 101);
        assert_eq!(spec(":where(#a) p"), 1);
        assert_eq!(spec(":is(#a, .b) p"), 10_001);
        assert_eq!(spec("a::before"), 2);
    }

    #[test]
    fn backtracking_is_bounded() {
        // Mixed combinators force backtracking; memoization keeps this fast on a deep tree
        let markup = format!("{}<p id=t></p>", "<div>".repeat(200));
        let start = std::time::Instant::now();
        for _ in 0..50 {
            assert!(!check(&markup, "t", "span div > div div > div div > div p"));
        }
        assert!(start.elapsed().as_secs() < 2);
    }
}
