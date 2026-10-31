//! Cascade (selector matching, specificity, inheritance) and computed styles.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use crate::css::{self, Declaration, Rule, Stylesheet, parse_length, split_top_level};
use crate::selector::{self, PseudoElement, Selector};
use crate::dom::{Element, Node};

/// Limits on var() expansions and expanded length (prevents "billion laughs" blowups).
const MAX_VAR_EXPANSIONS: usize = 32;
const MAX_GENERATED_TEXT: usize = 1000;

/// Placeholder node for ::before / ::after content, whose text lives in `StyledNode::generated`.
static GENERATED: Node = Node::Text(String::new());
const MAX_VALUE_LEN: usize = 4096;

const UA_CSS: &str = r#"
html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, dl, dt, dd, blockquote, pre, form, header,
footer, nav, main, section, article, aside, figure, figcaption, address, hr, table, caption,
fieldset, legend, details, summary, center, hgroup, menu, noscript, thead, tbody, tfoot { display: block }
head, script, style, title, meta, link, base, template, datalist, option, optgroup, param { display: none }
li { display: list-item }
table { display: table; text-align: left }
tr { display: table-row }
td, th { display: table-cell; padding: 2px 4px }
body { margin: 8px }
p, blockquote, ul, ol, dl, pre, figure, table { margin-top: 1em; margin-bottom: 1em }
h1 { font-size: 2em; margin: 0.67em 0; font-weight: bold }
h2 { font-size: 1.5em; margin: 0.83em 0; font-weight: bold }
h3 { font-size: 1.17em; margin: 1em 0; font-weight: bold }
h4 { margin: 1.33em 0; font-weight: bold }
h5 { font-size: 0.83em; margin: 1.67em 0; font-weight: bold }
h6 { font-size: 0.67em; margin: 2.33em 0; font-weight: bold }
ul, ol, menu { padding-left: 40px }
li ul, li ol { margin-top: 0; margin-bottom: 0 }
blockquote, figure { margin-left: 40px; margin-right: 40px }
dd { margin-left: 40px }
b, strong, th, dt, summary, legend { font-weight: bold }
small, sub, sup { font-size: 0.83em }
a { color: #1a5fb4; text-decoration: underline }
pre, textarea, listing, xmp, plaintext { white-space: pre }
hr { border-top: 1px solid #c0c0c0; margin: 0.5em 0 }
center, caption { text-align: center }
fieldset { border: 1px solid #c0c0c0; padding: 0.35em 0.75em; margin: 0 2px }
button, select, textarea { border: 1px solid #a0a0a0; padding: 1px 6px }
button { display: inline-block; background-color: #efefef }
"#;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Display {
    Block,
    Inline,
    ListItem,
    Table,
    TableRow,
    Flex,
    Grid,
    None,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Position {
    Static,
    /// Also used for `sticky`, which needs scrolling to differ from static.
    Relative,
    Absolute,
    Fixed,
}

impl Position {
    /// Absolutely positioned boxes are taken out of normal flow.
    pub fn out_of_flow(self) -> bool {
        matches!(self, Position::Absolute | Position::Fixed)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Float {
    None,
    Left,
    Right,
}

/// A grid track size.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TrackSize {
    Px(f32),
    Percent(f32),
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
}

/// One column track: `minmax(min, max)`, or the same size twice.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Track {
    pub min: TrackSize,
    pub max: TrackSize,
}

/// `grid-template-columns`. `auto_repeat` holds `repeat(auto-fill | auto-fit, ...)`, which
/// expands to as many copies as fit, inserted at `auto_repeat_at`.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct GridTemplate {
    pub tracks: Vec<Track>,
    pub auto_repeat: Vec<Track>,
    pub auto_repeat_at: usize,
}

/// One end of a grid placement (`grid-column: 1 / span 2`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GridLine {
    Auto,
    Line(i32),
    Span(u32),
}

/// Explicit placement on one axis: (start, end).
pub type GridPlacement = (GridLine, GridLine);

pub const MAX_GRID_TRACKS: usize = 100;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Justify {
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// Cross-axis alignment. `stretch` (the default) is treated as `Start`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AlignItems {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Len {
    Px(f32),
    Percent(f32),
    Auto,
}

impl Len {
    pub fn resolve(self, base: f32) -> f32 {
        match self {
            Len::Px(px) => px,
            Len::Percent(p) => base * p / 100.0,
            Len::Auto => 0.0,
        }
    }
}

#[derive(Clone)]
pub struct Style {
    pub display: Display,
    pub color: u32,
    pub background: Option<u32>,
    pub font_size: f32,
    pub bold: bool,
    pub underline: bool,
    pub pre: bool,
    pub text_align: Align,
    pub line_height: f32,
    pub list_marker: bool,
    /// [top, right, bottom, left]
    pub margin: [Len; 4],
    pub padding: [Len; 4],
    border_width: [f32; 4],
    border_on: [bool; 4],
    pub border_color: u32,
    pub width: Len,
    pub height: Len,
    pub max_width: Len,
    /// The common "visually hidden" (sr-only) pattern: content meant only for screen readers.
    visually_hidden: bool,
    height_1px: bool,
    pub flex_column: bool,
    pub flex_wrap: bool,
    pub justify: Justify,
    pub align_items: AlignItems,
    pub row_gap: f32,
    pub column_gap: f32,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Len,
    pub grid_columns: Option<Arc<GridTemplate>>,
    /// `grid-template-areas`: rows of area names ("." for empty cells).
    pub grid_areas: Option<Arc<Vec<Vec<String>>>>,
    pub grid_area: Option<Arc<str>>,
    pub grid_column: GridPlacement,
    pub grid_row: GridPlacement,
    pub float: Float,
    /// `inline-block`, `inline-flex`, ...: laid out inside per `display`, but placed in a
    /// line as a single box.
    pub inline_level: bool,
    pub position: Position,
    /// `top`, `right`, `bottom`, `left`
    pub inset: [Len; 4],
    /// `clear`: (left, right)
    pub clear: (bool, bool),
    /// Raw `content` value (case preserved); only meaningful on pseudo-elements.
    content: Option<String>,
    vars: Rc<HashMap<String, String>>,
}

impl Style {
    fn root() -> Style {
        Style {
            display: Display::Block,
            color: 0x1f1f1f,
            background: None,
            font_size: 16.0,
            bold: false,
            underline: false,
            pre: false,
            text_align: Align::Left,
            line_height: 1.45,
            list_marker: true,
            margin: [Len::Px(0.0); 4],
            padding: [Len::Px(0.0); 4],
            border_width: [0.0; 4],
            border_on: [false; 4],
            border_color: 0x1f1f1f,
            width: Len::Auto,
            height: Len::Auto,
            max_width: Len::Auto,
            visually_hidden: false,
            height_1px: false,
            flex_column: false,
            flex_wrap: false,
            justify: Justify::Start,
            align_items: AlignItems::Start,
            row_gap: 0.0,
            column_gap: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Len::Auto,
            grid_columns: None,
            grid_areas: None,
            grid_area: None,
            grid_column: (GridLine::Auto, GridLine::Auto),
            grid_row: (GridLine::Auto, GridLine::Auto),
            float: Float::None,
            inline_level: false,
            position: Position::Static,
            inset: [Len::Auto; 4],
            clear: (false, false),
            content: None,
            vars: Rc::default(),
        }
    }

    /// Takes only the inherited properties from the parent.
    fn inherit(parent: &Style) -> Style {
        Style {
            display: Display::Inline,
            background: None,
            margin: [Len::Px(0.0); 4],
            padding: [Len::Px(0.0); 4],
            border_width: [0.0; 4],
            border_on: [false; 4],
            border_color: parent.color,
            width: Len::Auto,
            height: Len::Auto,
            max_width: Len::Auto,
            visually_hidden: false,
            height_1px: false,
            flex_column: false,
            flex_wrap: false,
            justify: Justify::Start,
            align_items: AlignItems::Start,
            row_gap: 0.0,
            column_gap: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Len::Auto,
            grid_columns: None,
            grid_areas: None,
            grid_area: None,
            grid_column: (GridLine::Auto, GridLine::Auto),
            grid_row: (GridLine::Auto, GridLine::Auto),
            float: Float::None,
            inline_level: false,
            position: Position::Static,
            inset: [Len::Auto; 4],
            clear: (false, false),
            content: None,
            ..parent.clone()
        }
    }

    /// Effective border widths [top, right, bottom, left].
    pub fn border(&self) -> [f32; 4] {
        std::array::from_fn(|i| if self.border_on[i] { self.border_width[i] } else { 0.0 })
    }
}

pub struct StyledNode<'a> {
    pub node: &'a Node,
    pub style: Style,
    pub children: Vec<StyledNode<'a>>,
    /// An inline element containing blocks (laid out as a block).
    pub block_inside: bool,
    /// Text generated by ::before / ::after.
    pub generated: Option<String>,
}

impl StyledNode<'_> {
    pub fn is_block(&self) -> bool {
        matches!(self.node, Node::Element(_))
            && !self.is_atomic_inline()
            && (self.style.display != Display::Inline || self.block_inside)
    }

    /// An inline-block (or inline-flex, ...): a box placed in a line like a word.
    pub fn is_atomic_inline(&self) -> bool {
        matches!(self.node, Node::Element(_)) && self.style.inline_level && self.style.display != Display::Inline
    }

    /// An absolutely positioned element. Text nodes copy their parent's style, so the
    /// node kind must be checked too.
    pub fn is_out_of_flow(&self) -> bool {
        matches!(self.node, Node::Element(_)) && self.style.position.out_of_flow()
    }

    pub fn tag(&self) -> Option<&str> {
        match self.node {
            Node::Element(e) => Some(&e.tag),
            Node::Text(_) => None,
        }
    }
}

pub fn style_tree<'a>(root: &'a Node, author: &[Stylesheet], viewport_width: f32) -> Option<StyledNode<'a>> {
    static UA: OnceLock<Stylesheet> = OnceLock::new();
    let ua = UA.get_or_init(|| css::parse_stylesheet(UA_CSS));
    let mut sheets = vec![(0u8, ua)];
    sheets.extend(author.iter().map(|s| (1u8, s)));
    let index = Index::build(&sheets, viewport_width);
    build(root, &Style::root(), &mut Vec::new(), &index)
}

