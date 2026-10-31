//! JavaScript support, built on the Boa engine (a JavaScript interpreter written in Rust).
//!
//! Scripts run once, after the page is parsed, on a throw-away thread:
//!
//! 1. The parsed tree is copied into an arena (`State`) that the scripts mutate through a
//!    small native bridge (`__dom`). `prelude.js` builds the usual DOM API on top of it.
//! 2. Scripts run in document order, then `DOMContentLoaded` / `load` fire, then timers run on
//!    a virtual clock (nothing sleeps) until they are exhausted or a budget is spent.
//! 3. The arena is converted back into a plain tree, which is styled and laid out as usual.
//!
//! What scripts cannot do, by design: no network access (`fetch`, XHR, WebSocket and dynamic
//! `<script src>` all fail), no cookies, no storage that outlives the page, no navigation,
//! no modules. Pages are not interactive after loading, so event handlers only run for
//! events the page itself dispatches.
//!
//! Hostile scripts are bounded by loop and recursion limits inside the engine, a node budget,
//! and a wall-clock watchdog: if the thread overruns, its result is discarded and the page is
//! shown as parsed.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::mpsc;
use std::time::Duration;

use boa_engine::{Context, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source, js_string};
use url::Url;

use crate::dom::{Element, Node};
use crate::html::{self, MAX_DEPTH};
use crate::selector;

const PRELUDE: &str = include_str!("prelude.js");
const MAX_NODES: usize = 400_000;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TIMER_RUNS: usize = 4000;
const WATCHDOG: Duration = Duration::from_secs(10);
const THREAD_STACK: usize = 64 * 1024 * 1024;
/// Per call frame; a loop that spins longer than this is cut off with an exception.
const LOOP_LIMIT: u64 = 5_000_000;
const RECURSION_LIMIT: usize = 400;

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
];

/// `src` attributes of the external scripts that will run, in document order.
pub fn external_sources(dom: &Node) -> Vec<String> {
    let mut out = Vec::new();
    visit(dom, &mut |el| {
        if is_runnable(el) {
            if let Some(src) = el.attr("src").map(str::trim).filter(|s| !s.is_empty()) {
                out.push(src.to_string());
            }
        }
    });
    out
}

fn visit(node: &Node, f: &mut impl FnMut(&Element)) {
    if let Node::Element(el) = node {
        f(el);
        for child in &el.children {
            visit(child, f);
        }
    }
}

/// Classic scripts run; modules, JSON and other data blocks do not. `nomodule` scripts run
/// because modules are not supported (they are the fallback for exactly that).
fn is_runnable(el: &Element) -> bool {
    if el.tag != "script" {
        return false;
    }
    match el.attr("type").map(|t| t.trim().to_ascii_lowercase()) {
        None => true,
        Some(t) => matches!(
            t.as_str(),
            "" | "text/javascript" | "application/javascript" | "application/x-javascript" | "text/ecmascript" | "application/ecmascript"
        ),
    }
}

/// Runs the page's scripts and returns the resulting tree. `external` maps `src` attributes
/// to fetched source. With JavaScript running, <noscript> content is never shown.
pub fn run(dom: Node, page_url: &Url, external: HashMap<String, String>) -> Node {
    let mut has_scripts = false;
    visit(&dom, &mut |el| has_scripts |= is_runnable(el));
    let mut result = if has_scripts {
        let backup = dom.clone();
        let url = page_url.clone();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().stack_size(THREAD_STACK).spawn(move || {
            let _ = tx.send(execute(dom, &url, &external));
        });
        match spawned {
            Ok(_) => rx.recv_timeout(WATCHDOG).unwrap_or_else(|_| {
                debug("scripts ran too long; showing the page without their changes");
                backup
            }),
            Err(_) => backup,
        }
    } else {
        dom
    };
    remove_noscript(&mut result);
    result
}

fn remove_noscript(node: &mut Node) {
    let Node::Element(el) = node else { return };
    el.children.retain(|c| !matches!(c, Node::Element(e) if e.tag == "noscript"));
    for child in &mut el.children {
        remove_noscript(child);
    }
}

fn debug(message: &str) {
    if std::env::var_os("SAFEBROWSER_JS_DEBUG").is_some() {
        eprintln!("[js] {message}");
    }
}

// ---------------------------------------------------------------------------------------------
// Arena

enum Kind {
    Document,
    Element { tag: String, attrs: Vec<(String, String)> },
    Text(String),
}

