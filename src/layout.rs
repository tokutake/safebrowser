//! Block/inline flow layout, producing a display list and link hit areas.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use crate::dom::{Element, Node};
use crate::font::Fonts;
use crate::image::Image;
use crate::style::{Align, AlignItems, Display, Float, GridLine, Position, GridPlacement, Justify, Len, MAX_GRID_TRACKS, Style, StyledNode, Track, TrackSize};

#[derive(Clone)]
pub enum Item {
    Rect { x: f32, y: f32, w: f32, h: f32, color: u32 },
    /// y is the baseline.
    Text { x: f32, y: f32, text: String, size: f32, bold: bool, color: u32 },
    Image { x: f32, y: f32, w: f32, h: f32, image: Arc<Image> },
}

/// A clickable area of the page.
#[derive(Clone)]
pub struct LinkBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub target: Target,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Target {
    Href(Rc<str>),
    /// A submit button; the value is the button's `Element::id`.
    Submit(usize),
    /// A text field. The chrome draws it with the user's input on top of the layout.
    Field(Rc<Field>),
    /// A checkbox, radio button or select box; clicking changes its state.
    Control(Rc<Control>),
}

#[derive(PartialEq, Debug)]
pub enum ControlKind {
    Checkbox,
    Radio,
    /// Option labels
    Select(Vec<String>),
}

#[derive(PartialEq, Debug)]
pub struct Control {
    /// `Element::id` of the <input> / <select>
    pub id: usize,
    pub kind: ControlKind,
    /// Initial state: 1 when checked, or the selected option's index.
    pub initial: usize,
    pub size: f32,
    pub color: u32,
}

#[derive(PartialEq, Debug)]
pub struct Field {
    /// `Element::id` of the <input> / <textarea>
    pub id: usize,
    pub initial: String,
    pub placeholder: String,
    pub size: f32,
    pub color: u32,
    /// Password fields are masked.
    pub secret: bool,
}

#[derive(Default)]
pub struct PageLayout {
    pub items: Vec<Item>,
    pub links: Vec<LinkBox>,
    /// id attribute -> element top (for scrolling to #fragment).
    pub anchors: HashMap<String, f32>,
    /// Background of html/body, painted over the whole viewport.
    pub background: Option<u32>,
    pub height: f32,
}

impl PageLayout {
    pub fn link_at(&self, x: f32, y: f32) -> Option<&Target> {
        self.links
            .iter()
            .find(|l| x >= l.x && x < l.x + l.w && y >= l.y && y < l.y + l.h)
            .map(|l| &l.target)
    }
}

/// `viewport_height` is used for `position: fixed` boxes anchored to the bottom.
pub fn layout(root: &StyledNode, width: f32, viewport_height: f32, fonts: &mut Fonts, images: &Images) -> PageLayout {
    let mut layouter = Layouter {
        images,
        out: PageLayout::default(),
        fonts,
        link: None,
        inline_width: width,
        measuring: false,
        intrinsic: HashMap::new(),
        floats: Vec::new(),
        float_layer: Vec::new(),
        positioned_layer: Vec::new(),
        containing_blocks: vec![ContainingBlock { x: 0.0, y: 0.0, w: width, h: Some(viewport_height) }],
    };
    let (height, _, _) = layouter.block(root, 0.0, 0.0, width, None);
    let float_bottom = layouter.floats.iter().map(|f| f.y1).fold(0.0, f32::max);
    layouter.out.height = height.max(float_bottom);
    // Floats paint above the backgrounds of the blocks that follow them
    let mut floats = std::mem::take(&mut layouter.float_layer);
    layouter.out.items.append(&mut floats);
    // Positioned boxes paint above everything in flow
    let mut positioned = std::mem::take(&mut layouter.positioned_layer);
    layouter.out.items.append(&mut positioned);
    layouter.out
}

const BASELINE_RATIO: f32 = 0.88;
/// Slack when checking whether text fits, so a box sized exactly to its content
/// does not wrap because of floating-point rounding.
const FIT_EPSILON: f32 = 0.5;
/// Grids larger than this are stacked vertically instead: auto-placement cost grows with
/// items × rows × columns, and a hostile page must not be able to stall layout.
const MAX_GRID_ITEMS: usize = 2000;
const MAX_GRID_ROW_SPAN: usize = 32;
const PLACEHOLDER_COLOR: u32 = 0x6b6b6b;

#[derive(Clone)]
struct TextStyle {
    size: f32,
    bold: bool,
    color: u32,
    underline: bool,
    line_height: f32,
}

impl From<&Style> for TextStyle {
    fn from(s: &Style) -> Self {
        TextStyle { size: s.font_size, bold: s.bold, color: s.color, underline: s.underline, line_height: s.line_height }
    }
}

enum Frag {
    Word(String, TextStyle, Option<Target>),
    /// A form control, placed in the line like a word.
    Widget(Widget, TextStyle),
    Space(TextStyle),
    Break(TextStyle),
}

/// Box size and kind of an inline form control.
#[derive(Clone)]
struct Widget {
    w: f32,
    /// Narrowest width it could take (differs from `w` only while measuring inline-blocks).
    min_w: f32,
    h: f32,
    kind: WidgetKind,
}

#[derive(Clone)]
enum WidgetKind {
    Field(Rc<Field>),
    Control(Rc<Control>),
    /// A button or select box with a fixed label; clickable when `target` is set.
    Button { label: String, target: Option<Target> },
    /// An inline-block laid out at the origin; moved into place when its line is flushed.
    Atomic(Box<(Vec<Item>, Vec<LinkBox>)>),
    Image { image: Arc<Image>, target: Option<Target> },
}

struct Placed {
    widget: Option<Widget>,
    x: f32,
    /// Width of the preceding space (to join link underlines across words).
    lead: f32,
    w: f32,
    text: String,
    style: TextStyle,
    href: Option<Target>,
}

/// Decoded images by the URL the page refers to them with (see `dom::image_source`).
pub type Images = HashMap<String, Arc<Image>>;

struct Layouter<'f> {
    images: &'f Images,
    out: PageLayout,
    fonts: &'f mut Fonts,
    /// Target of the enclosing <a> or <button>, so links wrapping block content stay clickable.
    link: Option<Target>,
    /// Width of the line box being collected, for percentage-sized form controls.
    inline_width: f32,
    /// Set while computing intrinsic sizes: there is no line width yet, so inline-blocks
    /// report their content sizes instead of being laid out.
    measuring: bool,
    /// (min-content, max-content) widths keyed by node address, valid for one layout pass.
    intrinsic: HashMap<usize, (f32, f32)>,
    /// Floats placed so far, in page coordinates. Lines and new formatting contexts avoid them.
    floats: Vec<FloatBox>,
    /// Draw commands of floats, painted after in-flow content.
    float_layer: Vec<Item>,
    /// Draw commands of absolutely positioned boxes, painted last.
    positioned_layer: Vec<Item>,
    /// Padding boxes of positioned ancestors; the first entry is the initial containing block.
    containing_blocks: Vec<ContainingBlock>,
}

/// The box that absolutely positioned descendants are placed against. Its height is only
/// known for the initial containing block, so `bottom` elsewhere falls back to the static position.
#[derive(Clone, Copy)]
struct ContainingBlock {
    x: f32,
    y: f32,
    w: f32,
    h: Option<f32>,
}

#[derive(Clone, Copy)]
struct FloatBox {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    left: bool,
}

/// How a block's width is decided.
#[derive(Clone, Copy, PartialEq)]
enum Sizing {
    /// Normal flow: fill the available width unless `width` says otherwise.
    Flow,
    /// Flex items and table cells: the container fixes the outer width, and
    /// percentages resolve against the container instead of the item itself.
    Fixed { pct_base: f32 },
}

/// One flex item: an element, or a run of bare text inside the container.
struct FlexItem<'n, 'a> {
    node: &'n StyledNode<'a>,
    /// Outer main size (margins included) before growing or shrinking.
    base: f32,
    min: f32,
    grow: f32,
    shrink: f32,
    auto_left: bool,
    auto_right: bool,
}

