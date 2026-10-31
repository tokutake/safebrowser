//! DOM tree. It is built by the parser, optionally rewritten by scripts (see js.rs) before
//! layout, and read-only afterwards.

#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(tag: impl Into<String>, attrs: Vec<(String, String)>) -> Self {
        Element { tag: tag.into(), attrs, children: Vec::new() }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn has_class(&self, class: &str) -> bool {
        self.attr("class").is_some_and(|v| v.split_ascii_whitespace().any(|c| c == class))
    }

    /// Text of the direct text children (enough for <option>, <textarea> and <title>).
    pub fn text(&self) -> String {
        text_content(self)
    }

    /// For a <select>: the option marked `selected`, else the first one.
    pub fn selected_option(&self) -> Option<&Element> {
        let mut options = Vec::new();
        collect_options(self, &mut options);
        options.iter().find(|o| o.attr("selected").is_some()).or(options.first()).copied()
    }

    /// The value an <option> submits: its `value` attribute, else its text.
    pub fn option_value(&self) -> String {
        self.attr("value").map_or_else(|| self.text().trim().to_string(), str::to_string)
    }

    /// Identity of this element for the lifetime of its document (form field state is keyed by it).
    pub fn id(&self) -> usize {
        self as *const Element as usize
    }
}

/// The URL an <img> shows: `src`, else the first candidate of `srcset`.
pub fn image_source(el: &Element) -> Option<&str> {
    let src = el.attr("src").map(str::trim).filter(|s| !s.is_empty());
    src.or_else(|| {
        let first = el.attr("srcset")?.split(',').next()?;
        first.split_ascii_whitespace().next()
    })
}

fn collect_options<'a>(el: &'a Element, out: &mut Vec<&'a Element>) {
    for child in &el.children {
        if let Node::Element(c) = child {
            if c.tag == "option" {
                out.push(c);
            } else if c.tag == "optgroup" {
                collect_options(c, out);
            }
        }
    }
}

pub enum StyleSource {
    Inline(String),
    External(String),
}

/// Document metadata needed before layout.
#[derive(Default)]
pub struct DocInfo {
    pub title: String,
    pub styles: Vec<StyleSource>,
    pub base: Option<String>,
}

pub fn doc_info(root: &Node) -> DocInfo {
    let mut info = DocInfo::default();
    walk(root, &mut info);
    info.title = info.title.split_whitespace().collect::<Vec<_>>().join(" ");
    info
}

// Recursion is fine: the parser caps depth at MAX_DEPTH.
fn walk(node: &Node, info: &mut DocInfo) {
    let Node::Element(el) = node else { return };
    match el.tag.as_str() {
        "title" if info.title.is_empty() => info.title = text_content(el),
        "style" => info.styles.push(StyleSource::Inline(text_content(el))),
        "link" => {
            let rel = el.attr("rel").unwrap_or("").to_ascii_lowercase();
            let media = el.attr("media").unwrap_or("all").to_ascii_lowercase();
            let is_sheet = rel.split_ascii_whitespace().any(|r| r == "stylesheet")
                && !rel.contains("alternate");
            if is_sheet && !(media.contains("print") && !media.contains("screen")) {
                if let Some(href) = el.attr("href") {
                    info.styles.push(StyleSource::External(href.to_string()));
                }
            }
        }
        "base" if info.base.is_none() => info.base = el.attr("href").map(str::to_string),
        _ => {}
    }
    for child in &el.children {
        walk(child, info);
    }
}

fn text_content(el: &Element) -> String {
    el.children
        .iter()
        .filter_map(|c| match c {
            Node::Text(t) => Some(t.as_str()),
            Node::Element(_) => None,
        })
        .collect()
}