struct N {
    kind: Kind,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// A copy of the live tree in the form the selector matcher wants, valid for one `version`.
struct Snapshot {
    version: u64,
    root: usize,
    tree: Node,
    /// Arena ids of the elements of `tree` in document order.
    ids: Vec<usize>,
}

#[derive(Default)]
struct State {
    nodes: Vec<N>,
    version: u64,
    snapshot: Option<Snapshot>,
    started: HashSet<usize>,
    current_script: Option<usize>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

impl State {
    fn from_tree(root: &Node) -> State {
        let mut st = State::default();
        st.nodes.push(N { kind: Kind::Document, parent: None, children: Vec::new() });
        if let Node::Element(doc) = root {
            for child in &doc.children {
                st.import(child, 0, 1);
            }
        }
        st.ensure_structure();
        st
    }

    /// Copies `node` under `parent`, dropping what is nested deeper than the parser allows.
    fn import(&mut self, node: &Node, parent: usize, depth: usize) {
        if depth > MAX_DEPTH || self.nodes.len() >= MAX_NODES {
            return;
        }
        let kind = match node {
            Node::Text(t) => Kind::Text(t.clone()),
            Node::Element(el) => Kind::Element { tag: el.tag.clone(), attrs: el.attrs.clone() },
        };
        let id = self.nodes.len();
        self.nodes.push(N { kind, parent: Some(parent), children: Vec::new() });
        self.nodes[parent].children.push(id);
        if let Node::Element(el) = node {
            for child in &el.children {
                self.import(child, id, depth + 1);
            }
        }
    }

    /// Browsers always have html, head and body; scripts rely on that.
    fn ensure_structure(&mut self) {
        let html = match self.child_element(0, "html") {
            Some(h) => h,
            None => {
                let h = self.make_element("html");
                let existing = std::mem::take(&mut self.nodes[0].children);
                self.nodes[0].children.push(h);
                self.nodes[h].parent = Some(0);
                let body = self.make_element("body");
                self.append_raw(h, body);
                for c in existing {
                    self.nodes[c].parent = None;
                    self.append_raw(body, c);
                }
                h
            }
        };
        if self.child_element(html, "head").is_none() {
            let head = self.make_element("head");
            self.nodes[head].parent = Some(html);
            self.nodes[html].children.insert(0, head);
        }
        if self.child_element(html, "body").is_none() {
            let body = self.make_element("body");
            self.append_raw(html, body);
        }
    }

    fn make_element(&mut self, tag: &str) -> usize {
        self.nodes.push(N { kind: Kind::Element { tag: tag.to_string(), attrs: Vec::new() }, parent: None, children: Vec::new() });
        self.nodes.len() - 1
    }

    fn append_raw(&mut self, parent: usize, child: usize) {
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    fn child_element(&self, parent: usize, tag: &str) -> Option<usize> {
        self.nodes[parent].children.iter().copied().find(|&c| self.tag(c) == Some(tag))
    }

    fn tag(&self, id: usize) -> Option<&str> {
        match &self.nodes.get(id)?.kind {
            Kind::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    fn touch(&mut self) {
        self.version += 1;
    }

    fn create(&mut self, kind: Kind) -> Result<usize, &'static str> {
        if self.nodes.len() >= MAX_NODES {
            return Err("The page created too many nodes.");
        }
        self.nodes.push(N { kind, parent: None, children: Vec::new() });
        Ok(self.nodes.len() - 1)
    }

    fn detach(&mut self, id: usize) {
        if let Some(p) = self.nodes[id].parent.take() {
            self.nodes[p].children.retain(|&c| c != id);
            self.touch();
        }
    }

    fn depth(&self, mut id: usize) -> usize {
        let mut depth = 0;
        while let Some(p) = self.nodes[id].parent {
            depth += 1;
            id = p;
        }
        depth
    }

    /// Levels in the subtree at `id` (1 for a leaf), counted without recursion.
    fn height(&self, id: usize) -> usize {
        let mut best = 0;
        let mut stack = vec![(id, 1)];
        while let Some((n, h)) = stack.pop() {
            best = best.max(h);
            stack.extend(self.nodes[n].children.iter().map(|&c| (c, h + 1)));
        }
        best
    }

    fn is_inclusive_ancestor(&self, ancestor: usize, mut node: usize) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            match self.nodes[node].parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }

    fn insert(&mut self, parent: usize, child: usize, before: Option<usize>) -> Result<(), &'static str> {
        if matches!(self.nodes[parent].kind, Kind::Text(_)) || matches!(self.nodes[child].kind, Kind::Document) {
            return Err("HierarchyRequestError: this node cannot have children");
        }
        if self.is_inclusive_ancestor(child, parent) {
            return Err("HierarchyRequestError: a node cannot contain itself");
        }
        if let Some(b) = before {
            if self.nodes[b].parent != Some(parent) {
                return Err("NotFoundError: the reference node is not a child of this node");
            }
        }
        if self.depth(parent) + 1 + self.height(child) > MAX_DEPTH {
            return Err("HierarchyRequestError: the tree is nested too deeply");
        }
        self.detach(child);
        let pos = before.and_then(|b| self.nodes[parent].children.iter().position(|&c| c == b));
        match pos {
            Some(i) => self.nodes[parent].children.insert(i, child),
            None => self.nodes[parent].children.push(child),
        }
        self.nodes[child].parent = Some(parent);
        self.touch();
        Ok(())
    }