impl Layouter<'_> {
    /// Lays out a block in normal flow; returns (height including margins, top margin, bottom margin).
    fn block(&mut self, sn: &StyledNode, x: f32, y: f32, avail: f32, marker: Option<String>) -> (f32, f32, f32) {
        self.block_sized(sn, x, y, avail, marker, Sizing::Flow)
    }

    fn block_sized(&mut self, sn: &StyledNode, x: f32, y: f32, avail: f32, marker: Option<String>, sizing: Sizing) -> (f32, f32, f32) {
        let s = &sn.style;
        let pct_base = match sizing {
            Sizing::Flow => avail,
            Sizing::Fixed { pct_base } => pct_base,
        };
        let items_start = self.out.items.len();
        let links_start = self.out.links.len();
        // position: relative shifts the box after layout, without affecting its neighbors
        let (rel_dx, rel_dy) = if s.position == Position::Relative {
            let offset = |a: Len, b: Len, base: f32| match (a, b) {
                (Len::Auto, Len::Auto) => 0.0,
                (Len::Auto, b) => -b.resolve(base),
                (a, _) => a.resolve(base),
            };
            (offset(s.inset[3], s.inset[1], pct_base), offset(s.inset[0], s.inset[2], 0.0))
        } else {
            (0.0, 0.0)
        };
        let [mt, mr, mb, ml] = s.margin.map(|m| m.resolve(pct_base));
        let [pt, pr, pb, pl] = s.padding.map(|p| p.resolve(pct_base));
        let [bt, br, bb, bl] = s.border();
        let frame = pl + pr + bl + br;

        let mut width = match (sizing, s.width) {
            (Sizing::Fixed { .. }, _) | (Sizing::Flow, Len::Auto) => avail - ml - mr - frame,
            (Sizing::Flow, w) => w.resolve(pct_base),
        };
        if sizing == Sizing::Flow {
            if s.max_width != Len::Auto {
                width = width.min(s.max_width.resolve(pct_base));
            }
            // Tables without an explicit width shrink to fit their content
            if s.display == Display::Table && s.width == Len::Auto {
                width = width.min(self.intrinsic(sn).1 - ml - mr - frame);
            }
        }
        let width = width.max(0.0);

        let mut left = x + ml;
        if sizing == Sizing::Flow {
            match (s.margin[3], s.margin[1]) {
                (Len::Auto, Len::Auto) => left = x + ((avail - width - frame) / 2.0).max(0.0),
                (Len::Auto, _) => left = x + (avail - width - frame - mr).max(0.0),
                _ => {}
            }
        }
        let top = y + mt;

        if let Node::Element(el) = sn.node {
            if let Some(id) = el.attr("id") {
                self.out.anchors.entry(id.to_string()).or_insert(top);
            }
        }

        // The background must be painted before children: reserve a slot, fill it once the height is known
        let is_root = matches!(sn.tag(), Some("html" | "body"));
        let background = match s.background {
            Some(color) if is_root && self.out.background.is_none() => {
                self.out.background = Some(color);
                None
            }
            other => other,
        };
        let bg_slot = background.map(|_| {
            self.out.items.push(Item::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0, color: 0 });
            self.out.items.len() - 1
        });

        let content_x = left + bl + pl;
        let content_y = top + bt + pt;

        if let Some(marker) = marker {
            let mw = self.fonts.measure(&marker, s.font_size, false);
            let line_h = s.font_size * s.line_height;
            self.out.items.push(Item::Text {
                x: content_x - mw - 6.0,
                y: content_y + (line_h - s.font_size) / 2.0 + s.font_size * BASELINE_RATIO,
                text: marker,
                size: s.font_size,
                bold: false,
                color: s.color,
            });
        }

        let saved_link = self.link.clone();
        if let Node::Element(el) = sn.node {
            if let Some(target) = element_target(el) {
                self.link = Some(target);
            }
        }
        let positioned = s.position != Position::Static;
        if positioned {
            self.containing_blocks.push(ContainingBlock {
                x: left + bl + rel_dx,
                y: top + bt + rel_dy,
                w: width + pl + pr,
                h: None,
            });
        }
        let content_h = self.children(sn, content_x, content_y, width);
        if positioned {
            self.containing_blocks.pop();
        }
        self.link = saved_link;
        let outer_w = width + frame;
        let height = bt + pt + content_h + pb + bb;

        if let (Some(slot), Some(color)) = (bg_slot, background) {
            self.out.items[slot] = Item::Rect { x: left, y: top, w: outer_w, h: height, color };
        }
        let c = s.border_color;
        for (w, rect) in [
            (bt, (left, top, outer_w, bt)),
            (bb, (left, top + height - bb, outer_w, bb)),
            (bl, (left, top, bl, height)),
            (br, (left + outer_w - br, top, br, height)),
        ] {
            if w > 0.0 {
                self.out.items.push(Item::Rect { x: rect.0, y: rect.1, w: rect.2, h: rect.3, color: c });
            }
        }
        if rel_dx != 0.0 || rel_dy != 0.0 {
            offset_items(&mut self.out.items[items_start..], rel_dx, rel_dy);
            offset_links(&mut self.out.links[links_start..], rel_dx, rel_dy);
        }
        (mt + height + mb, mt, mb)
    }

    /// Lays out children and returns the content height.
    fn children(&mut self, sn: &StyledNode, x: f32, y: f32, width: f32) -> f32 {
        if sn.style.display == Display::TableRow {
            // Table row: cells side by side with equal widths
            let cells: Vec<&StyledNode> = sn.children.iter().filter(|c| matches!(c.node, Node::Element(_))).collect();
            if cells.is_empty() {
                return 0.0;
            }
            let cell_w = width / cells.len() as f32;
            return cells
                .iter()
                .enumerate()
                .map(|(i, cell)| self.block(cell, x + cell_w * i as f32, y, cell_w, None).0)
                .fold(0.0, f32::max);
        }
        if sn.style.display == Display::Flex {
            return self.flex(sn, x, y, width);
        }
        if sn.style.display == Display::Table {
            return self.table(sn, x, y, width);
        }
        if sn.style.display == Display::Grid {
            return self.grid(sn, x, y, width);
        }

        if let Node::Element(el) = sn.node {
            if el.tag == "img" {
                // A floated or block-level image
                return match self.image_of(el) {
                    Some(image) => {
                        let (w, h) = self.image_size(&image, &sn.style, width);
                        self.out.items.push(Item::Image { x, y, w, h, image });
                        h
                    }
                    // Not loaded: its alt text is laid out like inline content
                    None => self.inline(&[sn], x, y, width, sn.style.text_align),
                };
            }
        }
        let ordered = sn.tag() == Some("ol");
        let mut counter = 0;
        let mut cy = y;
        let mut prev_margin: f32 = 0.0;
        let mut run: Vec<&StyledNode> = Vec::new();
        for child in &sn.children {
            if !child.is_block() || (is_form_control(child) && !child.is_out_of_flow() && child.style.float == Float::None) {
                run.push(child);
                continue;
            }
            if !run.is_empty() {
                let h = self.inline(&run, x, cy, width, sn.style.text_align);
                if h > 0.0 {
                    cy += h;
                    prev_margin = 0.0;
                }
                run.clear();
            }
            if child.is_out_of_flow() {
                self.place_absolute(child, x, cy);
                continue;
            }
            if child.style.float != Float::None {
                self.place_float(child, x, cy, width);
                continue;
            }
            let (clear_left, clear_right) = child.style.clear;
            if clear_left || clear_right {
                let below = self.clear_y(cy, x, width, clear_left, clear_right);
                if below > cy {
                    cy = below;
                    prev_margin = 0.0;
                }
            }
            let marker = (child.style.display == Display::ListItem && child.style.list_marker).then(|| {
                counter += 1;
                if ordered { format!("{counter}.") } else { "•".to_string() }
            });
            // Collapse adjacent vertical margins
            let mt = child.style.margin[0].resolve(width);
            cy -= prev_margin.min(mt).max(0.0);
            // Tables, flex and grid containers sit beside floats instead of under them
            let (bx, bw) = if matches!(child.style.display, Display::Table | Display::Flex | Display::Grid) {
                let (l, r) = self.band(cy, x, width);
                (l, r - l)
            } else {
                (x, width)
            };
            let (h, _, mb) = self.block(child, bx, cy, bw, marker);
            cy += h;
            prev_margin = mb;
        }
        if !run.is_empty() {
            cy += self.inline(&run, x, cy, width, sn.style.text_align);
        }
        cy - y
    }

    fn collect(&mut self, sn: &StyledNode, href: Option<Target>, out: &mut Vec<Frag>) {
        let style = TextStyle::from(&sn.style);
        if let Some(text) = &sn.generated {
            split_words(text, &style, &href, out);
            return;
        }
        match sn.node {
            Node::Text(text) if sn.style.pre => {
                for (i, line) in text.split('\n').enumerate() {
                    if i > 0 {
                        out.push(Frag::Break(style.clone()));
                    }
                    if !line.is_empty() {
                        out.push(Frag::Word(line.replace('\t', "    "), style.clone(), href.clone()));
                    }
                }
            }
            Node::Text(text) => split_words(text, &style, &href, out),
            Node::Element(el) if sn.is_atomic_inline() && !is_form_control(sn) && !matches!(el.tag.as_str(), "img" | "br") => {
                let frag = self.atomic(sn, style, href);
                out.push(frag);
            }
            Node::Element(el) => match el.tag.as_str() {
                "br" => out.push(Frag::Break(style)),
                "img" => {
                    if let Some(image) = self.image_of(el) {
                        let (w, h) = self.image_size(&image, &sn.style, self.inline_width);
                        let min_w = if matches!(sn.style.width, Len::Percent(_)) { 0.0 } else { w };
                        out.push(Frag::Widget(Widget { w, min_w, h, kind: WidgetKind::Image { image, target: href } }, style));
                        return;
                    }
                    // Not loaded (or undecodable): show the alt text instead
                    let alt = el.attr("alt").unwrap_or("").trim();
                    let label = if alt.is_empty() { "[image]".to_string() } else { format!("[image: {alt}]") };
                    split_words(&label, &TextStyle { color: PLACEHOLDER_COLOR, ..style }, &href, out);
                }
                "input" => {
                    let kind = el.attr("type").unwrap_or("text").to_ascii_lowercase();
                    let checked = el.attr("checked").is_some();
                    match kind.as_str() {
                        "hidden" => {}
                        "checkbox" | "radio" => {
                            let kind = if kind == "radio" { ControlKind::Radio } else { ControlKind::Checkbox };
                            let side = (style.size * 1.0).round().max(10.0);
                            let control = Control { id: el.id(), kind, initial: checked as usize, size: side, color: sn.style.color };
                            out.push(Frag::Widget(Widget { w: side, min_w: side, h: side, kind: WidgetKind::Control(Rc::new(control)) }, style));
                        }
                        "submit" | "image" | "button" | "reset" => {
                            let default = if kind == "reset" { "Reset" } else { "Submit" };
                            let target = matches!(kind.as_str(), "submit" | "image").then(|| Target::Submit(el.id()));
                            out.push(self.button(el.attr("value").unwrap_or(default).to_string(), target, style));
                        }
                        // No file system access from pages
                        "file" => out.push(self.button("File upload unavailable".to_string(), None, style)),
                        _ => out.push(self.text_field(el, &sn.style, 1.0, kind == "password")),
                    }
                }
                "textarea" => {
                    let rows = el.attr("rows").and_then(|r| r.parse::<f32>().ok()).unwrap_or(2.0).clamp(1.0, 20.0);
                    out.push(self.text_field(el, &sn.style, rows, false));
                }
                "select" => {
                    let options = el.options();
                    let labels: Vec<String> = options.iter().map(|o| o.text().trim().to_string()).collect();
                    let initial = el.selected_option().and_then(|s| options.iter().position(|o| std::ptr::eq(*o, s))).unwrap_or(0);
                    let widest = labels.iter().map(|l| self.fonts.measure(l, style.size, false)).fold(0.0, f32::max);
                    let w = widest + 32.0;
                    let h = style.size * 1.4 + 6.0;
                    let control = Control { id: el.id(), kind: ControlKind::Select(labels), initial, size: style.size, color: 0x202124 };
                    out.push(Frag::Widget(Widget { w, min_w: w, h, kind: WidgetKind::Control(Rc::new(control)) }, style));
                }
                _ => {
                    let href = element_target(el).or(href);
                    for child in &sn.children {
                        self.collect(child, href.clone(), out);
                    }
                }
            },
        }
    }

    fn image_of(&self, el: &Element) -> Option<Arc<Image>> {
        self.images.get(crate::dom::image_source(el)?).cloned()
    }

    /// Display size of an image: CSS or attribute sizes win, a missing side follows the
    /// aspect ratio, and the result is shrunk to fit `avail` (or `max-width`).
    fn image_size(&self, image: &Image, s: &Style, avail: f32) -> (f32, f32) {
        let (nw, nh) = (image.width as f32, image.height as f32);
        let avail = avail.max(1.0);
        let wanted_w = match s.width {
            Len::Px(w) => Some(w),
            Len::Percent(p) => Some(avail * p / 100.0),
            _ => None,
        };
        let wanted_h = match s.height {
            Len::Px(h) => Some(h),
            _ => None,
        };
        let (mut w, mut h) = match (wanted_w, wanted_h) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, w * nh / nw),
            (None, Some(h)) => (h * nw / nh, h),
            (None, None) => (nw, nh),
        };
        let mut limit = avail;
        if s.max_width != Len::Auto {
            limit = limit.min(s.max_width.resolve(avail));
        }
        if !self.measuring && w > limit {
            // An explicit height is kept only when the width was not the one being squeezed
            h = if wanted_h.is_some() && wanted_w.is_none() { h } else { h * limit / w };
            w = limit;
        }
        (w.max(1.0), h.max(1.0))
    }

    /// Places a float at the left or right edge, beside earlier floats, without advancing
    /// the flow. Its draw commands go to the float layer.
    fn place_float(&mut self, sn: &StyledNode, x: f32, y: f32, width: f32) {
        let s = &sn.style;
        let frame = s.margin[1].resolve(width) + s.margin[3].resolve(width) + s.padding[1].resolve(width) + s.padding[3].resolve(width) + s.border()[1] + s.border()[3];
        let outer = match s.width {
            Len::Auto => self.intrinsic(sn).1,
            w => w.resolve(width) + frame,
        };
        let outer = outer.clamp(0.0, width);

        let mut fy = y;
        let (mut l, mut r) = self.band(fy, x, width);
        while r - l + FIT_EPSILON < outer {
            let Some(next) = self.next_float_bottom(fy, x, width) else { break };
            fy = next;
            (l, r) = self.band(fy, x, width);
        }
        let left = s.float == Float::Left;
        let fx = if left { l } else { r - outer };

        let saved = std::mem::take(&mut self.out.items);
        let layer_start = self.float_layer.len();
        let (h, _, _) = self.block_sized(sn, fx, fy, outer, None, Sizing::Fixed { pct_base: width });
        let mine = std::mem::replace(&mut self.out.items, saved);
        // Nested floats were appended meanwhile; keep this float beneath them
        self.float_layer.splice(layer_start..layer_start, mine);
        self.floats.push(FloatBox { x0: fx, x1: fx + outer, y0: fy, y1: fy + h, left });
    }

    /// Places an absolutely positioned box against its containing block. Without offsets it
    /// stays at its static position (`static_x`, `static_y`). Its draw commands go to the
    /// positioned layer, above in-flow content.
    fn place_absolute(&mut self, sn: &StyledNode, static_x: f32, static_y: f32) {
        let s = &sn.style;
        let cb = if s.position == Position::Fixed { self.containing_blocks[0] } else { *self.containing_blocks.last().expect("initial containing block") };
        let inset = |l: Len, base: f32| match l {
            Len::Auto => None,
            l => Some(l.resolve(base)),
        };
        let (top, right, bottom, left) = (inset(s.inset[0], 0.0), inset(s.inset[1], cb.w), inset(s.inset[2], 0.0), inset(s.inset[3], cb.w));
        let frame = s.margin[1].resolve(cb.w) + s.margin[3].resolve(cb.w) + s.padding[1].resolve(cb.w) + s.padding[3].resolve(cb.w) + s.border()[1] + s.border()[3];
        let outer_w = match (s.width, left, right) {
            (Len::Auto, Some(l), Some(r)) => cb.w - l - r,
            (Len::Auto, _, _) => self.intrinsic(sn).1.min(cb.w),
            (w, _, _) => w.resolve(cb.w) + frame,
        }
        .max(0.0);
        let x = match (left, right) {
            (Some(l), _) => cb.x + l,
            (None, Some(r)) => cb.x + cb.w - r - outer_w,
            (None, None) => static_x,
        };
        let y = top.map_or(static_y, |t| cb.y + t);

        // Lay out into fresh layers so nested positioned boxes end up above this one
        let saved_items = std::mem::take(&mut self.out.items);
        let saved_layer = std::mem::take(&mut self.positioned_layer);
        let links_start = self.out.links.len();
        let (h, _, _) = self.block_sized(sn, x, y, outer_w, None, Sizing::Fixed { pct_base: cb.w });
        let mut mine = std::mem::replace(&mut self.out.items, saved_items);
        let nested = std::mem::replace(&mut self.positioned_layer, saved_layer);
        mine.extend(nested);

        if let (None, Some(b), Some(cb_h)) = (top, bottom, cb.h) {
            let dy = cb.y + cb_h - b - h - y;
            offset_items(&mut mine, 0.0, dy);
            offset_links(&mut self.out.links[links_start..], 0.0, dy);
        }
        self.positioned_layer.extend(mine);
    }

    /// Horizontal band [left, right) of a container that is free of floats at height `y`.
    fn band(&self, y: f32, x: f32, width: f32) -> (f32, f32) {
        let (mut l, mut r) = (x, x + width);
        for f in &self.floats {
            if f.y0 <= y && y < f.y1 && f.x1 > x && f.x0 < x + width {
                if f.left {
                    l = l.max(f.x1);
                } else {
                    r = r.min(f.x0);
                }
            }
        }
        (l, r.max(l))
    }

    /// The nearest float bottom below `y` among floats overlapping the container.
    fn next_float_bottom(&self, y: f32, x: f32, width: f32) -> Option<f32> {
        self.floats
            .iter()
            .filter(|f| f.y0 <= y && y < f.y1 && f.x1 > x && f.x0 < x + width)
            .map(|f| f.y1)
            .reduce(f32::min)
    }

    /// The y position below all floats on the given sides.
    fn clear_y(&self, y: f32, x: f32, width: f32, left: bool, right: bool) -> f32 {
        self.floats
            .iter()
            .filter(|f| ((f.left && left) || (!f.left && right)) && f.x1 > x && f.x0 < x + width)
            .map(|f| f.y1)
            .fold(y, f32::max)
    }

    fn update_band(&self, line: &mut LineState) {
        let (l, r) = self.band(line.cy, line.container_x, line.container_w);
        line.x = l;
        line.width = r - l;
    }

    /// Lays out a flex container's children and returns the content height.
    fn flex(&mut self, sn: &StyledNode, x: f32, y: f32, width: f32) -> f32 {
        let s = &sn.style;
        let nodes = self.in_flow_children(sn, x, y);
        if s.flex_column {
            return self.flex_column(&nodes, s, x, y, width);
        }

        let items: Vec<FlexItem> = nodes.iter().map(|node| self.flex_item(node, width)).collect();
        let mut lines: Vec<Range<usize>> = Vec::new();
        let mut start = 0;
        let mut used = 0.0;
        for (i, item) in items.iter().enumerate() {
            let gap = if i > start { s.column_gap } else { 0.0 };
            if s.flex_wrap && i > start && used + gap + item.base > width {
                lines.push(start..i);
                start = i;
                used = item.base;
            } else {
                used += gap + item.base;
            }
        }
        if start < items.len() {
            lines.push(start..items.len());
        }

        let mut cy = y;
        for (line_no, line) in lines.into_iter().enumerate() {
            if line_no > 0 {
                cy += s.row_gap;
            }
            let line_items = &items[line];
            let n = line_items.len();
            let gaps = s.column_gap * (n.saturating_sub(1)) as f32;
            let mut sizes: Vec<f32> = line_items.iter().map(|it| it.base).collect();
            let free = width - gaps - sizes.iter().sum::<f32>();
            if free > 0.0 {
                let grow: f32 = line_items.iter().map(|it| it.grow).sum();
                if grow > 0.0 {
                    for (size, it) in sizes.iter_mut().zip(line_items) {
                        *size += free * it.grow / grow;
                    }
                }
            } else if free < 0.0 {
                let weight: f32 = line_items.iter().map(|it| it.shrink * it.base).sum();
                if weight > 0.0 {
                    for (size, it) in sizes.iter_mut().zip(line_items) {
                        *size = (it.base + free * it.shrink * it.base / weight).max(it.min);
                    }
                }
            }

            let free = (width - gaps - sizes.iter().sum::<f32>()).max(0.0);
            let auto_margins = line_items.iter().map(|it| it.auto_left as usize + it.auto_right as usize).sum::<usize>();
            let auto_share = if auto_margins > 0 { free / auto_margins as f32 } else { 0.0 };
            let (lead, between) = if auto_margins > 0 {
                (0.0, 0.0)
            } else {
                match s.justify {
                    Justify::Start => (0.0, 0.0),
                    Justify::Center => (free / 2.0, 0.0),
                    Justify::End => (free, 0.0),
                    Justify::SpaceBetween if n > 1 => (0.0, free / (n - 1) as f32),
                    Justify::SpaceBetween => (0.0, 0.0),
                    Justify::SpaceAround => (free / n as f32 / 2.0, free / n as f32),
                    Justify::SpaceEvenly => (free / (n + 1) as f32, free / (n + 1) as f32),
                }
            };

            let mut cx = x + lead;
            let mut placed = Vec::with_capacity(n);
            for (it, size) in line_items.iter().zip(&sizes) {
                if it.auto_left {
                    cx += auto_share;
                }
                let marks = (self.out.items.len(), self.out.links.len());
                let h = self.flex_child(it.node, cx, cy, *size, width, s.text_align);
                placed.push((marks, h));
                cx += size + s.column_gap + between;
                if it.auto_right {
                    cx += auto_share;
                }
            }

            let line_h = placed.iter().map(|(_, h)| *h).fold(0.0, f32::max);
            if s.align_items != AlignItems::Start {
                let ends: Vec<(usize, usize)> = placed.iter().skip(1).map(|(m, _)| *m).chain([(self.out.items.len(), self.out.links.len())]).collect();
                for (((i0, l0), h), (i1, l1)) in placed.iter().zip(ends) {
                    let dy = match s.align_items {
                        AlignItems::Center => (line_h - h) / 2.0,
                        AlignItems::End => line_h - h,
                        AlignItems::Start => 0.0,
                    };
                    self.shift(*i0..i1, *l0..l1, dy);
                }
            }
            cy += line_h;
        }
        cy - y
    }

    /// Children that take part in flex/grid layout. Absolutely positioned ones are placed
    /// right away at the container's origin as their static position.
    fn in_flow_children<'n, 'a>(&mut self, sn: &'n StyledNode<'a>, x: f32, y: f32) -> Vec<&'n StyledNode<'a>> {
        let mut nodes = Vec::new();
        for child in sn.children.iter().filter(|c| !is_blank_text(c)) {
            if child.is_out_of_flow() {
                self.place_absolute(child, x, y);
            } else {
                nodes.push(child);
            }
        }
        nodes
    }

    fn flex_column(&mut self, nodes: &[&StyledNode], s: &Style, x: f32, y: f32, width: f32) -> f32 {
        let mut cy = y;
        for (i, node) in nodes.iter().enumerate() {
            if i > 0 {
                cy += s.row_gap;
            }
            // Items shrink to their content width only when aligned to the center or end
            let w = if s.align_items != AlignItems::Start && node.style.width == Len::Auto {
                self.intrinsic(node).1.min(width)
            } else {
                width
            };
            let ix = match s.align_items {
                AlignItems::Center => x + (width - w) / 2.0,
                AlignItems::End => x + width - w,
                AlignItems::Start => x,
            };
            cy += self.flex_child(node, ix, cy, w, width, s.text_align);
        }
        cy - y
    }

    fn flex_child(&mut self, node: &StyledNode, x: f32, y: f32, width: f32, container_w: f32, align: Align) -> f32 {
        match node.node {
            Node::Element(_) if !is_form_control(node) => self.block_sized(node, x, y, width, None, Sizing::Fixed { pct_base: container_w }).0,
            _ => self.inline(&[node], x, y, width, align),
        }
    }

    /// Grid layout: explicit column tracks, named areas, line-based and automatic placement.
    /// Rows are always sized to their content.
    fn grid(&mut self, sn: &StyledNode, x: f32, y: f32, width: f32) -> f32 {
        let s = &sn.style;
        let nodes = self.in_flow_children(sn, x, y);
        if nodes.len() > MAX_GRID_ITEMS {
            return self.flex_column(&nodes, s, x, y, width);
        }
        let mut tracks = expand_tracks(s.grid_columns.as_deref(), width, s.column_gap);
        if tracks.is_empty() {
            tracks.push(Track { min: TrackSize::Auto, max: TrackSize::Fr(1.0) });
        }

        // Place items: named areas and explicit lines first, then auto-placement row by row
        let mut cells: Vec<GridCell> = Vec::with_capacity(nodes.len());
        let mut pending = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            let st = &node.style;
            let area = st.grid_area.as_deref().zip(s.grid_areas.as_deref()).and_then(|(name, areas)| find_area(areas, name));
            if let Some((row, rows, col, cols)) = area {
                cells.push(GridCell { item: i, row, rows: rows.min(MAX_GRID_ROW_SPAN), col, cols });
                continue;
            }
            let (col, cols) = resolve_axis(st.grid_column, tracks.len());
            let (row, rows) = resolve_axis(st.grid_row, 0);
            let rows = rows.min(MAX_GRID_ROW_SPAN);
            match (row, col) {
                (Some(row), Some(col)) => cells.push(GridCell { item: i, row, rows, col, cols }),
                _ => pending.push((i, row, rows, col, cols)),
            }
        }
        let mut taken: HashSet<(usize, usize)> = HashSet::new();
        for c in &cells {
            mark(&mut taken, c);
        }
        let ncols = tracks.len().max(cells.iter().map(|c| c.col + c.cols).max().unwrap_or(0));
        let (mut cursor_row, mut cursor_col) = (0, 0);
        for (item, row, rows, col, cols) in pending {
            let cols = cols.min(ncols);
            let mut r = row.unwrap_or(cursor_row);
            let placed = loop {
                let start = if row.is_none() && r == cursor_row { cursor_col } else { 0 };
                let candidates: Vec<usize> = match col {
                    Some(c) => vec![c],
                    None => (start..=ncols.saturating_sub(cols)).collect(),
                };
                let fits = |c: &usize| (r..r + rows).all(|rr| (*c..*c + cols).all(|cc| !taken.contains(&(rr, cc))));
                if let Some(c) = candidates.into_iter().find(fits) {
                    break GridCell { item, row: r, rows, col: c, cols };
                }
                r += 1;
            };
            if row.is_none() && col.is_none() {
                cursor_row = placed.row;
                cursor_col = placed.col + placed.cols;
            }
            mark(&mut taken, &placed);
            cells.push(placed);
        }
        tracks.resize(ncols.max(tracks.len()), Track { min: TrackSize::Auto, max: TrackSize::Auto });

        // Size columns from the content of single-column items
        let mut content = vec![(0.0f32, 0.0f32); tracks.len()];
        for c in cells.iter().filter(|c| c.cols == 1) {
            let (min, max) = self.intrinsic(nodes[c.item]);
            content[c.col].0 = content[c.col].0.max(min);
            content[c.col].1 = content[c.col].1.max(max);
        }
        let widths = size_tracks(&tracks, &content, width, s.column_gap);
        let mut col_x = Vec::with_capacity(widths.len() + 1);
        let mut cx = x;
        for w in &widths {
            col_x.push(cx);
            cx += w + s.column_gap;
        }
        col_x.push(cx);

        // Lay out row by row; an item spanning rows only has to fit by the end of its last row
        cells.sort_by_key(|c| (c.row, c.col));
        let nrows = cells.iter().map(|c| c.row + c.rows).max().unwrap_or(0);
        let mut bottoms = vec![0.0f32; nrows];
        let mut row_y = y;
        let mut next = 0;
        for r in 0..nrows {
            if r > 0 {
                row_y += s.row_gap;
            }
            bottoms[r] = bottoms[r].max(row_y);
            while next < cells.len() && cells[next].row == r {
                let c = &cells[next];
                let w = col_x[c.col + c.cols] - col_x[c.col] - s.column_gap;
                let h = self.flex_child(nodes[c.item], col_x[c.col], row_y, w.max(0.0), width, s.text_align);
                let last = r + c.rows - 1;
                bottoms[last] = bottoms[last].max(row_y + h);
                next += 1;
            }
            row_y = row_y.max(bottoms[r]);
        }
        row_y - y
    }

    /// Table layout: column widths come from the cells' content, so columns line up across rows.
    fn table(&mut self, sn: &StyledNode, x: f32, y: f32, width: f32) -> f32 {
        let mut parts = Vec::new();
        table_parts(sn, &mut parts);
        let columns = self.table_columns(&parts);
        let widths = distribute_columns(&columns, width);

        let mut cy = y;
        for part in parts {
            if part.style.display != Display::TableRow {
                // Captions and stray content span the whole table
                cy += self.flex_child(part, x, cy, width, width, Align::Left);
                continue;
            }
            let bg_slot = part.style.background.map(|color| {
                self.out.items.push(Item::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0, color });
                self.out.items.len() - 1
            });
            let (mut cx, mut col, mut row_h) = (x, 0, 0.0f32);
            for cell in table_cells(part) {
                let span = colspan(cell);
                let w: f32 = widths.iter().skip(col).take(span).sum();
                row_h = row_h.max(self.block_sized(cell, cx, cy, w, None, Sizing::Fixed { pct_base: width }).0);
                cx += w;
                col += span;
            }
            if let Some(slot) = bg_slot {
                if let Item::Rect { color, .. } = self.out.items[slot] {
                    self.out.items[slot] = Item::Rect { x, y: cy, w: width, h: row_h, color };
                }
            }
            cy += row_h;
        }
        cy - y
    }

    /// (min-content, max-content) of each column, from cells that span a single column.
    fn table_columns(&mut self, parts: &[&StyledNode]) -> Vec<(f32, f32)> {
        let mut columns: Vec<(f32, f32)> = Vec::new();
        for row in parts.iter().filter(|p| p.style.display == Display::TableRow) {
            let mut col = 0;
            for cell in table_cells(row) {
                let span = colspan(cell);
                if columns.len() < col + span {
                    columns.resize(col + span, (0.0, 0.0));
                }
                if span == 1 {
                    let (min, max) = self.intrinsic(cell);
                    columns[col].0 = columns[col].0.max(min);
                    columns[col].1 = columns[col].1.max(max);
                }
                col += span;
            }
        }
        columns
    }

    fn flex_item<'n, 'a>(&mut self, node: &'n StyledNode<'a>, container_w: f32) -> FlexItem<'n, 'a> {
        let s = &node.style;
        let (min, max) = self.intrinsic(node);
        let (auto_left, auto_right) = (s.margin[3] == Len::Auto, s.margin[1] == Len::Auto);
        let outer = |content: f32| content + px(s.margin[1]) + px(s.margin[3]) + px(s.padding[1]) + px(s.padding[3]) + s.border()[1] + s.border()[3];
        let mut base = match (s.flex_basis, s.width) {
            (Len::Auto, Len::Auto) => max,
            (Len::Auto, w) | (w, _) => outer(w.resolve(container_w)),
        };
        if s.max_width != Len::Auto {
            base = base.min(outer(s.max_width.resolve(container_w)));
        }
        FlexItem { node, base, min: min.min(base), grow: s.flex_grow, shrink: s.flex_shrink, auto_left, auto_right }
    }

    /// Moves already emitted items and links down by `dy`.
    fn shift(&mut self, items: Range<usize>, links: Range<usize>, dy: f32) {
        if dy != 0.0 {
            offset_items(&mut self.out.items[items], 0.0, dy);
            offset_links(&mut self.out.links[links], 0.0, dy);
        }
    }

    /// (min-content, max-content) outer widths, cached per node.
    fn intrinsic(&mut self, sn: &StyledNode) -> (f32, f32) {
        let key = sn as *const StyledNode as usize;
        if let Some(&sizes) = self.intrinsic.get(&key) {
            return sizes;
        }
        let sizes = self.compute_intrinsic(sn);
        self.intrinsic.insert(key, sizes);
        sizes
    }

    fn compute_intrinsic(&mut self, sn: &StyledNode) -> (f32, f32) {
        let was_measuring = std::mem::replace(&mut self.measuring, true);
        let sizes = self.compute_intrinsic_inner(sn);
        self.measuring = was_measuring;
        sizes
    }

    fn compute_intrinsic_inner(&mut self, sn: &StyledNode) -> (f32, f32) {
        let s = &sn.style;
        let frame = match sn.node {
            Node::Text(_) => 0.0,
            Node::Element(_) => {
                px(s.margin[1]) + px(s.margin[3]) + px(s.padding[1]) + px(s.padding[3]) + s.border()[1] + s.border()[3]
            }
        };
        if let (Node::Element(_), Len::Px(w)) = (sn.node, s.width) {
            return (w + frame, w + frame);
        }
        if let Node::Element(el) = sn.node {
            if el.tag == "img" && (sn.is_block() || sn.is_atomic_inline()) {
                if let Some(image) = self.image_of(el) {
                    let w = self.image_size(&image, s, f32::MAX).0;
                    return (w + frame, w + frame);
                }
            }
        }
        if s.display == Display::Table {
            let mut parts = Vec::new();
            table_parts(sn, &mut parts);
            let columns = self.table_columns(&parts);
            let min: f32 = columns.iter().map(|c| c.0).sum();
            let max: f32 = columns.iter().map(|c| c.1).sum();
            return (min + frame, max + frame);
        }
        if !sn.is_block() && !sn.is_atomic_inline() {
            let mut frags = Vec::new();
            self.collect(sn, None, &mut frags);
            let (min, max) = self.measure_frags(&frags);
            return (min + frame, max + frame);
        }

        if s.display == Display::Grid {
            if let Some(sizes) = self.grid_intrinsic(sn) {
                return (sizes.0 + frame, sizes.1 + frame);
            }
        }
        let side_by_side = s.display == Display::TableRow || (s.display == Display::Flex && !s.flex_column);
        let (mut min, mut max) = (0.0f32, 0.0f32);
        let mut run: Vec<Frag> = Vec::new();
        let mut count = 0;
        for child in &sn.children {
            if is_blank_text(child) || child.is_out_of_flow() {
                continue;
            }
            if !side_by_side && !child.is_block() {
                self.collect(child, None, &mut run);
                continue;
            }
            let (cmin, cmax) = self.intrinsic(child);
            if side_by_side {
                min = if s.flex_wrap { min.max(cmin) } else { min + cmin };
                max += cmax;
                count += 1;
            } else {
                min = min.max(cmin);
                max = max.max(cmax);
            }
        }
        if side_by_side && count > 1 {
            max += s.column_gap * (count - 1) as f32;
        }
        let (rmin, rmax) = self.measure_frags(&run);
        (min.max(rmin) + frame, max.max(rmax) + frame)
    }

    /// Approximate content sizes of a multi-column grid: the items laid side by side plus
    /// fixed tracks and gaps. None for single-column grids, which size like blocks.
    fn grid_intrinsic(&mut self, sn: &StyledNode) -> Option<(f32, f32)> {
        let s = &sn.style;
        let template = s.grid_columns.as_deref()?;
        let ncols = template.tracks.len() + template.auto_repeat.len();
        if ncols <= 1 {
            return None;
        }
        let fixed: f32 = template
            .tracks
            .iter()
            .map(|t| match t.min {
                TrackSize::Px(px) => px,
                _ => 0.0,
            })
            .sum();
        let gaps = s.column_gap * (ncols - 1) as f32;
        let (mut min, mut max) = (0.0f32, 0.0f32);
        for child in sn.children.iter().filter(|c| !is_blank_text(c)) {
            let (cmin, cmax) = self.intrinsic(child);
            min = min.max(cmin);
            max += cmax;
        }
        Some((min + fixed + gaps, max + fixed + gaps))
    }

    /// (longest word, longest unbroken line) of a fragment list.
    fn measure_frags(&mut self, frags: &[Frag]) -> (f32, f32) {
        let (mut min, mut max, mut line, mut pending) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for frag in frags {
            match frag {
                Frag::Widget(widget, _) => {
                    min = min.max(widget.min_w);
                    line += pending + widget.w;
                    pending = 0.0;
                }
                Frag::Word(text, style, _) => {
                    let w = self.fonts.measure(text, style.size, style.bold);
                    min = min.max(w);
                    line += pending + w;
                    pending = 0.0;
                }
                Frag::Space(style) => {
                    if line > 0.0 {
                        pending = self.fonts.measure(" ", style.size, style.bold);
                    }
                }
                Frag::Break(_) => {
                    max = max.max(line);
                    line = 0.0;
                    pending = 0.0;
                }
            }
        }
        (min, max.max(line))
    }

    /// Lays out an inline-block at the origin, to be placed in a line later. Floats and
    /// positioned boxes inside it stay inside it.
    fn atomic(&mut self, sn: &StyledNode, style: TextStyle, href: Option<Target>) -> Frag {
        let s = &sn.style;
        if self.measuring {
            let (min_w, w) = self.intrinsic(sn);
            return Frag::Widget(Widget { w, min_w, h: 0.0, kind: WidgetKind::Atomic(Box::default()) }, style);
        }
        let avail = self.inline_width.max(0.0);
        let frame = s.margin[1].resolve(avail) + s.margin[3].resolve(avail) + s.padding[1].resolve(avail) + s.padding[3].resolve(avail) + s.border()[1] + s.border()[3];
        let mut outer_w = match s.width {
            Len::Auto => self.intrinsic(sn).1.min(avail),
            w => w.resolve(avail) + frame,
        };
        if s.max_width != Len::Auto {
            outer_w = outer_w.min(s.max_width.resolve(avail) + frame);
        }

        let saved_items = std::mem::take(&mut self.out.items);
        let saved_links = std::mem::take(&mut self.out.links);
        let saved_float_layer = std::mem::take(&mut self.float_layer);
        let saved_positioned = std::mem::take(&mut self.positioned_layer);
        let saved_link = std::mem::replace(&mut self.link, href);
        let floats = self.floats.len();
        let (h, _, _) = self.block_sized(sn, 0.0, 0.0, outer_w, None, Sizing::Fixed { pct_base: avail });
        self.floats.truncate(floats);
        self.link = saved_link;
        self.inline_width = avail;
        let mut items = std::mem::replace(&mut self.out.items, saved_items);
        items.extend(std::mem::replace(&mut self.float_layer, saved_float_layer));
        items.extend(std::mem::replace(&mut self.positioned_layer, saved_positioned));
        let links = std::mem::replace(&mut self.out.links, saved_links);
        Frag::Widget(Widget { w: outer_w, min_w: outer_w, h, kind: WidgetKind::Atomic(Box::new((items, links))) }, style)
    }

    fn button(&mut self, label: String, target: Option<Target>, style: TextStyle) -> Frag {
        let w = self.fonts.measure(&label, style.size, style.bold) + 16.0;
        let h = style.size * 1.4 + 6.0;
        Frag::Widget(Widget { w, min_w: w, h, kind: WidgetKind::Button { label, target } }, style)
    }

    fn text_field(&mut self, el: &Element, s: &Style, rows: f32, secret: bool) -> Frag {
        let w = match s.width {
            Len::Px(px) => px,
            Len::Percent(p) => self.inline_width * p / 100.0,
            // Block-level fields fill the line; inline ones follow the `size` attribute
            Len::Auto if s.display != Display::Inline && !s.inline_level && !self.measuring => self.inline_width,
            Len::Auto => el.attr("size").and_then(|n| n.parse::<f32>().ok()).unwrap_or(20.0).clamp(1.0, 200.0) * s.font_size * 0.55 + 12.0,
        };
        let field = Field {
            id: el.id(),
            initial: if el.tag == "textarea" { el.text() } else { el.attr("value").unwrap_or("").to_string() },
            placeholder: el.attr("placeholder").unwrap_or("").to_string(),
            size: s.font_size,
            color: s.color,
            secret,
        };
        let h = rows * s.font_size * 1.4 + 8.0;
        let w = if self.measuring { w.max(24.0) } else { w.clamp(24.0, self.inline_width.max(24.0)) };
        Frag::Widget(Widget { w, min_w: w, h, kind: WidgetKind::Field(Rc::new(field)) }, TextStyle::from(s))
    }

    /// Breaks a run of inline content into lines; returns its height.
    fn inline(&mut self, run: &[&StyledNode], x: f32, y: f32, width: f32, align: Align) -> f32 {
        self.inline_width = width;
        let mut frags = Vec::new();
        for sn in run {
            self.collect(sn, self.link.clone(), &mut frags);
        }
        if !frags.iter().any(|f| matches!(f, Frag::Word(..) | Frag::Widget(..))) {
            return 0.0;
        }

        let mut line = LineState {
            x,
            width,
            container_x: x,
            container_w: width,
            align,
            cy: y,
            cx: 0.0,
            pending_space: 0.0,
            placed: Vec::new(),
        };
        self.update_band(&mut line);
        for frag in frags {
            match frag {
                Frag::Space(style) => {
                    if !line.placed.is_empty() {
                        line.pending_space = self.fonts.measure(" ", style.size, style.bold);
                    }
                }
                Frag::Break(style) => {
                    if line.placed.is_empty() {
                        line.cy += style.size * style.line_height;
                        self.update_band(&mut line);
                    } else {
                        self.flush(&mut line);
                    }
                }
                Frag::Word(text, style, href) => {
                    let w = self.fonts.measure(&text, style.size, style.bold);
                    if !line.placed.is_empty() && line.cx + line.pending_space + w > line.width + FIT_EPSILON {
                        self.flush(&mut line);
                    }
                    // Too narrow beside a float: continue below it
                    while line.placed.is_empty() && w > line.width + FIT_EPSILON && line.width + FIT_EPSILON < line.container_w {
                        let Some(below) = self.next_float_bottom(line.cy, line.container_x, line.container_w) else { break };
                        line.cy = below;
                        self.update_band(&mut line);
                    }
                    if line.placed.is_empty() && w > line.width + FIT_EPSILON {
                        self.place_broken_word(&mut line, text, style, href);
                        continue;
                    }
                    let lead = if line.placed.is_empty() { 0.0 } else { line.pending_space };
                    line.cx += lead;
                    line.pending_space = 0.0;
                    line.placed.push(Placed { widget: None, x: line.cx, lead, w, text, style, href });
                    line.cx += w;
                }
                Frag::Widget(widget, style) => {
                    if !line.placed.is_empty() && line.cx + line.pending_space + widget.w > line.width + FIT_EPSILON {
                        self.flush(&mut line);
                    }
                    let lead = if line.placed.is_empty() { 0.0 } else { line.pending_space };
                    line.cx += lead;
                    line.pending_space = 0.0;
                    let w = widget.w;
                    line.placed.push(Placed { widget: Some(widget), x: line.cx, lead, w, text: String::new(), style, href: None });
                    line.cx += w;
                }
            }
        }
        self.flush(&mut line);
        line.cy - y
    }

    /// Breaks a word wider than the line (e.g. a URL) at character boundaries.
    fn place_broken_word(&mut self, line: &mut LineState, text: String, style: TextStyle, href: Option<Target>) {
        let mut chunk = String::new();
        let mut chunk_w = 0.0;
        for c in text.chars() {
            let cw = self.fonts.measure(c.encode_utf8(&mut [0; 4]), style.size, style.bold);
            if chunk_w + cw > line.width + FIT_EPSILON && !chunk.is_empty() {
                line.placed.push(Placed { widget: None, x: 0.0, lead: 0.0, w: chunk_w, text: std::mem::take(&mut chunk), style: style.clone(), href: href.clone() });
                line.cx = chunk_w;
                self.flush(line);
                chunk_w = 0.0;
            }
            chunk.push(c);
            chunk_w += cw;
        }
        line.placed.push(Placed { widget: None, x: 0.0, lead: 0.0, w: chunk_w, text: chunk, style, href });
        line.cx = chunk_w;
        line.pending_space = 0.0;
    }

    /// Emits a form control: fields become clickable areas drawn by the chrome, buttons are drawn here.
    fn place_widget(&mut self, widget: Widget, x: f32, y: f32, style: &TextStyle) {
        let (w, h) = (widget.w, widget.h);
        match widget.kind {
            WidgetKind::Field(field) => self.out.links.push(LinkBox { x, y, w, h, target: Target::Field(field) }),
            WidgetKind::Control(control) => self.out.links.push(LinkBox { x, y, w, h, target: Target::Control(control) }),
            WidgetKind::Atomic(content) => {
                let (mut items, mut links) = *content;
                offset_items(&mut items, x, y);
                offset_links(&mut links, x, y);
                self.out.items.append(&mut items);
                self.out.links.append(&mut links);
            }
            WidgetKind::Image { image, target } => {
                self.out.items.push(Item::Image { x, y, w, h, image });
                if let Some(target) = target {
                    self.out.links.push(LinkBox { x, y, w, h, target });
                }
            }
            WidgetKind::Button { label, target } => {
                self.out.items.push(Item::Rect { x, y, w, h, color: 0x8f8f8f });
                self.out.items.push(Item::Rect { x: x + 1.0, y: y + 1.0, w: w - 2.0, h: h - 2.0, color: 0xefefef });
                self.out.items.push(Item::Text {
                    x: x + 8.0,
                    y: y + (h - style.size) / 2.0 + style.size * BASELINE_RATIO,
                    text: label,
                    size: style.size,
                    bold: style.bold,
                    color: 0x202124,
                });
                if let Some(target) = target {
                    self.out.links.push(LinkBox { x, y, w, h, target });
                }
            }
        }
    }

    fn flush(&mut self, line: &mut LineState) {
        if line.placed.is_empty() {
            return;
        }
        let max_size = line.placed.iter().map(|p| p.style.size).fold(0.0, f32::max);
        let line_h = line
            .placed
            .iter()
            .map(|p| match &p.widget {
                None => p.style.size * p.style.line_height,
                Some(Widget { kind: WidgetKind::Atomic(_) | WidgetKind::Image { .. }, h, .. }) => *h,
                Some(w) => w.h + 4.0,
            })
            .fold(0.0, f32::max);
        let baseline = line.cy + (line_h - max_size) / 2.0 + max_size * BASELINE_RATIO;
        let offset = match line.align {
            Align::Left => 0.0,
            Align::Center => ((line.width - line.cx) / 2.0).max(0.0),
            Align::Right => (line.width - line.cx).max(0.0),
        };

        let mut prev_end: Option<f32> = None;
        let mut prev_href: Option<Target> = None;
        for p in line.placed.drain(..) {
            let px = line.x + offset + p.x;
            if let Some(widget) = p.widget {
                let top = line.cy + (line_h - widget.h) / 2.0;
                self.place_widget(widget, px, top, &p.style);
                prev_end = None;
                prev_href = None;
                continue;
            }
            if p.style.underline {
                // Underline the space between words of the same link
                let joined = p.href.is_some() && p.href == prev_href;
                let start = if joined { px - p.lead } else { px };
                let thickness = (p.style.size / 14.0).max(1.0);
                self.out.items.push(Item::Rect { x: start, y: baseline + 2.0, w: px + p.w - start, h: thickness, color: p.style.color });
            }
            prev_href = p.href.clone();
            if let Some(target) = p.href {
                self.out.links.push(LinkBox { x: px, y: line.cy, w: p.w, h: line_h, target });
            }
            // Merge adjacent words with the same style (e.g. per-character CJK) into one draw command
            if let (Some(end), Some(Item::Text { text, size, bold, color, .. })) = (prev_end, self.out.items.last_mut()) {
                if (end - px).abs() < 0.01 && *size == p.style.size && *bold == p.style.bold && *color == p.style.color && !p.style.underline {
                    text.push_str(&p.text);
                    prev_end = Some(px + p.w);
                    continue;
                }
            }
            self.out.items.push(Item::Text { x: px, y: baseline, text: p.text, size: p.style.size, bold: p.style.bold, color: p.style.color });
            prev_end = (!p.style.underline).then_some(px + p.w);
        }
        line.cy += line_h;
        line.cx = 0.0;
        line.pending_space = 0.0;
        self.update_band(line);
    }
}