fn build<'a>(node: &'a Node, parent: &Style, ancestors: &mut Vec<&'a Element>, index: &Index) -> Option<StyledNode<'a>> {
    match node {
        Node::Text(_) => Some(StyledNode { node, style: parent.clone(), children: Vec::new(), block_inside: false, generated: None }),
        Node::Element(el) => {
            let Computed { style, before, after } = compute(el, parent, ancestors, index);
            if style.display == Display::None {
                return None;
            }
            let generated = |(style, text): (Style, String)| StyledNode {
                node: &GENERATED,
                style,
                children: Vec::new(),
                block_inside: false,
                generated: Some(text),
            };
            ancestors.push(el);
            // A closed <details> shows only its <summary>
            let closed_details = el.tag == "details" && el.attr("open").is_none();
            let children: Vec<_> = before
                .map(generated)
                .into_iter()
                .chain(
                    el.children
                        .iter()
                        .filter(|c| !closed_details || matches!(c, Node::Element(e) if e.tag == "summary"))
                        .filter_map(|c| build(c, &style, ancestors, index)),
                )
                .chain(after.map(generated))
                .collect();
            ancestors.pop();
            let block_inside = children.iter().any(StyledNode::is_block);
            Some(StyledNode { node, style, children, block_inside, generated: None })
        }
    }
}