    fn text(&self, id: usize) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            match &self.nodes[n].kind {
                Kind::Text(t) => out.push_str(t),
                _ => stack.extend(self.nodes[n].children.iter().rev()),
            }
            if out.len() > MAX_TEXT_BYTES {
                break;
            }
        }
        out
    }

    fn clear_children(&mut self, id: usize) {
        for c in std::mem::take(&mut self.nodes[id].children) {
            self.nodes[c].parent = None;
        }
        self.touch();
    }

    fn set_text(&mut self, id: usize, text: &str) -> Result<(), &'static str> {
        if let Kind::Text(t) = &mut self.nodes[id].kind {
            *t = text.to_string();
            self.touch();
            return Ok(());
        }
        self.clear_children(id);
        if !text.is_empty() {
            let t = self.create(Kind::Text(text.to_string()))?;
            self.append_raw(id, t);
        }
        Ok(())
    }

    fn set_inner(&mut self, id: usize, markup: &str) -> Result<(), &'static str> {
        let nodes = html::parse_fragment(markup);
        self.clear_children(id);
        let depth = self.depth(id) + 1;
        let before = self.nodes.len();
        for node in &nodes {
            self.import(node, id, depth);
        }
        // Scripts inserted through innerHTML never run
        for n in before..self.nodes.len() {
            if self.tag(n) == Some("script") {
                self.started.insert(n);
            }
        }
        self.touch();
        Ok(())
    }

    fn clone_node(&mut self, id: usize, deep: bool) -> Result<usize, &'static str> {
        let kind = match &self.nodes[id].kind {
            Kind::Document => Kind::Document,
            Kind::Text(t) => Kind::Text(t.clone()),
            Kind::Element { tag, attrs } => Kind::Element { tag: tag.clone(), attrs: attrs.clone() },
        };
        let copy = self.create(kind)?;
        if self.started.contains(&id) {
            self.started.insert(copy);
        }
        if deep {
            for child in self.nodes[id].children.clone() {
                let c = self.clone_node(child, true)?;
                self.append_raw(copy, c);
            }
        }
        Ok(copy)
    }

    fn serialize(&self, id: usize, outer: bool, out: &mut String) {
        match &self.nodes[id].kind {
            Kind::Text(t) => {
                let raw = self.nodes[id].parent.and_then(|p| self.tag(p)).is_some_and(|t| matches!(t, "script" | "style"));
                if raw { out.push_str(t) } else { out.push_str(&escape(t, false)) }
            }
            Kind::Document => self.serialize_children(id, out),
            Kind::Element { tag, attrs } => {
                if outer {
                    out.push('<');
                    out.push_str(tag);
                    for (k, v) in attrs {
                        out.push_str(&format!(" {k}=\"{}\"", escape(v, true)));
                    }
                    out.push('>');
                }
                if !VOID_TAGS.contains(&tag.as_str()) {
                    self.serialize_children(id, out);
                    if outer {
                        out.push_str(&format!("</{tag}>"));
                    }
                }
            }
        }
    }

    fn serialize_children(&self, id: usize, out: &mut String) {
        for &c in &self.nodes[id].children {
            self.serialize(c, true, out);
        }
    }

    /// Converts a subtree back to the immutable tree type, collecting element ids in order.
    fn build(&self, id: usize, depth: usize, ids: &mut Vec<usize>) -> Option<Node> {
        if depth > MAX_DEPTH {
            return None;
        }
        Some(match &self.nodes[id].kind {
            Kind::Text(t) => Node::Text(t.clone()),
            kind => {
                let (tag, attrs) = match kind {
                    Kind::Element { tag, attrs } => (tag.clone(), attrs.clone()),
                    _ => ("#document".to_string(), Vec::new()),
                };
                ids.push(id);
                let mut el = Element::new(tag, attrs);
                el.children = self.nodes[id].children.iter().filter_map(|&c| self.build(c, depth + 1, ids)).collect();
                Node::Element(el)
            }
        })
    }

    fn top_of(&self, mut id: usize) -> usize {
        while let Some(p) = self.nodes[id].parent {
            id = p;
        }
        id
    }

    fn ensure_snapshot(&mut self, id: usize) {
        let root = self.top_of(id);
        let fresh = self.snapshot.as_ref().is_some_and(|s| s.version == self.version && s.root == root);
        if !fresh {
            let mut ids = Vec::new();
            // Text roots have no element tree; they never match anything
            let tree = self.build(root, 0, &mut ids).unwrap_or(Node::Text(String::new()));
            self.snapshot = Some(Snapshot { version: self.version, root, tree, ids });
        }
    }

    /// Elements under `scope` matching `selector`, in document order.
    fn query(&mut self, scope: usize, selector: &str, all: bool) -> Result<Vec<usize>, &'static str> {
        let selectors = selector::parse_selector_list(selector);
        if selectors.is_empty() {
            return Err("SyntaxError: the selector is not valid or not supported");
        }
        self.ensure_snapshot(scope);
        let snap = self.snapshot.as_ref().expect("just built");
        let (tree, ids, snap_root) = (&snap.tree, &snap.ids, snap.root);
        let mut found = Vec::new();
        let mut index = 0;
        let mut ancestors: Vec<&Element> = Vec::new();
        let nodes = &self.nodes;
        let in_scope = |id: usize| {
            if scope == snap_root {
                return id != scope;
            }
            let mut cur = nodes[id].parent;
            while let Some(p) = cur {
                if p == scope {
                    return true;
                }
                cur = nodes[p].parent;
            }
            false
        };
        walk(tree, &mut ancestors, &mut index, &mut |el, anc, i| {
            if (!all && !found.is_empty()) || el.tag == "#document" {
                return;
            }
            let id = ids[i];
            if selectors.iter().any(|s| s.pseudo.is_none() && selector::matches(s, el, anc)) && in_scope(id) {
                found.push(id);
            }
        });
        Ok(found)
    }

    fn matches(&mut self, id: usize, selector: &str) -> Result<bool, &'static str> {
        let selectors = selector::parse_selector_list(selector);
        if selectors.is_empty() {
            return Err("SyntaxError: the selector is not valid or not supported");
        }
        if self.tag(id).is_none() {
            return Ok(false);
        }
        self.ensure_snapshot(id);
        let snap = self.snapshot.as_ref().expect("just built");
        let mut result = false;
        let mut index = 0;
        let mut ancestors: Vec<&Element> = Vec::new();
        let ids = &snap.ids;
        walk(&snap.tree, &mut ancestors, &mut index, &mut |el, anc, i| {
            if ids[i] == id {
                result = selectors.iter().any(|s| s.pseudo.is_none() && selector::matches(s, el, anc));
            }
        });
        Ok(result)
    }

    fn by_id(&self, wanted: &str) -> Option<usize> {
        let mut stack = vec![0];
        while let Some(n) = stack.pop() {
            if let Kind::Element { attrs, .. } = &self.nodes[n].kind {
                if attrs.iter().any(|(k, v)| k == "id" && v == wanted) {
                    return Some(n);
                }
            }
            stack.extend(self.nodes[n].children.iter().rev());
        }
        None
    }

    fn find_tag(&self, wanted: &str) -> Option<usize> {
        let mut stack = vec![0];
        while let Some(n) = stack.pop() {
            if self.tag(n) == Some(wanted) {
                return Some(n);
            }
            stack.extend(self.nodes[n].children.iter().rev());
        }
        None
    }

    fn attrs_mut(&mut self, id: usize) -> Option<&mut Vec<(String, String)>> {
        match &mut self.nodes[id].kind {
            Kind::Element { attrs, .. } => Some(attrs),
            _ => None,
        }
    }

    /// Ids of the runnable scripts, in document order.
    fn scripts(&self) -> Vec<(usize, Option<String>, String)> {
        let mut out = Vec::new();
        let mut stack = vec![0];
        while let Some(n) = stack.pop() {
            if let Kind::Element { tag, attrs } = &self.nodes[n].kind {
                if tag == "script" {
                    let el = Element::new(tag.clone(), attrs.clone());
                    if is_runnable(&el) {
                        let src = el.attr("src").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                        out.push((n, src, self.text(n)));
                    }
                }
            }
            stack.extend(self.nodes[n].children.iter().rev());
        }
        out
    }
}