struct LineState {
    /// Current line's free band (narrowed by floats)
    x: f32,
    width: f32,
    container_x: f32,
    container_w: f32,
    align: Align,
    cy: f32,
    cx: f32,
    pending_space: f32,
    placed: Vec<Placed>,
}

/// Splits text into words at whitespace; CJK text may break between any characters.
fn split_words(text: &str, style: &TextStyle, href: &Option<Target>, out: &mut Vec<Frag>) {
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut Vec<Frag>| {
        if !word.is_empty() {
            out.push(Frag::Word(std::mem::take(word), style.clone(), href.clone()));
        }
    };
    for c in text.chars() {
        if is_invisible(c) {
            continue;
        }
        if c.is_whitespace() && c != '\u{a0}' {
            flush(&mut word, out);
            if !matches!(out.last(), Some(Frag::Space(_)) | Some(Frag::Break(_)) | None) {
                out.push(Frag::Space(style.clone()));
            }
        } else if is_cjk(c) {
            flush(&mut word, out);
            out.push(Frag::Word(c.to_string(), style.clone(), href.clone()));
        } else {
            word.push(c);
        }
    }
    flush(&mut word, out);
}

struct GridCell {
    item: usize,
    row: usize,
    rows: usize,
    col: usize,
    cols: usize,
}