#[derive(Clone, Copy)]
struct Entry<'s> {
    seq: usize,
    origin: u8,
    order: usize,
    rule: &'s Rule,
    selector: &'s Selector,
}

/// Indexes rules by their rightmost compound to narrow per-element candidates.
#[derive(Default)]
struct Index<'s> {
    by_id: HashMap<&'s str, Vec<Entry<'s>>>,
    by_class: HashMap<&'s str, Vec<Entry<'s>>>,
    by_tag: HashMap<&'s str, Vec<Entry<'s>>>,
    any: Vec<Entry<'s>>,
}

impl<'s> Index<'s> {
    fn build(sheets: &[(u8, &'s Stylesheet)], viewport_width: f32) -> Self {
        let mut index = Index::default();
        let mut seq = 0;
        let rules = sheets.iter().flat_map(|(o, s)| s.rules.iter().map(move |r| (*o, r)));
        for (order, (origin, rule)) in rules.filter(|(_, r)| r.applies_at(viewport_width)).enumerate() {
            for selector in &rule.selectors {
                let entry = Entry { seq, origin, order, rule, selector };
                seq += 1;
                let last = selector.parts.last().expect("non-empty selector");
                if let Some(id) = &last.id {
                    index.by_id.entry(id).or_default().push(entry);
                } else if let Some(class) = last.classes.first() {
                    index.by_class.entry(class).or_default().push(entry);
                } else if let Some(tag) = &last.tag {
                    index.by_tag.entry(tag).or_default().push(entry);
                } else {
                    index.any.push(entry);
                }
            }
        }
        index
    }

    fn candidates(&self, el: &Element) -> Vec<Entry<'s>> {
        let mut out: Vec<Entry> = Vec::new();
        if let Some(v) = el.attr("id").and_then(|id| self.by_id.get(id)) {
            out.extend(v);
        }
        for class in el.attr("class").unwrap_or("").split_ascii_whitespace() {
            if let Some(v) = self.by_class.get(class) {
                out.extend(v);
            }
        }
        if let Some(v) = self.by_tag.get(el.tag.as_str()) {
            out.extend(v);
        }
        out.extend(&self.any);
        out.sort_by_key(|e| e.seq);
        out.dedup_by_key(|e| e.seq);
        out
    }
}

/// An element's style plus its ::before / ::after content, if any.
struct Computed {
    style: Style,
    before: Option<(Style, String)>,
    after: Option<(Style, String)>,
}

fn compute(el: &Element, parent: &Style, ancestors: &[&Element], index: &Index) -> Computed {
    let mut style = Style::inherit(parent);
    if el.tag == "#document" {
        style.display = Display::Block;
        return Computed { style, before: None, after: None };
    }
    if el.attr("hidden").is_some() {
        style.display = Display::None;
    }
    apply_presentational_hints(el, &mut style, parent);

    let matched: Vec<Entry> = index.candidates(el).into_iter().filter(|e| selector::matches(e.selector, el, ancestors)).collect();
    let inline = el.attr("style").map(css::parse_declarations).unwrap_or_default();
    let own: Vec<Entry> = matched.iter().filter(|e| e.selector.pseudo.is_none()).copied().collect();
    cascade(&mut style, parent, &own, &inline);
    // Floats and absolutely positioned boxes are always block-level
    if style.float != Float::None || style.position.out_of_flow() {
        if style.display == Display::Inline {
            style.display = Display::Block;
        }
        style.inline_level = false;
    }
    let tiny = style.height_1px && matches!(style.width, Len::Px(w) if w <= 1.0);
    if style.visually_hidden || tiny {
        style.display = Display::None;
    }

    let pseudo = |which: PseudoElement| -> Option<(Style, String)> {
        let rules: Vec<Entry> = matched.iter().filter(|e| e.selector.pseudo == Some(which)).copied().collect();
        if rules.is_empty() {
            return None;
        }
        let mut ps = Style::inherit(&style);
        cascade(&mut ps, &style, &rules, &[]);
        let text = resolve_content(ps.content.as_deref()?, el)?;
        (ps.display != Display::None && !ps.visually_hidden).then_some((ps, text))
    };
    let (before, after) = (pseudo(PseudoElement::Before), pseudo(PseudoElement::After));
    Computed { style, before, after }
}

/// Applies matched rules and inline declarations to `style`.
fn cascade(style: &mut Style, parent: &Style, entries: &[Entry], inline: &[Declaration]) {
    // Sort by (precedence, specificity, order) and apply so later entries win
    let mut decls: Vec<((u8, u32, usize), &Declaration)> = Vec::new();
    for e in entries {
        for d in &e.rule.declarations {
            let level = match (d.important, e.origin) {
                (false, 0) => 0,
                (false, _) => 1,
                (true, 0) => 5,
                (true, _) => 3,
            };
            decls.push(((level, e.selector.specificity, e.order), d));
        }
    }
    for d in inline {
        decls.push(((if d.important { 4 } else { 2 }, 0, usize::MAX), d));
    }
    decls.sort_by_key(|(key, _)| *key);

    // Apply custom properties first, then font-size (the em base), then the rest
    for (_, d) in &decls {
        if d.name.starts_with("--") {
            Rc::make_mut(&mut style.vars).insert(d.name.clone(), d.value.clone());
        }
    }
    for (_, d) in decls.iter().filter(|(_, d)| d.name == "font-size") {
        if let Some(v) = substitute_vars(&d.value, &style.vars) {
            if let Some(size) = parse_font_size(&v.to_ascii_lowercase(), parent.font_size) {
                style.font_size = size;
            }
        }
    }
    for (_, d) in &decls {
        if d.name.starts_with("--") || d.name == "font-size" {
            continue;
        }
        if let Some(v) = substitute_vars(&d.value, &style.vars) {
            if d.name == "content" {
                style.content = Some(v);
            } else {
                apply(style, parent, &d.name, &v.to_ascii_lowercase());
            }
        }
    }
}

/// Resolves a `content` value to text. Strings, attr() and quotes are supported;
/// counters and images are not. Icon-font glyphs (private use area) are dropped.
fn resolve_content(raw: &str, el: &Element) -> Option<String> {
    let mut out = String::new();
    for token in split_top_level(raw.trim(), ' ') {
        match token {
            "none" | "normal" => return None,
            "open-quote" => out.push('“'),
            "close-quote" => out.push('”'),
            t if t.starts_with(['"', '\'']) => {
                let inner = &t[1..];
                let inner = inner.strip_suffix(&t[..1]).unwrap_or(inner);
                out.push_str(&unescape_css_string(inner));
            }
            t => {
                if let Some(name) = t.strip_prefix("attr(").and_then(|r| r.strip_suffix(')')) {
                    out.push_str(el.attr(name.trim()).unwrap_or(""));
                }
            }
        }
        if out.len() > MAX_GENERATED_TEXT {
            break;
        }
    }
    let text: String = out.chars().filter(|&c| !is_private_use(c)).take(MAX_GENERATED_TEXT).collect();
    (!text.is_empty()).then_some(text)
}

/// `\2022` → '•', `\"` → '"'.
fn unescape_css_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            hex.push(chars.next().unwrap());
        }
        if hex.is_empty() {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            if chars.peek() == Some(&' ') {
                chars.next();
            }
            let code = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
            out.push(char::from_u32(code).filter(|&c| c != '\0').unwrap_or('\u{FFFD}'));
        }
    }
    out
}

