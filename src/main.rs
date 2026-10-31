//! SafeBrowser: a browser written in Rust, with a sandboxed JavaScript interpreter and no unsafe code of its own.
//! Unsafe code is forbidden throughout this crate.
#![forbid(unsafe_code)]

mod css;
mod dom;
mod font;
mod form;
mod html;
mod image;
mod js;
mod layout;
mod loader;
mod net;
mod paint;
mod policy;
mod selector;
mod style;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result, bail};
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use url::Url;

use crate::font::Fonts;
use crate::form::Request;
use crate::layout::{Field, PageLayout, Target};
use crate::loader::{Loaded, PageKind};
use crate::paint::Canvas;

const TOOLBAR_H: f32 = 40.0;
const STATUS_H: f32 = 22.0;
const UI_FONT: f32 = 13.0;
const ADDRESS_X: f32 = 186.0;
/// (x, width, label, font size). The font has no reload symbol, so that label is text.
const BUTTONS: [(f32, f32, &str, f32); 3] = [(8.0, 28.0, "←", 16.0), (38.0, 28.0, "→", 16.0), (68.0, 46.0, "Reload", 11.0)];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--render") {
        return render_to_file(&args[1..]);
    }
    run_window(args.first().map(String::as_str))
}

struct Page {
    doc: Loaded,
    layout: PageLayout,
}

struct App {
    fonts: Fonts,
    canvas: Canvas,
    agent: ureq::Agent,
    page: Option<Page>,
    back: Vec<Url>,
    forward: Vec<Url>,
    scroll: f32,
    address: String,
    editing: bool,
    hover: Option<String>,
    /// What the user typed into the page's text fields, keyed by field id.
    fields: HashMap<usize, String>,
    focused: Option<Rc<Field>>,
    /// A message for the status bar (e.g. why a form was not sent).
    notice: Option<String>,
    /// In-flight request (id, record in history). Stale responses are dropped by id.
    loading: Option<(u64, bool)>,
    next_id: u64,
    tx: Sender<(u64, Loaded)>,
    rx: Receiver<(u64, Loaded)>,
    dirty: bool,
}

impl App {
    fn new(fonts: Fonts, w: usize, h: usize) -> Self {
        let (tx, rx) = mpsc::channel();
        App {
            fonts,
            canvas: Canvas::new(w, h),
            agent: net::agent(),
            page: None,
            back: Vec::new(),
            forward: Vec::new(),
            scroll: 0.0,
            address: String::new(),
            editing: false,
            hover: None,
            fields: HashMap::new(),
            focused: None,
            notice: None,
            loading: None,
            next_id: 0,
            tx,
            rx,
            dirty: true,
        }
    }

    fn current_url(&self) -> Option<&Url> {
        self.page.as_ref().map(|p| &p.doc.url)
    }

    fn title(&self) -> String {
        match &self.page {
            Some(p) => format!("{} — SafeBrowser", p.doc.title),
            None => "SafeBrowser".into(),
        }
    }

    fn viewport_height(&self) -> f32 {
        (self.canvas.h as f32 - TOOLBAR_H - STATUS_H).max(0.0)
    }

    fn navigate(&mut self, url: Url, record: bool) {
        // A #fragment in the same document only scrolls
        if let (Some(current), Some(fragment)) = (self.current_url(), url.fragment()) {
            let (mut a, mut b) = (current.clone(), url.clone());
            a.set_fragment(None);
            b.set_fragment(None);
            if a == b {
                if let Some(&y) = self.page.as_ref().and_then(|p| p.layout.anchors.get(fragment)) {
                    self.scroll = y;
                    self.clamp_scroll();
                }
                self.dirty = true;
                return;
            }
        }
        self.load(url, record);
    }

    fn load(&mut self, url: Url, record: bool) {
        self.load_request(Request::Get(url), record);
    }

    fn load_request(&mut self, request: Request, record: bool) {
        self.editing = false;
        self.notice = None;
        let url = request.url().clone();
        if url.as_str() == loader::HOME_URL {
            self.show(loader::home_page(), record);
            return;
        }
        if let Err(message) = policy::check(&url) {
            self.show(loader::error_page(&url, &message), record);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.loading = Some((id, record));
        let (agent, tx) = (self.agent.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send((id, loader::load(&agent, &request)));
        });
        self.dirty = true;
    }