fn mark(taken: &mut HashSet<(usize, usize)>, c: &GridCell) {
    for r in c.row..c.row + c.rows {
        for col in c.col..c.col + c.cols {
            taken.insert((r, col));
        }
    }
}

/// The template's tracks, with `repeat(auto-fill, ...)` expanded to as many copies as fit.
fn expand_tracks(template: Option<&crate::style::GridTemplate>, width: f32, gap: f32) -> Vec<Track> {
    let Some(t) = template else { return Vec::new() };
    let mut tracks = t.tracks.clone();
    if !t.auto_repeat.is_empty() {
        let fixed = |size: TrackSize| match size {
            TrackSize::Px(px) => px,
            TrackSize::Percent(p) => width * p / 100.0,
            _ => 0.0,
        };
        let used: f32 = t.tracks.iter().map(|tr| fixed(tr.min).max(fixed(tr.max)) + gap).sum();
        let unit: f32 = t.auto_repeat.iter().map(|tr| fixed(tr.min).max(fixed(tr.max)) + gap).sum();
        let room = MAX_GRID_TRACKS.saturating_sub(tracks.len()) / t.auto_repeat.len();
        let count = if unit > 0.0 { (((width - used + gap) / unit).floor() as usize).clamp(1, room.max(1)) } else { 1 };
        let at = t.auto_repeat_at.min(tracks.len());
        let repeated: Vec<Track> = t.auto_repeat.iter().copied().cycle().take(count * t.auto_repeat.len()).collect();
        tracks.splice(at..at, repeated);
    }
    tracks
}