fn is_private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)
}

/// Expands var(--name, fallback). Exceeding the limits makes the value invalid.
fn substitute_vars(value: &str, vars: &HashMap<String, String>) -> Option<String> {
    let mut v = value.to_string();
    for _ in 0..MAX_VAR_EXPANSIONS {
        let Some(start) = v.find("var(") else { return Some(v) };
        let inner_start = start + 4;
        let mut depth = 1;
        let mut end = None;
        for (i, c) in v[inner_start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(inner_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        let inner = &v[inner_start..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner.trim(), None),
        };
        let replacement = vars.get(name).map(String::as_str).or(fallback)?.to_string();
        v.replace_range(start..=end, &replacement);
        if v.len() > MAX_VALUE_LEN {
            return None;
        }
    }
    None
}

fn apply(s: &mut Style, parent: &Style, name: &str, v: &str) {
    if v == "inherit" {
        match name {
            "color" => s.color = parent.color,
            "background-color" | "background" => s.background = parent.background,
            _ => {}
        }
        return;
    }
    match name {
        "display" => {
            if let Some((d, inline_level)) = parse_display(v) {
                s.display = d;
                s.inline_level = inline_level;
            }
        }
        "color" => {
            if let Some((rgb, a)) = parse_color(v, s.color) {
                if a > 0.05 {
                    s.color = blend_on_white(rgb, a);
                }
            }
        }
        "background-color" | "background" => {
            if v == "none" || v == "transparent" {
                s.background = None;
            } else if let Some(c) = split_top_level(v, ' ').into_iter().find_map(|t| parse_color(t, s.color)) {
                s.background = if c.1 > 0.05 { Some(blend_on_white(c.0, c.1)) } else { None };
            }
        }
        "font-weight" => s.bold = parse_bold(v).unwrap_or(s.bold),
        "font" => {
            for token in split_top_level(v, ' ') {
                if let Some(b) = parse_bold(token) {
                    s.bold = b;
                } else if token.contains('/') || parse_font_size(token, parent.font_size).is_some() {
                    let (size, lh) = token.split_once('/').unwrap_or((token, ""));
                    if let Some(px) = parse_font_size(size, parent.font_size) {
                        s.font_size = px;
                    }
                    if let Some(lh) = parse_line_height(lh, s.font_size) {
                        s.line_height = lh;
                    }
                    break; // font family follows the size
                }
            }
        }
        "line-height" => s.line_height = parse_line_height(v, s.font_size).unwrap_or(s.line_height),
        "text-align" => {
            s.text_align = match v {
                "center" | "-webkit-center" => Align::Center,
                "right" | "end" => Align::Right,
                "left" | "start" | "justify" => Align::Left,
                _ => s.text_align,
            }
        }
        "text-decoration" | "text-decoration-line" => s.underline = v.contains("underline"),
        "white-space" => s.pre = matches!(v, "pre" | "pre-wrap" | "break-spaces"),
        "list-style" | "list-style-type" => s.list_marker = !v.split_whitespace().any(|t| t == "none"),
        "margin" => apply_box(&mut s.margin, v, s.font_size, true),
        "padding" => apply_box(&mut s.padding, v, s.font_size, false),
        "width" => s.width = parse_length(v, s.font_size).unwrap_or(s.width),
        "height" => {
            let height = parse_length(v, s.font_size);
            s.height_1px = matches!(height, Some(Len::Px(h)) if h <= 1.0);
            s.height = height.unwrap_or(s.height);
        }
        "clip" => s.visually_hidden |= v.starts_with("rect(0"),
        // Approximation: hidden content is almost always an out-of-flow menu or popup,
        // so dropping it (instead of reserving its space) is closer to what users see
        "visibility" => s.visually_hidden = v == "hidden" || v == "collapse",
        "clip-path" => s.visually_hidden |= v.starts_with("inset(50%") || v.starts_with("inset(100%") || v.starts_with("rect(0"),
        "max-width" => s.max_width = if v == "none" { Len::Auto } else { parse_length(v, s.font_size).unwrap_or(s.max_width) },
        "border" => {
            for side in 0..4 {
                apply_border_side(s, side, v);
            }
        }
        "border-width" => {
            let mut lens = [Len::Px(0.0); 4];
            apply_box(&mut lens, v, s.font_size, false);
            s.border_width = lens.map(|l| l.resolve(0.0));
        }
        "border-style" => {
            let on = !matches!(v, "none" | "hidden");
            s.border_on = [on; 4];
        }
        "flex-direction" => s.flex_column = v.starts_with("column"),
        "flex-wrap" => s.flex_wrap = v.starts_with("wrap"),
        "flex-flow" => {
            for token in v.split_whitespace() {
                match token {
                    "row" | "row-reverse" => s.flex_column = false,
                    "column" | "column-reverse" => s.flex_column = true,
                    "wrap" | "wrap-reverse" => s.flex_wrap = true,
                    "nowrap" => s.flex_wrap = false,
                    _ => {}
                }
            }
        }
        "justify-content" => {
            s.justify = match v {
                "center" => Justify::Center,
                "flex-end" | "end" | "right" => Justify::End,
                "space-between" => Justify::SpaceBetween,
                "space-around" => Justify::SpaceAround,
                "space-evenly" => Justify::SpaceEvenly,
                _ => Justify::Start,
            }
        }
        "align-items" => {
            s.align_items = match v {
                "center" => AlignItems::Center,
                "flex-end" | "end" => AlignItems::End,
                _ => AlignItems::Start,
            }
        }
        "gap" | "grid-gap" => {
            let gaps: Vec<f32> = split_top_level(v, ' ').into_iter().filter_map(|t| parse_px(t, s.font_size)).collect();
            if let Some(&row) = gaps.first() {
                s.row_gap = row;
                s.column_gap = *gaps.get(1).unwrap_or(&row);
            }
        }
        "row-gap" | "grid-row-gap" => s.row_gap = parse_px(v, s.font_size).unwrap_or(s.row_gap),
        "column-gap" | "grid-column-gap" => s.column_gap = parse_px(v, s.font_size).unwrap_or(s.column_gap),
        "flex-grow" => s.flex_grow = v.parse::<f32>().ok().filter(|n| n.is_finite()).map_or(s.flex_grow, |n| n.max(0.0)),
        "flex-shrink" => s.flex_shrink = v.parse::<f32>().ok().filter(|n| n.is_finite()).map_or(s.flex_shrink, |n| n.max(0.0)),
        "flex-basis" => s.flex_basis = if v == "content" { Len::Auto } else { parse_length(v, s.font_size).unwrap_or(s.flex_basis) },
        "flex" => apply_flex_shorthand(s, v),
        "position" => {
            s.position = match v {
                "relative" | "sticky" | "-webkit-sticky" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                _ => Position::Static,
            }
        }
        "top" | "right" | "bottom" | "left" => {
            let side = ["top", "right", "bottom", "left"].iter().position(|s| *s == name).unwrap_or(0);
            s.inset[side] = parse_length(v, s.font_size).unwrap_or(s.inset[side]);
        }
        "inset" => apply_box(&mut s.inset, v, s.font_size, true),
        "float" => {
            s.float = match v {
                "left" | "inline-start" => Float::Left,
                "right" | "inline-end" => Float::Right,
                _ => Float::None,
            }
        }
        "clear" => {
            s.clear = match v {
                "left" | "inline-start" => (true, false),
                "right" | "inline-end" => (false, true),
                "both" => (true, true),
                _ => (false, false),
            }
        }
        "grid-template-columns" => s.grid_columns = parse_grid_template(v, s.font_size).map(Arc::new),
        // `grid-template: <rows> / <columns>`; only the columns matter here
        "grid-template" => {
            if let Some((_, columns)) = v.split_once('/') {
                s.grid_columns = parse_grid_template(columns, s.font_size).map(Arc::new);
            }
        }
        "grid-template-areas" => s.grid_areas = parse_grid_areas(v).map(Arc::new),
        "grid-area" => {
            let parts: Vec<&str> = v.split('/').map(str::trim).collect();
            if parts.len() >= 2 {
                // grid-area: row-start / column-start / row-end / column-end
                let line = |i: usize| parts.get(i).map_or(GridLine::Auto, |p| parse_grid_line(p));
                s.grid_row = (line(0), line(2));
                s.grid_column = (line(1), line(3));
            } else if v != "auto" {
                s.grid_area = Some(Arc::from(v));
            }
        }
        "grid-column" => s.grid_column = parse_grid_placement(v),
        "grid-row" => s.grid_row = parse_grid_placement(v),
        "grid-column-start" => s.grid_column.0 = parse_grid_line(v),
        "grid-column-end" => s.grid_column.1 = parse_grid_line(v),
        "grid-row-start" => s.grid_row.0 = parse_grid_line(v),
        "grid-row-end" => s.grid_row.1 = parse_grid_line(v),
        "border-color" => {
            if let Some((rgb, _)) = parse_color(v, s.color) {
                s.border_color = rgb;
            }
        }
        _ => {
            for (i, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
                if name.strip_prefix("margin-") == Some(side) {
                    if let Some(l) = parse_length(v, s.font_size) {
                        s.margin[i] = l;
                    }
                } else if name.strip_prefix("padding-") == Some(side) {
                    if let Some(l) = parse_length(v, s.font_size) {
                        s.padding[i] = clamp_non_negative(l);
                    }
                } else if name.strip_prefix("border-") == Some(side) {
                    apply_border_side(s, i, v);
                }
            }
        }
    }
}

/// Legacy HTML attributes that affect presentation (bgcolor, width, align, ...).
/// They act like author rules with the lowest specificity, so real CSS overrides them.
fn apply_presentational_hints(el: &Element, s: &mut Style, parent: &Style) {
    let attr = |name: &str| el.attr(name).map(|v| v.trim().to_ascii_lowercase());
    if let Some(color) = attr("bgcolor") {
        apply(s, parent, "background-color", &color);
    }
    if el.tag == "img" {
        if let Some(height) = attr("height") {
            let height = if height.chars().all(|c| c.is_ascii_digit()) { format!("{height}px") } else { height };
            apply(s, parent, "height", &height);
        }
    }
    if matches!(el.tag.as_str(), "table" | "td" | "th" | "img" | "hr") {
        if let Some(width) = attr("width") {
            let width = if width.chars().all(|c| c.is_ascii_digit()) { format!("{width}px") } else { width };
            apply(s, parent, "width", &width);
        }
    }
    if matches!(el.tag.as_str(), "td" | "th" | "tr" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        if let Some(align) = attr("align") {
            apply(s, parent, "text-align", &align);
        }
    }
    if el.tag == "table" && attr("align").as_deref() == Some("center") {
        s.margin[1] = Len::Auto;
        s.margin[3] = Len::Auto;
    }
    if el.tag == "font" {
        if let Some(color) = attr("color") {
            apply(s, parent, "color", &color);
        }
    }
}

fn parse_grid_template(v: &str, font_size: f32) -> Option<GridTemplate> {
    if v.trim() == "none" {
        return None;
    }
    let mut template = GridTemplate::default();
    for token in split_top_level(v, ' ') {
        if token.starts_with('[') {
            continue; // line names
        }
        if let Some(inner) = token.strip_prefix("repeat(").and_then(|r| r.strip_suffix(')')) {
            let (count, tracks) = inner.split_once(',')?;
            let tracks: Vec<Track> = split_top_level(tracks, ' ')
                .into_iter()
                .filter(|t| !t.starts_with('['))
                .map(|t| parse_track(t, font_size))
                .collect::<Option<_>>()?;
            match count.trim() {
                "auto-fill" | "auto-fit" => {
                    template.auto_repeat_at = template.tracks.len();
                    template.auto_repeat = tracks;
                }
                n => {
                    let n: usize = n.parse().ok()?;
                    for _ in 0..n.min(MAX_GRID_TRACKS) {
                        template.tracks.extend_from_slice(&tracks);
                    }
                }
            }
        } else {
            template.tracks.push(parse_track(token, font_size)?);
        }
        if template.tracks.len() > MAX_GRID_TRACKS {
            template.tracks.truncate(MAX_GRID_TRACKS);
            break;
        }
    }
    Some(template)
}

fn parse_track(t: &str, font_size: f32) -> Option<Track> {
    if let Some(inner) = t.strip_prefix("minmax(").and_then(|r| r.strip_suffix(')')) {
        let (min, max) = inner.split_once(',')?;
        return Some(Track { min: parse_track_size(min.trim(), font_size)?, max: parse_track_size(max.trim(), font_size)? });
    }
    if let Some(inner) = t.strip_prefix("fit-content(").and_then(|r| r.strip_suffix(')')) {
        return Some(Track { min: TrackSize::Auto, max: parse_track_size(inner.trim(), font_size)? });
    }
    let size = parse_track_size(t, font_size)?;
    Some(Track { min: size, max: size })
}

fn parse_track_size(t: &str, font_size: f32) -> Option<TrackSize> {
    Some(match t {
        "auto" => TrackSize::Auto,
        "min-content" => TrackSize::MinContent,
        "max-content" => TrackSize::MaxContent,
        _ => {
            if let Some(fr) = t.strip_suffix("fr") {
                let n: f32 = fr.parse().ok()?;
                return n.is_finite().then(|| TrackSize::Fr(n.max(0.0)));
            }
            match parse_length(t, font_size)? {
                Len::Px(px) => TrackSize::Px(px.max(0.0)),
                Len::Percent(p) => TrackSize::Percent(p.max(0.0)),
                Len::Auto => TrackSize::Auto,
            }
        }
    })
}

/// `"a a" "b c"` -> [["a", "a"], ["b", "c"]]. Ragged rows make the value invalid, per spec.
fn parse_grid_areas(v: &str) -> Option<Vec<Vec<String>>> {
    let rows: Vec<Vec<String>> = v
        .split(['"', '\''])
        .skip(1)
        .step_by(2)
        .map(|row| row.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .filter(|row| !row.is_empty())
        .take(MAX_GRID_TRACKS)
        .collect();
    let width = rows.first()?.len();
    (width <= MAX_GRID_TRACKS && rows.iter().all(|r| r.len() == width)).then_some(rows)
}

fn parse_grid_placement(v: &str) -> GridPlacement {
    match v.split_once('/') {
        Some((start, end)) => (parse_grid_line(start.trim()), parse_grid_line(end.trim())),
        None => (parse_grid_line(v.trim()), GridLine::Auto),
    }
}

fn parse_grid_line(v: &str) -> GridLine {
    if let Some(n) = v.strip_prefix("span") {
        return n.trim().parse().map_or(GridLine::Span(1), |n: u32| GridLine::Span(n.clamp(1, MAX_GRID_TRACKS as u32)));
    }
    match v.parse::<i32>() {
        Ok(n) if n != 0 => GridLine::Line(n.clamp(-(MAX_GRID_TRACKS as i32), MAX_GRID_TRACKS as i32)),
        _ => GridLine::Auto,
    }
}

/// `flex: none | auto | <grow> [<shrink>] [<basis>]`
fn apply_flex_shorthand(s: &mut Style, v: &str) {
    let (grow, shrink, basis) = match v {
        "none" => (0.0, 0.0, Len::Auto),
        "auto" => (1.0, 1.0, Len::Auto),
        "initial" => (0.0, 1.0, Len::Auto),
        _ => {
            let mut numbers = Vec::new();
            let mut basis = Len::Px(0.0);
            for token in split_top_level(v, ' ') {
                match token.parse::<f32>() {
                    Ok(n) if n.is_finite() => numbers.push(n.max(0.0)),
                    _ => basis = parse_length(token, s.font_size).unwrap_or(basis),
                }
            }
            (numbers.first().copied().unwrap_or(1.0), numbers.get(1).copied().unwrap_or(1.0), basis)
        }
    };
    s.flex_grow = grow;
    s.flex_shrink = shrink;
    s.flex_basis = basis;
}

/// A length that must resolve to pixels (percentages are ignored).
fn parse_px(v: &str, font_size: f32) -> Option<f32> {
    match parse_length(v, font_size)? {
        Len::Px(px) => Some(px.max(0.0)),
        _ => None,
    }
}

fn apply_box(target: &mut [Len; 4], v: &str, font_size: f32, allow_negative: bool) {
    let vals: Vec<Len> = split_top_level(v, ' ')
        .into_iter()
        .filter_map(|t| parse_length(t, font_size))
        .map(|l| if allow_negative { l } else { clamp_non_negative(l) })
        .collect();
    *target = match vals.as_slice() {
        [a] => [*a; 4],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d, ..] => [*a, *b, *c, *d],
        [] => return,
    };
}

fn clamp_non_negative(l: Len) -> Len {
    match l {
        Len::Px(p) => Len::Px(p.max(0.0)),
        Len::Percent(p) => Len::Percent(p.max(0.0)),
        Len::Auto => Len::Px(0.0),
    }
}

/// Parses `border: 1px solid #ccc`. Without a line style no border is drawn, per spec.
fn apply_border_side(s: &mut Style, side: usize, v: &str) {
    let mut width = 3.0;
    let mut on = false;
    for token in split_top_level(v, ' ') {
        match token {
            "none" | "hidden" => on = false,
            "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => on = true,
            "thin" => width = 1.0,
            "medium" => width = 3.0,
            "thick" => width = 5.0,
            _ => {
                if let Some(Len::Px(px)) = parse_length(token, s.font_size) {
                    width = px.max(0.0);
                } else if let Some((rgb, _)) = parse_color(token, s.color) {
                    s.border_color = rgb;
                }
            }
        }
    }
    s.border_width[side] = width;
    s.border_on[side] = on;
}

/// Returns (inner display, whether the box is inline-level).
fn parse_display(v: &str) -> Option<(Display, bool)> {
    Some(match v {
        "none" => (Display::None, false),
        "inline" | "contents" => (Display::Inline, false),
        "inline-block" => (Display::Block, true),
        "flex" => (Display::Flex, false),
        "inline-flex" => (Display::Flex, true),
        "grid" => (Display::Grid, false),
        "inline-grid" => (Display::Grid, true),
        "table" => (Display::Table, false),
        "inline-table" => (Display::Table, true),
        "list-item" => (Display::ListItem, false),
        "table-row" => (Display::TableRow, false),
        "block" | "flow-root" | "table-cell" | "table-caption" | "table-row-group" | "table-header-group"
        | "table-footer-group" => (Display::Block, false),
        _ => return None,
    })
}

fn parse_bold(v: &str) -> Option<bool> {
    match v {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" => Some(false),
        _ => v.parse::<u32>().ok().filter(|n| n % 100 == 0).map(|n| n >= 600),
    }
}

fn parse_font_size(v: &str, parent: f32) -> Option<f32> {
    let px = match v {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 13.0,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "smaller" => parent * 0.83,
        "larger" => parent * 1.2,
        _ => match parse_length(v, parent)? {
            Len::Px(px) => px,
            Len::Percent(p) => parent * p / 100.0,
            Len::Auto => return None,
        },
    };
    Some(px.clamp(6.0, 96.0))
}

fn parse_line_height(v: &str, font_size: f32) -> Option<f32> {
    if v == "normal" {
        return Some(1.3);
    }
    let lh = if let Ok(n) = v.parse::<f32>() {
        n
    } else {
        match parse_length(v, font_size)? {
            Len::Px(px) => px / font_size,
            Len::Percent(p) => p / 100.0,
            Len::Auto => return None,
        }
    };
    lh.is_finite().then(|| lh.clamp(0.8, 4.0))
}

/// Parses a color as (RGB, alpha).
fn parse_color(v: &str, current: u32) -> Option<(u32, f32)> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        let digits: Vec<u32> = hex.chars().map(|c| c.to_digit(16)).collect::<Option<_>>()?;
        let (rgb, a) = match digits.len() {
            3 | 4 => (
                digits[..3].iter().fold(0, |acc, d| (acc << 8) | (d * 17)),
                digits.get(3).map_or(1.0, |d| (d * 17) as f32 / 255.0),
            ),
            6 | 8 => (
                digits[..6].iter().fold(0, |acc, d| (acc << 4) | d),
                if digits.len() == 8 { (digits[6] * 16 + digits[7]) as f32 / 255.0 } else { 1.0 },
            ),
            _ => return None,
        };
        return Some((rgb, a));
    }
    // light-dark(light, dark): use the light value
    if let Some(inner) = v.strip_prefix("light-dark(").and_then(|r| r.strip_suffix(')')) {
        return parse_color(split_top_level(inner, ',').first()?, current);
    }
    if let Some((func, rest)) = v.split_once('(') {
        let args: Vec<&str> = rest.trim_end_matches(')').split([',', '/', ' ']).filter(|s| !s.is_empty()).collect();
        let num = |s: &str, scale: f32| -> Option<f32> {
            match s.strip_suffix('%') {
                Some(p) => p.parse::<f32>().ok().map(|n| n / 100.0 * scale),
                None => s.trim_end_matches("deg").parse::<f32>().ok(),
            }
        };
        let alpha = args.get(3).and_then(|a| num(a, 1.0)).unwrap_or(1.0);
        let rgb = match func {
            "rgb" | "rgba" => {
                let c: Vec<u32> = args.iter().take(3).map(|a| num(a, 255.0).map(|n| n.clamp(0.0, 255.0) as u32)).collect::<Option<_>>()?;
                (*c.first()? << 16) | (*c.get(1)? << 8) | *c.get(2)?
            }
            "hsl" | "hsla" => hsl_to_rgb(num(args.first()?, 1.0)?, num(args.get(1)?, 1.0)?, num(args.get(2)?, 1.0)?),
            _ => return None,
        };
        return Some((rgb, alpha.clamp(0.0, 1.0)));
    }
    let rgb = match v {
        "currentcolor" => current,
        "transparent" => return Some((0, 0.0)),
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xff0000,
        "green" => 0x008000,
        "blue" => 0x0000ff,
        "gray" | "grey" => 0x808080,
        "darkgray" | "darkgrey" => 0xa9a9a9,
        "lightgray" | "lightgrey" => 0xd3d3d3,
        "dimgray" | "dimgrey" => 0x696969,
        "silver" => 0xc0c0c0,
        "whitesmoke" => 0xf5f5f5,
        "gainsboro" => 0xdcdcdc,
        "maroon" => 0x800000,
        "darkred" => 0x8b0000,
        "crimson" => 0xdc143c,
        "purple" => 0x800080,
        "fuchsia" | "magenta" => 0xff00ff,
        "lime" => 0x00ff00,
        "darkgreen" => 0x006400,
        "olive" => 0x808000,
        "yellow" => 0xffff00,
        "gold" => 0xffd700,
        "orange" => 0xffa500,
        "navy" => 0x000080,
        "darkblue" => 0x00008b,
        "teal" => 0x008080,
        "aqua" | "cyan" => 0x00ffff,
        "brown" => 0xa52a2a,
        "pink" => 0xffc0cb,
        _ => return None,
    };
    Some((rgb, 1.0))
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let (s, l) = (s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m) * 255.0).round().clamp(0.0, 255.0)) as u32;
    (to(r) << 16) | (to(g) << 8) | to(b)
}