/// Pre-order walk calling `f(element, ancestors, element index)`.
fn walk<'a>(node: &'a Node, ancestors: &mut Vec<&'a Element>, index: &mut usize, f: &mut dyn FnMut(&'a Element, &[&'a Element], usize)) {
    let Node::Element(el) = node else { return };
    let i = *index;
    *index += 1;
    f(el, ancestors, i);
    ancestors.push(el);
    for child in &el.children {
        walk(child, ancestors, index, f);
    }
    ancestors.pop();
}

fn escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' if !attr => out.push_str("&lt;"),
            '>' if !attr => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Bridge to the engine

fn arg_string(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    match args.get(i) {
        Some(v) => Ok(v.to_string(ctx)?.to_std_string_lossy()),
        None => Ok(String::new()),
    }
}

fn arg_id(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<usize> {
    let n = match args.get(i) {
        Some(v) => v.to_number(ctx)?,
        None => -1.0,
    };
    let count = with(|st| st.nodes.len());
    if n >= 0.0 && (n as usize) < count && n.fract() == 0.0 { Ok(n as usize) } else { Err(error("TypeError: not a node")) }
}

fn arg_opt_id(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<Option<usize>> {
    match args.get(i) {
        Some(v) if v.to_number(ctx)? < 0.0 => Ok(None),
        _ => arg_id(args, i, ctx).map(Some),
    }
}

fn error(message: &str) -> JsError {
    JsNativeError::typ().with_message(message.to_string()).into()
}

fn string_value(s: String) -> JsValue {
    JsValue::from(JsString::from(s))
}

fn ids_value(ids: &[usize]) -> JsValue {
    string_value(ids.iter().map(usize::to_string).collect::<Vec<_>>().join(","))
}

fn id_value(id: Option<usize>) -> JsValue {
    JsValue::from(id.map_or(-1.0, |i| i as f64))
}

/// `__dom(op, ...args)`: the only way scripts reach the tree. `prelude.js` wraps it.
fn dom_op(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let op = arg_string(args, 0, ctx)?;
    let fail = |m: &'static str| -> JsError { error(m) };
    match op.as_str() {
        "type" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(JsValue::from(with(|st| match st.nodes[id].kind {
                Kind::Element { .. } => 1,
                Kind::Text(_) => 3,
                Kind::Document => 9,
            })))
        }
        "tag" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(with(|st| st.tag(id).map(str::to_string)).map_or(JsValue::null(), string_value))
        }
        "parent" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(id_value(with(|st| st.nodes[id].parent)))
        }
        "children" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(with(|st| ids_value(&st.nodes[id].children)))
        }
        "sibling" => {
            let id = arg_id(args, 1, ctx)?;
            let dir = args.get(2).map_or(Ok(1.0), |v| v.to_number(ctx))?;
            Ok(id_value(with(|st| {
                let siblings = &st.nodes[st.nodes[id].parent?].children;
                let i = siblings.iter().position(|&c| c == id)?;
                if dir < 0.0 { i.checked_sub(1).map(|j| siblings[j]) } else { siblings.get(i + 1).copied() }
            })))
        }
        "getattr" => {
            let id = arg_id(args, 1, ctx)?;
            let name = arg_string(args, 2, ctx)?.to_ascii_lowercase();
            Ok(with(|st| match &st.nodes[id].kind {
                Kind::Element { attrs, .. } => attrs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()),
                _ => None,
            })
            .map_or(JsValue::null(), string_value))
        }
        "setattr" => {
            let id = arg_id(args, 1, ctx)?;
            let name = arg_string(args, 2, ctx)?.to_ascii_lowercase();
            let value = arg_string(args, 3, ctx)?;
            with(|st| {
                if let Some(attrs) = st.attrs_mut(id) {
                    match attrs.iter().position(|(k, _)| *k == name) {
                        Some(i) => attrs[i].1 = value,
                        None if attrs.len() < 256 => attrs.push((name, value)),
                        None => {}
                    }
                    st.touch();
                }
            });
            Ok(JsValue::undefined())
        }
        "removeattr" => {
            let id = arg_id(args, 1, ctx)?;
            let name = arg_string(args, 2, ctx)?.to_ascii_lowercase();
            with(|st| {
                if let Some(attrs) = st.attrs_mut(id) {
                    attrs.retain(|(k, _)| *k != name);
                    st.touch();
                }
            });
            Ok(JsValue::undefined())
        }
        "attrnames" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(string_value(with(|st| match &st.nodes[id].kind {
                Kind::Element { attrs, .. } => attrs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(" "),
                _ => String::new(),
            })))
        }
        "text" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(string_value(with(|st| st.text(id))))
        }
        "settext" => {
            let id = arg_id(args, 1, ctx)?;
            let text = arg_string(args, 2, ctx)?;
            with(|st| st.set_text(id, &text)).map_err(fail)?;
            Ok(JsValue::undefined())
        }
        "create" => {
            let tag = arg_string(args, 1, ctx)?.to_ascii_lowercase();
            let id = with(|st| st.create(Kind::Element { tag, attrs: Vec::new() })).map_err(fail)?;
            Ok(JsValue::from(id as f64))
        }
        "createtext" => {
            let text = arg_string(args, 1, ctx)?;
            let id = with(|st| st.create(Kind::Text(text))).map_err(fail)?;
            Ok(JsValue::from(id as f64))
        }
        "insert" => {
            let parent = arg_id(args, 1, ctx)?;
            let child = arg_id(args, 2, ctx)?;
            let before = arg_opt_id(args, 3, ctx)?;
            with(|st| st.insert(parent, child, before)).map_err(fail)?;
            Ok(JsValue::undefined())
        }
        "remove" => {
            let id = arg_id(args, 1, ctx)?;
            with(|st| st.detach(id));
            Ok(JsValue::undefined())
        }
        "clone" => {
            let id = arg_id(args, 1, ctx)?;
            let deep = args.get(2).is_some_and(JsValue::to_boolean);
            Ok(JsValue::from(with(|st| st.clone_node(id, deep)).map_err(fail)? as f64))
        }
        "inner" | "outer" => {
            let id = arg_id(args, 1, ctx)?;
            let outer = op == "outer";
            Ok(string_value(with(|st| {
                let mut out = String::new();
                st.serialize(id, outer, &mut out);
                out
            })))
        }
        "setinner" => {
            let id = arg_id(args, 1, ctx)?;
            let markup = arg_string(args, 2, ctx)?;
            with(|st| st.set_inner(id, &markup)).map_err(fail)?;
            Ok(JsValue::undefined())
        }
        "query" => {
            let scope = arg_id(args, 1, ctx)?;
            let selector = arg_string(args, 2, ctx)?;
            let all = args.get(3).is_some_and(JsValue::to_boolean);
            let found = with(|st| st.query(scope, &selector, all)).map_err(fail)?;
            Ok(ids_value(&found))
        }
        "matches" => {
            let id = arg_id(args, 1, ctx)?;
            let selector = arg_string(args, 2, ctx)?;
            Ok(JsValue::from(with(|st| st.matches(id, &selector)).map_err(fail)?))
        }
        "byid" => {
            let wanted = arg_string(args, 1, ctx)?;
            Ok(id_value(with(|st| st.by_id(&wanted))))
        }
        "findtag" => {
            let wanted = arg_string(args, 1, ctx)?;
            Ok(id_value(with(|st| st.find_tag(&wanted))))
        }
        "connected" => {
            let id = arg_id(args, 1, ctx)?;
            Ok(JsValue::from(with(|st| st.top_of(id) == 0)))
        }
        "started" => {
            // Test-and-set: has this script already run (or been marked as never to run)?
            let id = arg_id(args, 1, ctx)?;
            Ok(JsValue::from(!with(|st| st.started.insert(id))))
        }
        "currentscript" => Ok(id_value(with(|st| st.current_script))),
        "write" => {
            let markup = arg_string(args, 1, ctx)?;
            with(|st| {
                let nodes = html::parse_fragment(&markup);
                // After the running script, or at the end of <body> once the page is loaded
                let (parent, after) = match st.current_script.and_then(|s| Some((st.nodes[s].parent?, s))) {
                    Some((p, s)) => (p, Some(s)),
                    None => (st.find_tag("body").unwrap_or(0), None),
                };
                let at = after.and_then(|s| st.nodes[parent].children.iter().position(|&c| c == s)).map_or(st.nodes[parent].children.len(), |i| i + 1);
                let tail = st.nodes[parent].children.split_off(at);
                let depth = st.depth(parent) + 1;
                for node in &nodes {
                    st.import(node, parent, depth);
                }
                st.nodes[parent].children.extend(tail);
                st.touch();
            });
            Ok(JsValue::undefined())
        }
        _ => Err(error("unknown dom operation")),
    }
}