/// Column widths: fixed and content-based tracks first, then leftover space to `fr` tracks
/// (or, without any, spread across auto tracks).
fn size_tracks(tracks: &[Track], content: &[(f32, f32)], width: f32, gap: f32) -> Vec<f32> {
    let resolve = |size: TrackSize, i: usize, for_min: bool| match size {
        TrackSize::Px(px) => px,
        TrackSize::Percent(p) => width * p / 100.0,
        TrackSize::MinContent => content[i].0,
        TrackSize::MaxContent => content[i].1,
        TrackSize::Auto => if for_min { content[i].0 } else { content[i].1 },
        TrackSize::Fr(_) => 0.0,
    };
    let mut sizes: Vec<f32> = tracks.iter().enumerate().map(|(i, t)| resolve(t.min, i, true)).collect();
    let limits: Vec<f32> = tracks.iter().enumerate().map(|(i, t)| resolve(t.max, i, false).max(sizes[i])).collect();
    let fr = |t: &Track| match t.max {
        TrackSize::Fr(f) => f,
        _ => 0.0,
    };
    let gaps = gap * tracks.len().saturating_sub(1) as f32;

    // Grow non-flexible tracks toward their limits
    let mut free = width - gaps - sizes.iter().sum::<f32>();
    let wanted: f32 = tracks.iter().enumerate().filter(|(_, t)| fr(t) == 0.0).map(|(i, _)| limits[i] - sizes[i]).sum();
    if free > 0.0 && wanted > 0.0 {
        let ratio = (free / wanted).min(1.0);
        for (i, t) in tracks.iter().enumerate() {
            if fr(t) == 0.0 {
                sizes[i] += (limits[i] - sizes[i]) * ratio;
            }
        }
        free = width - gaps - sizes.iter().sum::<f32>();
    }

    let total_fr: f32 = tracks.iter().map(fr).sum();
    if total_fr > 0.0 {
        // Flexible tracks share what is left after the non-flexible ones
        let rest = width - gaps - tracks.iter().enumerate().filter(|(_, t)| fr(t) == 0.0).map(|(i, _)| sizes[i]).sum::<f32>();
        for (i, t) in tracks.iter().enumerate() {
            if fr(t) > 0.0 {
                sizes[i] = sizes[i].max(rest * fr(t) / total_fr);
            }
        }
    } else if free > 0.0 {
        let autos: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].max == TrackSize::Auto).collect();
        for &i in &autos {
            sizes[i] += free / autos.len() as f32;
        }
    }
    sizes
}