/// Approximates translucent colors by compositing over white.
fn blend_on_white(rgb: u32, a: f32) -> u32 {
    if a >= 1.0 {
        return rgb;
    }
    let ch = |shift: u32| {
        let c = ((rgb >> shift) & 0xff) as f32;
        ((c * a + 255.0 * (1.0 - a)).round() as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_expansion_is_bounded() {
        let mut vars = HashMap::new();
        vars.insert("--a".to_string(), "var(--a) var(--a)".to_string());
        assert_eq!(substitute_vars("var(--a)", &vars), None);
        vars.insert("--b".to_string(), "#fff".to_string());
        assert_eq!(substitute_vars("var(--b)", &vars).as_deref(), Some("#fff"));
        assert_eq!(substitute_vars("var(--x, red)", &vars).as_deref(), Some("red"));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#abc", 0), Some((0xaabbcc, 1.0)));
        assert_eq!(parse_color("rgb(255 0 0)", 0), Some((0xff0000, 1.0)));
        assert_eq!(parse_color("hsl(120, 100%, 50%)", 0).unwrap().0, 0x00ff00);
        assert_eq!(parse_color("light-dark(#eee, #222)", 0), Some((0xeeeeee, 1.0)));
    }

    #[test]
    fn cascade_specificity_and_inline() {
        let dom = crate::html::parse(r#"<style>p { color: red } .x { color: blue }</style><p class="x" style="font-size: 20px">t</p>"#);
        let info = crate::dom::doc_info(&dom);
        let sheets: Vec<_> = info.styles.iter().map(|s| match s {
            crate::dom::StyleSource::Inline(c) => css::parse_stylesheet(c),
            _ => unreachable!(),
        }).collect();
        let tree = style_tree(&dom, &sheets, 1024.0).unwrap();
        let p = tree.children.iter().find(|c| c.tag() == Some("p")).unwrap();
        assert_eq!(p.style.color, 0x0000ff);
        assert_eq!(p.style.font_size, 20.0);
    }
}