    fn receive(&mut self) {
        while let Ok((id, loaded)) = self.rx.try_recv() {
            if let Some((current, record)) = self.loading {
                if current == id {
                    self.loading = None;
                    self.show(loaded, record);
                }
            }
        }
    }

    fn show(&mut self, doc: Loaded, record: bool) {
        if record {
            if let Some(url) = self.current_url().cloned() {
                self.back.push(url);
            }
            self.forward.clear();
        }
        let fragment = doc.url.fragment().map(str::to_string);
        self.page = Some(Page { doc, layout: PageLayout::default() });
        self.relayout();
        self.scroll = fragment
            .and_then(|f| self.page.as_ref()?.layout.anchors.get(&f).copied())
            .unwrap_or(0.0);
        self.clamp_scroll();
        self.hover = None;
        self.fields.clear();
        self.focused = None;
        self.dirty = true;
    }

    fn relayout(&mut self) {
        let width = self.canvas.w as f32;
        let viewport_height = self.viewport_height();
        if let Some(page) = &mut self.page {
            page.layout = match style::style_tree(&page.doc.dom, &page.doc.sheets, width) {
                Some(styled) => layout::layout(&styled, width, viewport_height, &mut self.fonts, &page.doc.images),
                None => PageLayout::default(),
            };
        }
    }