/// Bounding box (row, rows, col, cols) of a named area.
fn find_area(areas: &[Vec<String>], name: &str) -> Option<(usize, usize, usize, usize)> {
    let mut found: Option<(usize, usize, usize, usize)> = None;
    for (r, row) in areas.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell == name {
                let (r0, r1, c0, c1) = found.unwrap_or((r, r, c, c));
                found = Some((r0.min(r), r1.max(r), c0.min(c), c1.max(c)));
            }
        }
    }
    found.map(|(r0, r1, c0, c1)| (r0, r1 - r0 + 1, c0, c1 - c0 + 1))
}

/// Resolves a placement to (start track, span). `count` is the number of explicit tracks,
/// used for negative line numbers (0 when unknown, as for rows).
fn resolve_axis((start, end): GridPlacement, count: usize) -> (Option<usize>, usize) {
    let line = |n: i32| -> usize {
        if n > 0 { (n - 1) as usize } else { (count as i32 + 1 + n).max(0) as usize }
    };
    match (start, end) {
        (GridLine::Line(a), GridLine::Line(b)) => {
            let (a, b) = (line(a), line(b));
            (Some(a.min(b)), a.abs_diff(b).max(1))
        }
        (GridLine::Line(a), GridLine::Span(n)) => (Some(line(a)), n as usize),
        (GridLine::Line(a), GridLine::Auto) => (Some(line(a)), 1),
        (GridLine::Span(n), GridLine::Line(b)) => (Some(line(b).saturating_sub(n as usize)), n as usize),
        (GridLine::Auto, GridLine::Line(b)) => (Some(line(b).saturating_sub(1)), 1),
        (GridLine::Span(n), _) | (GridLine::Auto, GridLine::Span(n)) => (None, n as usize),
        (GridLine::Auto, GridLine::Auto) => (None, 1),
    }
}