/// `__eval(code)`: runs a classic script in the global scope (used for scripts added later).
fn eval_op(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let code = arg_string(args, 0, ctx)?;
    ctx.eval(Source::from_bytes(&code))
}

fn log_op(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let level = arg_string(args, 0, ctx)?;
    let message = arg_string(args, 1, ctx)?;
    debug(&format!("{level}: {message}"));
    Ok(JsValue::undefined())
}

/// Random bits for `crypto.getRandomValues`, from the OS-seeded hasher in std.
fn random_op(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let bits = std::collections::hash_map::RandomState::new().build_hasher().finish();
    Ok(JsValue::from((bits >> 11) as f64 / (1u64 << 53) as f64))
}

fn describe(err: &JsError, ctx: &mut Context) -> String {
    err.clone()
        .into_opaque(ctx)
        .and_then(|v| v.to_string(ctx))
        .map(|s| s.to_std_string_lossy())
        .unwrap_or_else(|_| "unknown error".to_string())
}

fn js_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || matches!(c, '\u{2028}' | '\u{2029}') => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn call_global(ctx: &mut Context, name: JsString) -> JsResult<JsValue> {
    let f = ctx.global_object().get(name, ctx)?;
    match f.as_callable() {
        Some(f) => f.call(&JsValue::undefined(), &[], ctx),
        None => Err(error("missing function")),
    }
}