    fn go_back(&mut self) {
        if let Some(url) = self.back.pop() {
            if let Some(current) = self.current_url().cloned() {
                self.forward.push(current);
            }
            self.load(url, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(url) = self.forward.pop() {
            if let Some(current) = self.current_url().cloned() {
                self.back.push(current);
            }
            self.load(url, false);
        }
    }

    fn reload(&mut self) {
        if let Some(url) = self.current_url().cloned() {
            self.load(url, false);
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll += dy;
        self.clamp_scroll();
        self.dirty = true;
    }

    fn clamp_scroll(&mut self) {
        let height = self.page.as_ref().map_or(0.0, |p| p.layout.height);
        self.scroll = self.scroll.clamp(0.0, (height - self.viewport_height()).max(0.0));
    }

    fn start_editing(&mut self) {
        self.editing = true;
        self.focused = None;
        self.address = self.current_url().map(Url::to_string).unwrap_or_default();
        self.dirty = true;
    }

    fn submit_address(&mut self) {
        if let Some(url) = policy::resolve_input(&self.address) {
            self.navigate(url, true);
        }
    }

    /// Resolves a link against the page's base URL.
    fn resolve_link(&self, href: &str) -> Option<Url> {
        self.page.as_ref()?.doc.base.join(href).ok()
    }

    fn link_at(&self, mx: f32, my: f32) -> Option<Target> {
        if my < TOOLBAR_H || my > self.canvas.h as f32 - STATUS_H {
            return None;
        }
        self.page.as_ref()?.layout.link_at(mx, my - TOOLBAR_H + self.scroll).cloned()
    }

    fn hover_at(&mut self, mx: f32, my: f32) {
        let hover = self.link_at(mx, my).and_then(|target| match target {
            Target::Href(href) => Some(match self.resolve_link(&href) {
                Some(url) => format!("Link: {url}"),
                None => format!("Invalid link: {href}"),
            }),
            Target::Submit(_) => Some("Submit form".to_string()),
            Target::Field(_) => None,
        });
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }

    fn click(&mut self, mx: f32, my: f32) {
        if my < TOOLBAR_H {
            match mx {
                x if x >= ADDRESS_X => self.start_editing(),
                x => match BUTTONS.iter().position(|(bx, bw, ..)| (*bx..bx + bw).contains(&x)) {
                    Some(0) => self.go_back(),
                    Some(1) => self.go_forward(),
                    Some(2) => self.reload(),
                    _ => {}
                },
            }
            return;
        }
        self.editing = false;
        self.focused = None;
        self.dirty = true;
        match self.link_at(mx, my) {
            Some(Target::Href(href)) => {
                if let Some(url) = self.resolve_link(&href) {
                    self.navigate(url, true);
                }
            }
            Some(Target::Submit(id)) => self.submit_form(id),
            Some(Target::Field(field)) => {
                self.fields.entry(field.id).or_insert_with(|| field.initial.clone());
                self.focused = Some(field);
            }
            None => {}
        }
    }

    /// Submits the form containing the control `id` (a field or a submit button).
    fn submit_form(&mut self, id: usize) {
        let Some(page) = &self.page else { return };
        match form::submit(&page.doc.dom, &page.doc.base, id, &self.fields) {
            Some(Ok(request)) => {
                self.focused = None;
                self.load_request(request, true);
            }
            Some(Err(message)) => self.notice = Some(message),
            None => {}
        }
        self.dirty = true;
    }

    /// Text typed while a page field is focused.
    fn type_into_field(&mut self, text: &str) {
        if let Some(field) = &self.focused {
            self.fields.entry(field.id).or_default().push_str(text);
            self.dirty = true;
        }
    }

    /// Handles a key press. Returns false to quit.
    fn handle_key(&mut self, key: Key, cmd: bool) -> bool {
        let page_step = (self.viewport_height() - 40.0).max(40.0);
        match (cmd, key) {
            (true, Key::Q | Key::W) => return false,
            (true, Key::L) => self.start_editing(),
            (true, Key::R) => self.reload(),
            (true, Key::LeftBracket) => self.go_back(),
            (true, Key::RightBracket) => self.go_forward(),
            (true, Key::Backspace) if self.editing => self.address.clear(),
            (true, Key::V) if self.editing => self.address.push_str(&paste()),
            (true, Key::V) if self.focused.is_some() => self.type_into_field(&paste()),
            (true, Key::Backspace) if self.focused.is_some() => {
                if let Some(field) = &self.focused {
                    self.fields.insert(field.id, String::new());
                }
            }
            (false, _) if self.focused.is_some() => {
                let id = self.focused.as_ref().map(|f| f.id).unwrap_or_default();
                match key {
                    Key::Backspace => {
                        if let Some(value) = self.fields.get_mut(&id) {
                            value.pop();
                        }
                    }
                    Key::Enter | Key::NumPadEnter => self.submit_form(id),
                    Key::Escape => self.focused = None,
                    _ => {}
                }
            }
            (false, _) if self.editing => match key {
                Key::Backspace => {
                    self.address.pop();
                }
                Key::Enter | Key::NumPadEnter => self.submit_address(),
                Key::Escape => self.editing = false,
                _ => {}
            },
            (false, Key::Down) => self.scroll_by(40.0),
            (false, Key::Up) => self.scroll_by(-40.0),
            (false, Key::PageDown | Key::Space) => self.scroll_by(page_step),
            (false, Key::PageUp) => self.scroll_by(-page_step),
            (false, Key::Home) => self.scroll_by(-1e9),
            (false, Key::End) => self.scroll_by(1e9),
            _ => {}
        }
        self.dirty = true;
        true
    }

    fn render(&mut self) {
        let (w, h) = (self.canvas.w as f32, self.canvas.h as f32);
        let bottom = h - STATUS_H;
        self.canvas.reset_clip();
        let background = self.page.as_ref().and_then(|p| p.layout.background).unwrap_or(0xffffff);
        self.canvas.rect(0.0, 0.0, w, h, background);
        if let Some(page) = &self.page {
            self.canvas.set_clip(0.0, TOOLBAR_H, w, bottom);
            let dy = TOOLBAR_H - self.scroll;
            paint::paint_page(&mut self.canvas, &mut self.fonts, &page.layout, dy, TOOLBAR_H, bottom);
            let focused = self.focused.as_ref().map(|f| f.id);
            paint::paint_fields(&mut self.canvas, &mut self.fonts, &page.layout, dy, TOOLBAR_H, bottom, &self.fields, focused);
            // Scrollbar
            let view = self.viewport_height();
            if page.layout.height > view {
                let thumb = (view * view / page.layout.height).max(24.0);
                let y = TOOLBAR_H + (view - thumb) * self.scroll / (page.layout.height - view);
                self.canvas.rect(w - 7.0, y, 5.0, thumb, 0xa0a0a0);
            }
            self.canvas.reset_clip();
        }
        self.draw_toolbar();
        self.draw_status_bar();
    }

    fn draw_toolbar(&mut self) {
        // Connection state
        let (badge, color) = if self.loading.is_some() {
            ("Loading", 0x5f6368)
        } else {
            match self.page.as_ref().map(|p| p.doc.kind) {
                Some(PageKind::Secure) => ("HTTPS", 0x188038),
                Some(PageKind::LocalHttp) => ("HTTP", 0xb3261e),
                Some(PageKind::Internal) => ("Internal", 0x5f6368),
                Some(PageKind::Error) => ("Error", 0xb3261e),
                None => ("", 0),
            }
        };

        let w = self.canvas.w as f32;
        let c = &mut self.canvas;
        c.rect(0.0, 0.0, w, TOOLBAR_H, 0xf1f3f4);
        c.rect(0.0, TOOLBAR_H - 1.0, w, 1.0, 0xd0d0d0);

        let enabled = [!self.back.is_empty(), !self.forward.is_empty(), self.page.is_some()];
        for ((x, width, label, size), on) in BUTTONS.iter().zip(enabled) {
            let color = if on { 0x3c4043 } else { 0xb0b0b0 };
            let label_w = self.fonts.measure(label, *size, false);
            c.text(&mut self.fonts, x + (width - label_w) / 2.0, 25.0, label, *size, false, color);
        }
        c.text(&mut self.fonts, 122.0, 25.0, badge, UI_FONT - 1.0, true, color);

        // Address bar (IDN hosts stay in xn-- form)
        let (x0, x1) = (ADDRESS_X, w - 8.0);
        let border = if self.editing { 0x0b57d0 } else { 0xc4c7c5 };
        c.rect(x0, 6.0, x1 - x0, 28.0, border);
        c.rect(x0 + 1.0, 7.0, x1 - x0 - 2.0, 26.0, 0xffffff);
        let text = if self.editing {
            self.address.clone()
        } else {
            self.page.as_ref().map(|p| p.doc.url.to_string()).unwrap_or_default()
        };
        let text_w = self.fonts.measure(&text, UI_FONT, false);
        let room = x1 - x0 - 20.0;
        // While editing, shift left so the end (caret) stays visible
        let tx = if self.editing && text_w > room { x0 + 10.0 + room - text_w } else { x0 + 10.0 };
        c.set_clip(x0 + 4.0, 6.0, x1 - 4.0, 34.0);
        c.text(&mut self.fonts, tx, 25.0, &text, UI_FONT, false, 0x202124);
        if self.editing {
            c.rect(tx + text_w + 1.0, 12.0, 1.5, 17.0, 0x0b57d0);
        }
        c.reset_clip();
    }

    fn draw_status_bar(&mut self) {
        let (w, h) = (self.canvas.w as f32, self.canvas.h as f32);
        let top = h - STATUS_H;
        self.canvas.rect(0.0, top, w, STATUS_H, 0xf1f3f4);
        self.canvas.rect(0.0, top, w, 1.0, 0xd0d0d0);
        let text = match (&self.hover, &self.notice, self.loading) {
            // Show the real link target (defeats misleading link text)
            (Some(hover), _, _) => hover.clone(),
            (None, Some(notice), _) => notice.clone(),
            (None, None, Some(_)) => "Loading…".to_string(),
            (None, None, None) => "JavaScript: sandboxed · Cookies: off · Images: on".to_string(),
        };
        self.canvas.set_clip(0.0, top, w - 8.0, h);
        self.canvas.text(&mut self.fonts, 8.0, top + 15.0, &text, UI_FONT - 1.0, false, 0x3c4043);
        self.canvas.reset_clip();
    }
}

/// Reads one line from the macOS clipboard (minifb has no clipboard support).
fn paste() -> String {
    std::process::Command::new("/usr/bin/pbpaste")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_default()
}

struct TypedChars(Rc<RefCell<Vec<char>>>);

impl InputCallback for TypedChars {
    fn add_char(&mut self, c: u32) {
        if let Some(c) = char::from_u32(c).filter(|c| !c.is_control()) {
            self.0.borrow_mut().push(c);
        }
    }
}

fn run_window(initial: Option<&str>) -> Result<()> {
    let fonts = Fonts::load()?;
    let mut window = Window::new("SafeBrowser", 1280, 860, WindowOptions { resize: true, ..WindowOptions::default() })?;
    window.set_target_fps(60);
    let typed = Rc::new(RefCell::new(Vec::new()));
    window.set_input_callback(Box::new(TypedChars(typed.clone())));

    let (w, h) = window.get_size();
    let mut app = App::new(fonts, w, h);
    match initial.and_then(policy::resolve_input) {
        Some(url) => app.navigate(url, false),
        None => app.show(loader::home_page(), false),
    }

    let mut was_down = false;
    let mut title = String::new();
    while window.is_open() {
        let (w, h) = window.get_size();
        if (w, h) != (app.canvas.w, app.canvas.h) && w > 0 && h > 0 {
            app.canvas.resize(w, h);
            app.relayout();
            app.clamp_scroll();
            app.dirty = true;
        }
        app.receive();

        let cmd = window.is_key_down(Key::LeftSuper) || window.is_key_down(Key::RightSuper);
        let chars: Vec<char> = typed.borrow_mut().drain(..).collect();
        if !cmd && !chars.is_empty() {
            if app.editing {
                app.address.extend(chars);
                app.dirty = true;
            } else if app.focused.is_some() {
                app.type_into_field(&chars.into_iter().collect::<String>());
            }
        }
        for key in window.get_keys_pressed(KeyRepeat::Yes) {
            if !app.handle_key(key, cmd) {
                return Ok(());
            }
        }
        if let Some((_, dy)) = window.get_scroll_wheel() {
            app.scroll_by(-dy * 12.0);
        }
        if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Discard) {
            app.hover_at(mx, my);
            let down = window.get_mouse_down(MouseButton::Left);
            if down && !was_down {
                app.click(mx, my);
            }
            was_down = down;
        }

        let new_title = app.title();
        if new_title != title {
            window.set_title(&new_title);
            title = new_title;
        }
        if app.dirty {
            app.dirty = false;
            app.render();
        }
        // On macOS, minifb draws only during update_with_buffer and drops the buffer afterwards,
        // so the buffer has to be submitted every frame or the window goes blank.
        window.update_with_buffer(&app.canvas.buf, app.canvas.w, app.canvas.h)?;
    }
    Ok(())
}

/// Renders a page to a BMP file without opening a window (for testing).
fn render_to_file(args: &[String]) -> Result<()> {
    let (Some(input), Some(out)) = (args.first(), args.get(1)) else {
        bail!("usage: safebrowser --render <URL> <out.bmp> [height] [width]");
    };
    let height = args.get(2).and_then(|h| h.parse().ok()).unwrap_or(900);
    let width = args.get(3).and_then(|w| w.parse().ok()).unwrap_or(1280);
    let mut app = App::new(Fonts::load()?, width, height);
    let url = policy::resolve_input(input).context("cannot interpret URL")?;
    let page = if url.as_str() == loader::HOME_URL { loader::home_page() } else { loader::load(&app.agent, &Request::Get(url)) };
    app.show(page, false);
    app.render();
    write_bmp(out, &app.canvas)
}

fn write_bmp(path: &str, canvas: &Canvas) -> Result<()> {
    let row = (canvas.w * 3).div_ceil(4) * 4;
    let size = 54 + row * canvas.h;
    let mut data = Vec::with_capacity(size);
    data.extend_from_slice(b"BM");
    data.extend_from_slice(&(size as u32).to_le_bytes());
    data.extend_from_slice(&[0; 4]);
    data.extend_from_slice(&54u32.to_le_bytes());
    data.extend_from_slice(&40u32.to_le_bytes());
    data.extend_from_slice(&(canvas.w as i32).to_le_bytes());
    data.extend_from_slice(&(canvas.h as i32).to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&24u16.to_le_bytes());
    data.extend_from_slice(&[0; 24]);
    for y in (0..canvas.h).rev() {
        let start = data.len();
        for &px in &canvas.buf[y * canvas.w..(y + 1) * canvas.w] {
            data.extend_from_slice(&[px as u8, (px >> 8) as u8, (px >> 16) as u8]);
        }
        data.resize(start + row, 0);
    }
    std::fs::write(path, data).with_context(|| format!("cannot write {path}"))
}