fn offset_items(items: &mut [Item], dx: f32, dy: f32) {
    for item in items {
        match item {
            Item::Rect { x, y, .. } | Item::Text { x, y, .. } | Item::Image { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }
}

fn offset_links(links: &mut [LinkBox], dx: f32, dy: f32) {
    for link in links {
        link.x += dx;
        link.y += dy;
    }
}

/// Rows (looking through thead/tbody/tfoot) and other content of a table, in document order.
fn table_parts<'n, 'a>(sn: &'n StyledNode<'a>, out: &mut Vec<&'n StyledNode<'a>>) {
    for child in &sn.children {
        if child.style.display == Display::TableRow {
            out.push(child);
        } else if child.children.iter().any(|g| g.style.display == Display::TableRow) {
            table_parts(child, out);
        } else if !is_blank_text(child) {
            out.push(child);
        }
    }
}

fn table_cells<'n, 'a>(row: &'n StyledNode<'a>) -> impl Iterator<Item = &'n StyledNode<'a>> {
    row.children.iter().filter(|c| matches!(c.node, Node::Element(_)))
}

fn colspan(cell: &StyledNode) -> usize {
    match cell.node {
        Node::Element(el) => el.attr("colspan").and_then(|v| v.trim().parse().ok()).unwrap_or(1).clamp(1, 64),
        Node::Text(_) => 1,
    }
}

/// Splits `width` between columns: extra space goes to columns in proportion to their
/// content, and when space is short, each column keeps at least its min-content width.
fn distribute_columns(columns: &[(f32, f32)], width: f32) -> Vec<f32> {
    let sum_min: f32 = columns.iter().map(|c| c.0).sum();
    let sum_max: f32 = columns.iter().map(|c| c.1).sum();
    columns
        .iter()
        .map(|&(min, max)| {
            if width >= sum_max {
                if sum_max > 0.0 { max * width / sum_max } else { width / columns.len() as f32 }
            } else if width >= sum_min && sum_max > sum_min {
                min + (width - sum_min) * (max - min) / (sum_max - sum_min)
            } else {
                min
            }
        })
        .collect()
}

/// What clicking inside `el` does: follow its href, or submit its form.
fn element_target(el: &Element) -> Option<Target> {
    match el.tag.as_str() {
        "a" => el.attr("href").map(|h| Target::Href(Rc::from(h))),
        "button" if !matches!(el.attr("type").map(str::to_ascii_lowercase).as_deref(), Some("button" | "reset")) => {
            Some(Target::Submit(el.id()))
        }
        _ => None,
    }
}

/// Replaced form controls have no children to lay out; they always go through inline layout.
fn is_form_control(sn: &StyledNode) -> bool {
    matches!(sn.tag(), Some("input" | "textarea" | "select"))
}

fn is_blank_text(sn: &StyledNode) -> bool {
    match (&sn.generated, sn.node) {
        (Some(text), _) => text.trim().is_empty(),
        (None, Node::Text(t)) => t.trim().is_empty(),
        (None, Node::Element(_)) => false,
    }
}

/// Pixel value of a length; percentages and `auto` count as zero for intrinsic sizing.
fn px(l: Len) -> f32 {
    match l {
        Len::Px(p) => p,
        _ => 0.0,
    }
}

/// Variation selectors and zero-width characters (they render as tofu in some fonts).
fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0xFE00..=0xFE0F | 0x200B..=0x200D | 0x2060 | 0xFEFF | 0xAD)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3000..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn lay_out(html: &str, width: f32) -> PageLayout {
        let dom = crate::html::parse(html);
        let info = crate::dom::doc_info(&dom);
        let sheets: Vec<_> = info
            .styles
            .iter()
            .filter_map(|s| match s {
                crate::dom::StyleSource::Inline(css) => Some(crate::css::parse_stylesheet(css)),
                crate::dom::StyleSource::External(_) => None,
            })
            .collect();
        let styled = crate::style::style_tree(&dom, &sheets, width).unwrap();
        let mut fonts = Fonts::load().unwrap();
        layout(&styled, width, 600.0, &mut fonts, &Images::new())
    }

    pub(super) fn texts(layout: &PageLayout) -> Vec<(String, f32, f32)> {
        layout
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Text { x, y, text, .. } => Some((text.clone(), *x, *y)),
                Item::Rect { .. } | Item::Image { .. } => None,
            })
            .collect()
    }

    #[test]
    fn flex_row_places_items_side_by_side() {
        let l = lay_out("<style>body{margin:0} nav{display:flex; gap:10px}</style><nav><a href=/a>One</a><a href=/b>Two</a><a href=/c>Three</a></nav>", 800.0);
        let t = texts(&l);
        assert_eq!(t.len(), 3);
        assert!(t.windows(2).all(|w| w[0].2 == w[1].2), "same baseline: {t:?}");
        assert!(t.windows(2).all(|w| w[0].1 < w[1].1), "left to right: {t:?}");
        assert_eq!(l.links.len(), 3);
    }

    #[test]
    fn flex_grow_and_space_between() {
        let l = lay_out("<style>body{margin:0} div{display:flex; justify-content:space-between}</style><div><span>L</span><span>R</span></div>", 800.0);
        let t = texts(&l);
        assert_eq!(t[0].1, 0.0);
        assert!(t[1].1 > 780.0, "right item pushed to the end: {t:?}");

        let l = lay_out("<style>body{margin:0} .c{display:flex} .a{flex:1} .b{flex:1}</style><div class=c><div class=a>A</div><div class=b>B</div></div>", 800.0);
        let t = texts(&l);
        assert_eq!(t[1].1, 400.0, "two equal columns: {t:?}");
    }

    #[test]
    fn flex_wrap_moves_items_to_next_line() {
        let l = lay_out("<style>body{margin:0} .c{display:flex; flex-wrap:wrap} .c div{width:300px}</style><div class=c><div>A</div><div>B</div><div>C</div></div>", 700.0);
        let t = texts(&l);
        assert_eq!(t[0].2, t[1].2);
        assert!(t[2].2 > t[0].2, "third item wraps: {t:?}");
    }

    #[test]
    fn links_wrapping_blocks_stay_clickable() {
        let l = lay_out("<a href=/card><div>Card title</div></a>", 800.0);
        assert!(l.links.iter().any(|link| link.target == Target::Href(Rc::from("/card"))));
    }
}

#[cfg(test)]
mod grid_tests {
    use super::tests::{lay_out, texts};

    #[test]
    fn grid_columns_with_fr_and_fixed_tracks() {
        let l = lay_out("<style>body{margin:0} .g{display:grid; grid-template-columns:200px 1fr 1fr}</style><div class=g><div>A</div><div>B</div><div>C</div><div>D</div></div>", 1000.0);
        let t = texts(&l);
        assert_eq!((t[0].1, t[1].1, t[2].1), (0.0, 200.0, 600.0), "{t:?}");
        assert_eq!(t[3].1, 0.0, "fourth item wraps to the next row: {t:?}");
        assert!(t[3].2 > t[0].2);
    }

    #[test]
    fn grid_template_areas() {
        let l = lay_out(
            "<style>body{margin:0} .g{display:grid; grid-template-columns:100px 1fr; grid-template-areas:'head head' 'side main'}
             .h{grid-area:head} .s{grid-area:side} .m{grid-area:main}</style>
             <div class=g><div class=m>Main</div><div class=s>Side</div><div class=h>Head</div></div>",
            800.0,
        );
        let find = |name: &str| texts(&l).into_iter().find(|t| t.0 == name).unwrap();
        let (head, side, main) = (find("Head"), find("Side"), find("Main"));
        assert_eq!((head.1, side.1, main.1), (0.0, 0.0, 100.0));
        assert!(side.2 > head.2 && side.2 == main.2);
    }

    #[test]
    fn grid_auto_fill_and_span() {
        let l = lay_out(
            "<style>body{margin:0} .g{display:grid; grid-template-columns:repeat(auto-fill, minmax(200px, 1fr))} .w{grid-column:1 / -1}</style>
             <div class=g><div class=w>Wide</div><div>A</div><div>B</div><div>C</div><div>D</div></div>",
            850.0,
        );
        let t = texts(&l);
        // 850px fits four 200px columns
        assert_eq!(t[0].0, "Wide");
        assert!(t[1..5].iter().all(|x| x.2 == t[1].2), "A-D share a row: {t:?}");
        assert!(t[1].2 > t[0].2);
    }

    #[test]
    fn huge_spans_stay_bounded() {
        let items = "<div style='grid-row: span 100; grid-column: span 100'>x</div>".repeat(300);
        let html = format!("<style>.g{{display:grid; grid-template-columns:repeat(100, 1px)}}</style><div class=g>{items}</div>");
        let start = std::time::Instant::now();
        lay_out(&html, 800.0);
        assert!(start.elapsed().as_secs() < 5);
    }
}

#[cfg(test)]
mod generated_content_tests {
    use super::tests::{lay_out, texts};