fn execute(dom: Node, url: &Url, external: &HashMap<String, String>) -> Node {
    STATE.with(|s| *s.borrow_mut() = State::from_tree(&dom));
    let mut ctx = Context::default();
    ctx.runtime_limits_mut().set_loop_iteration_limit(LOOP_LIMIT);
    ctx.runtime_limits_mut().set_recursion_limit(RECURSION_LIMIT);

    for (name, len, f) in [
        (js_string!("__dom"), 2, dom_op as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
        (js_string!("__eval"), 1, eval_op),
        (js_string!("__log"), 2, log_op),
        (js_string!("__random"), 0, random_op),
    ] {
        if ctx.register_global_builtin_callable(name, len, NativeFunction::from_fn_ptr(f)).is_err() {
            return finish();
        }
    }
    if let Err(e) = ctx.eval(Source::from_bytes(PRELUDE)) {
        debug(&format!("prelude failed: {}", describe(&e, &mut ctx)));
        return finish();
    }
    let init = format!("__init({})", js_quote(url.as_str()));
    if let Err(e) = ctx.eval(Source::from_bytes(&init)) {
        debug(&format!("init failed: {}", describe(&e, &mut ctx)));
        return finish();
    }

    let scripts = with(|st| st.scripts());
    for (id, src, inline) in scripts {
        let code = match &src {
            Some(src) => match external.get(src) {
                Some(code) => code.as_str(),
                None => {
                    debug(&format!("script {src} was not loaded"));
                    continue;
                }
            },
            None => inline.as_str(),
        };
        with(|st| {
            st.started.insert(id);
            st.current_script = Some(id);
        });
        if let Err(e) = ctx.eval(Source::from_bytes(code)) {
            let message = describe(&e, &mut ctx);
            debug(&format!("script error: {message}"));
            let report = format!("__reportError({})", js_quote(&message));
            let _ = ctx.eval(Source::from_bytes(&report));
        }
        with(|st| st.current_script = None);
        let _ = ctx.run_jobs();
    }

    for step in ["__domContentLoaded", "__load"] {
        if let Err(e) = call_global(&mut ctx, JsString::from(step)) {
            debug(&format!("{step} failed: {}", describe(&e, &mut ctx)));
        }
        let _ = ctx.run_jobs();
    }
    for _ in 0..MAX_TIMER_RUNS {
        match call_global(&mut ctx, js_string!("__nextTimer")) {
            Ok(v) if v.to_boolean() => {}
            Ok(_) => break,
            Err(e) => {
                debug(&format!("timer failed: {}", describe(&e, &mut ctx)));
                break;
            }
        }
        let _ = ctx.run_jobs();
    }
    finish()
}

fn finish() -> Node {
    with(|st| st.build(0, 0, &mut Vec::new())).unwrap_or_else(|| Node::Element(Element::new("#document", Vec::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_html(source: &str) -> Node {
        let url = Url::parse("https://example.test/page?x=1#top").unwrap();
        run(html::parse(source), &url, HashMap::new())
    }

    fn find<'a>(node: &'a Node, id: &str) -> Option<&'a Element> {
        let Node::Element(el) = node else { return None };
        if el.attr("id") == Some(id) {
            return Some(el);
        }
        el.children.iter().find_map(|c| find(c, id))
    }

    fn text(el: &Element) -> String {
        el.children
            .iter()
            .map(|c| match c {
                Node::Text(t) => t.clone(),
                Node::Element(e) => text(e),
            })
            .collect()
    }

    #[test]
    fn scripts_can_change_the_page() {
        let dom = run_html(r#"<div id=a>old</div><script>document.getElementById('a').textContent = 'new ' + (1 + 2);</script>"#);
        assert_eq!(text(find(&dom, "a").unwrap()), "new 3");
    }

    #[test]
    fn create_elements_attributes_and_style() {
        let dom = run_html(
            r#"<div id=root></div><script>
            const p = document.createElement('p');
            p.id = 'made'; p.className = 'x y'; p.classList.add('z');
            p.style.backgroundColor = 'red'; p.setAttribute('data-n', '5');
            p.innerHTML = '<b>bold</b> &amp; text';
            document.getElementById('root').appendChild(p);
            </script>"#,
        );
        let p = find(&dom, "made").expect("inserted");
        assert_eq!(p.attr("class"), Some("x y z"));
        assert_eq!(p.attr("style"), Some("background-color: red;"));
        assert_eq!(p.attr("data-n"), Some("5"));
        assert_eq!(text(p), "bold & text");
    }

    #[test]
    fn query_selectors_and_events_and_timers() {
        let dom = run_html(
            r#"<ul id=list><li class=a>1</li><li class=b>2</li><li class=a>3</li></ul><p id=out></p><script>
            const hits = document.querySelectorAll('#list li.a');
            let log = [hits.length, document.querySelector('li.b').textContent];
            document.addEventListener('DOMContentLoaded', () => log.push('dcl'));
            window.addEventListener('load', () => log.push('load'));
            setTimeout(() => { log.push('t2'); document.getElementById('out').textContent = log.join(','); }, 20);
            setTimeout(() => log.push('t1'), 10);
            Promise.resolve().then(() => log.push('micro'));
            </script>"#,
        );
        assert_eq!(text(find(&dom, "out").unwrap()), "2,2,micro,dcl,load,t1,t2");
    }

    #[test]
    fn errors_do_not_stop_later_scripts() {
        let dom = run_html(r#"<p id=a></p><script>throw new Error('boom')</script><script>document.getElementById('a').textContent='ok'</script>"#);
        assert_eq!(text(find(&dom, "a").unwrap()), "ok");
    }

    #[test]
    fn globals_are_shared_between_scripts() {
        let dom = run_html(r#"<p id=a></p><script>let shared = 41; var v = 1;</script><script>document.getElementById('a').textContent = shared + v;</script>"#);
        assert_eq!(text(find(&dom, "a").unwrap()), "42");
    }

    #[test]
    fn document_write_inserts_after_the_script() {
        let dom = run_html(r#"<div id=a><script>document.write('<i id=w>hi</i>')</script></div>"#);
        assert_eq!(text(find(&dom, "w").unwrap()), "hi");
    }

    #[test]
    fn network_is_unavailable() {
        let dom = run_html(
            r#"<p id=a></p><script>
            fetch('https://example.com').catch(e => { document.getElementById('a').textContent = 'blocked: ' + e.message; });
            </script>"#,
        );
        assert!(text(find(&dom, "a").unwrap()).starts_with("blocked: Network access"));
    }

    #[test]
    fn infinite_loops_are_cut_off_and_later_scripts_still_run() {
        let dom = run_html(r#"<p id=a>-</p><script>while (true) {}</script><script>document.getElementById('a').textContent = 'after';</script>"#);
        assert_eq!(text(find(&dom, "a").unwrap()), "after");
    }

    #[test]
    fn noscript_is_removed_and_location_is_readable() {
        let dom = run_html(r#"<noscript><p id=ns>enable js</p></noscript><p id=a></p><script>document.getElementById('a').textContent = location.hostname + location.search;</script>"#);
        assert!(find(&dom, "ns").is_none());
        assert_eq!(text(find(&dom, "a").unwrap()), "example.test?x=1");
    }

    #[test]
    fn dynamically_added_inline_scripts_run_but_innerhtml_scripts_do_not() {
        let dom = run_html(
            r#"<p id=a>-</p><p id=b>-</p><script>
            const s = document.createElement('script');
            s.textContent = "document.getElementById('a').textContent = 'ran'";
            document.body.appendChild(s);
            const d = document.createElement('div');
            d.innerHTML = "<script>document.getElementById('b').textContent = 'bad'<\/script>";
            document.body.appendChild(d);
            </script>"#,
        );
        assert_eq!(text(find(&dom, "a").unwrap()), "ran");
        assert_eq!(text(find(&dom, "b").unwrap()), "-");
    }
}