    fn joined(html: &str) -> String {
        texts(&lay_out(html, 800.0)).into_iter().map(|t| t.0).collect::<Vec<_>>().join("|")
    }

    #[test]
    fn before_and_after_insert_text() {
        let out = joined("<style>li{display:inline} li+li::before, .sep::before{content:' · '} q::before{content:open-quote} q::after{content:close-quote}</style><p><span>a</span><span class=sep>b</span> <q>hi</q></p>");
        assert!(out.contains('·'), "{out}");
        assert!(out.contains('“') && out.contains('”'), "{out}");
    }

    #[test]
    fn escapes_attr_and_empty_content() {
        let out = joined(r#"<style>.b::before{content:"\2022 "} .t::after{content:" (" attr(title) ")"} .clear::after{content:""} .icon::before{content:"\f101"}</style><p class=b>x</p><p class=t title=T>y</p><p class=clear>z</p><p class=icon>w</p>"#);
        assert!(out.contains('•'), "{out}");
        assert!(out.contains("(T)"), "{out}");
        assert!(!out.contains('\u{f101}'), "icon glyphs are dropped: {out}");
    }
}

#[cfg(test)]
mod float_tests {
    use super::tests::{lay_out, texts};

    #[test]
    fn text_wraps_around_a_right_float() {
        let l = lay_out(
            "<style>body{margin:0} .f{float:right; width:300px}</style><div class=f>Box</div><p>one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen</p>",
            600.0,
        );
        let t = texts(&l);
        let boxed = t.iter().find(|x| x.0 == "Box").unwrap();
        assert_eq!(boxed.1, 300.0, "float sits at the right edge: {t:?}");
        let words: Vec<_> = t.iter().filter(|x| x.0 != "Box").collect();
        assert!(words.iter().all(|w| w.1 < 300.0 || w.2 > boxed.2 + 30.0), "text avoids the float: {t:?}");
    }

    #[test]
    fn clear_moves_below_floats() {
        let l = lay_out(
            "<style>body{margin:0} .f{float:left; width:100px} .c{clear:both}</style><div class=f>A<br>B<br>C</div><div class=c>After</div>",
            600.0,
        );
        let t = texts(&l);
        let c = t.iter().find(|x| x.0 == "C").unwrap();
        let after = t.iter().find(|x| x.0 == "After").unwrap();
        assert!(after.2 > c.2, "{t:?}");
        assert_eq!(after.1, 0.0);
    }
}

#[cfg(test)]
mod position_tests {
    use super::tests::{lay_out, texts};

    fn find(html: &str, word: &str) -> (f32, f32) {
        let t = texts(&lay_out(html, 800.0));
        let hit = t.iter().find(|x| x.0 == word).unwrap_or_else(|| panic!("{word} not found in {t:?}"));
        (hit.1, hit.2)
    }

    #[test]
    fn absolute_is_out_of_flow_and_placed_against_positioned_ancestor() {
        let html = "<style>body{margin:0} .box{position:relative; margin-top:100px; height:50px} .tag{position:absolute; top:10px; right:0}</style>
                    <div class=box><span class=tag>Tag</span>Body</div><p>After</p>";
        let (tag_x, tag_y) = find(html, "Tag");
        let (body_x, body_y) = find(html, "Body");
        assert!(tag_x > 700.0, "right-aligned in the box: {tag_x}");
        assert!(tag_y > 100.0 && tag_y < body_y + 20.0);
        assert_eq!(body_x, 0.0, "the absolute box takes no space in flow");
    }

    #[test]
    fn offscreen_skip_links_are_moved_away() {
        let (x, _) = find("<style>.skip{position:absolute; left:-9999px}</style><a class=skip href=#main>Skip</a><p>Hello</p>", "Skip");
        assert!(x < -9000.0);
    }

    #[test]
    fn relative_shifts_without_moving_neighbors() {
        let html = "<style>body{margin:0} .r{position:relative; left:20px; top:5px}</style><p class=r>Moved</p><p>Next</p>";
        let plain = "<style>body{margin:0}</style><p>Moved</p><p>Next</p>";
        let (mx, my) = find(html, "Moved");
        let (px, py) = find(plain, "Moved");
        assert_eq!((mx - px, my - py), (20.0, 5.0));
        assert_eq!(find(html, "Next"), find(plain, "Next"));
    }

    #[test]
    fn fixed_bottom_uses_viewport_height() {
        let (_, y) = find("<style>.bar{position:fixed; bottom:0; left:0}</style><div class=bar>Cookie</div>", "Cookie");
        assert!(y > 550.0 && y <= 600.0, "anchored to the 600px viewport bottom: {y}");
    }
}

#[cfg(test)]
mod form_layout_tests {
    use super::tests::lay_out;
    use super::Target;

    #[test]
    fn inputs_become_fields_and_buttons() {
        let l = lay_out(
            "<form><input name=q placeholder=Search style='width:300px'><input type=submit value=Go><button>Send</button>
             <div style='display:flex'><input name=a style='width:100%'></div></form>",
            800.0,
        );
        let fields: Vec<_> = l.links.iter().filter(|b| matches!(b.target, Target::Field(_))).collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].w, 300.0);
        assert!(fields[1].w > 700.0, "percentage width inside a flex item: {}", fields[1].w);
        let submits = l.links.iter().filter(|b| matches!(b.target, Target::Submit(_))).count();
        assert_eq!(submits, 2, "<input type=submit> and <button>");
    }
}

#[cfg(test)]
mod inline_block_tests {
    use super::tests::{lay_out, texts};
    use super::{Item, Target};

    #[test]
    fn inline_blocks_keep_box_styles_and_sit_in_the_line() {
        let l = lay_out(
            "<style>body{margin:0} .pill{display:inline-block; padding:4px 12px; background:#eee; border:1px solid #ccc}</style>
             <p>Tags: <span class=pill>alpha</span> <span class=pill>beta</span> end</p>",
            800.0,
        );
        let t = texts(&l);
        let y = |w: &str| t.iter().find(|x| x.0 == w).unwrap().2;
        let x = |w: &str| t.iter().find(|x| x.0 == w).unwrap().1;
        assert!((y("Tags:") - y("alpha")).abs() < 6.0, "same line: {t:?}");
        assert!(x("alpha") > x("Tags:") && x("beta") > x("alpha") + 40.0 && x("end") > x("beta"));
        let backgrounds = l.items.iter().filter(|i| matches!(i, Item::Rect { color: 0xeeeeee, .. })).count();
        assert_eq!(backgrounds, 2, "each pill paints its background");
    }

    #[test]
    fn inline_block_buttons_still_submit() {
        let l = lay_out("<form><button>Go</button></form>", 800.0);
        assert!(l.links.iter().any(|b| matches!(b.target, Target::Submit(_))));
    }

    #[test]
    fn measuring_ignores_the_previous_line_width() {
        // A narrow box first leaves a small line width behind; tab labels must not shrink to it
        let l = lay_out(
            "<style>body{margin:0} ul{display:flex; padding:0} li{display:flex} a{display:inline-flex; padding:0 8px}</style>
             <div style='width:30px'>x</div><ul><li><a>Pull requests</a></li><li><a>Security and quality</a></li></ul>",
            800.0,
        );
        let t = texts(&l);
        let y = |w: &str| t.iter().find(|x| x.0 == w).unwrap().2;
        assert_eq!(y("Pull"), y("requests"), "{t:?}");
        assert_eq!(y("Security"), y("quality"), "{t:?}");
    }

    #[test]
    fn inline_flex_lays_out_children_in_a_row() {
        let l = lay_out("<style>body{margin:0} .b{display:inline-flex; gap:8px}</style><p>x <span class=b><span>A</span><span>B</span></span> y</p>", 800.0);
        let t = texts(&l);
        let a = t.iter().find(|x| x.0 == "A").unwrap();
        let b = t.iter().find(|x| x.0 == "B").unwrap();
        assert_eq!(a.2, b.2);
        assert!(b.1 > a.1);
    }
}

#[cfg(test)]
mod image_tests {
    use std::sync::Arc;

    use super::*;
    use crate::image::Image;

    fn lay_out_with(html: &str, width: f32, w: u32, h: u32) -> PageLayout {
        let dom = crate::html::parse(html);
        let sheets: Vec<_> = crate::dom::doc_info(&dom)
            .styles
            .iter()
            .filter_map(|s| match s {
                crate::dom::StyleSource::Inline(css) => Some(crate::css::parse_stylesheet(css)),
                crate::dom::StyleSource::External(_) => None,
            })
            .collect();
        let styled = crate::style::style_tree(&dom, &sheets, width).unwrap();
        let mut fonts = Fonts::load().unwrap();
        let image = Arc::new(Image { width: w, height: h, stored_w: 1, stored_h: 1, pixels: vec![0xff00ff00] });
        let images = Images::from([("a.png".to_string(), image)]);
        layout(&styled, width, 600.0, &mut fonts, &images)
    }

    fn boxes(l: &PageLayout) -> Vec<(f32, f32, f32, f32)> {
        l.items.iter().filter_map(|i| if let Item::Image { x, y, w, h, .. } = i { Some((*x, *y, *w, *h)) } else { None }).collect()
    }

    #[test]
    fn images_use_their_natural_size_or_the_given_width() {
        let l = lay_out_with(r#"<img src="a.png"><img src="a.png" width="50">"#, 800.0, 200, 100);
        let b = boxes(&l);
        assert_eq!((b[0].2, b[0].3), (200.0, 100.0));
        assert_eq!((b[1].2, b[1].3), (50.0, 25.0));
    }

    #[test]
    fn wide_images_shrink_to_the_page_keeping_their_ratio() {
        let l = lay_out_with(r#"<img src="a.png">"#, 400.0, 1000, 500);
        assert_eq!((boxes(&l)[0].2, boxes(&l)[0].3), (400.0, 200.0));
    }

    #[test]
    fn block_images_and_missing_images() {
        let l = lay_out_with(r#"<style>img{display:block}</style><img src="a.png"><img src="a.png"><img src="none.png" alt="gone">"#, 800.0, 100, 40);
        let b = boxes(&l);
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].1 - b[0].1, 40.0, "block images stack");
        let words: Vec<String> = super::tests::texts(&l).into_iter().map(|(t, ..)| t).collect();
        assert_eq!(words.join(" "), "[image: gone]");
    }

    #[test]
    fn linked_images_are_clickable() {
        let l = lay_out_with(r#"<a href="/x"><img src="a.png"></a>"#, 800.0, 30, 30);
        assert!(l.link_at(10.0, 10.0).is_some());
    }
}
