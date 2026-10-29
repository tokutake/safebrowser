# SafeBrowser

A browser written from scratch in Rust that does not run JavaScript.

Mainstream engines (WebKit, Chromium) are written in C++, and many of their most serious vulnerabilities are memory-safety bugs. SafeBrowser implements everything in Rust: HTML/CSS parsing, layout and painting.

## Usage

```sh
cargo run --release                          # open the start page
cargo run --release -- https://example.com   # open a URL
cargo run --release -- --render <URL> out.bmp [height]   # render to an image without a window (for testing)
cargo test
cargo test -- --ignored   # end-to-end tests against real sites (needs network)
```

| Action | Keys |
| --- | --- |
| Address bar | ⌘L or click; ⌘⌫ clears, ⌘V pastes |
| Page text fields | click to focus, type, ⌘V pastes, Enter submits, Esc leaves |
| Back / forward | ⌘[ / ⌘] |
| Reload | ⌘R |
| Scroll | wheel, arrow keys, Space, PageUp/PageDown, Home/End |
| Quit | ⌘Q / ⌘W |

## Security design

- **Memory safety**: the whole crate uses `#![forbid(unsafe_code)]`.
- **No JavaScript**: `<script>` elements are dropped during parsing, and `<noscript>` content is rendered instead.
- **HTTPS only**: `http://` is upgraded to `https://` (except `localhost` / `127.0.0.1`). The same check runs on every redirect hop. Certificate errors cannot be bypassed.
- **Forms**: GET and urlencoded POST. Forms with password fields are never sent (there are no cookies, so logins cannot work, and a GET would put the password in the URL). File uploads are not supported. Results of a POST are reloaded with GET, so a form is never resubmitted by accident.
- **Dangerous URLs are refused**: `javascript:`, `file:`, `data:` and other schemes, and URLs with embedded credentials such as `https://bank@evil.example/`.
- **Hard to track**:
  - No cookies are stored or sent, and no Referer header is sent.
  - Images are never fetched, so tracking pixels do nothing.
  - `:visited` never matches, so browsing history cannot leak.
- **Anti-spoofing**:
  - IDN hosts are shown in `xn--` form.
  - Hovering a link shows its real target in the status bar.
  - The connection badge reflects what was actually received over HTTPS, not the URL.
- **Resource limits** keep hostile pages from crashing or hanging the browser:
  - page size 8 MB; up to 16 stylesheets of 2 MB each
  - DOM depth 256 and 200k nodes
  - 32 CSS `var()` expansions
  - up to 32 stylesheets / 10 MB of CSS per page
  - grid auto-placement capped at 2000 items and 100 tracks
  - selectors limited to 8 compounds; matching with mixed combinators is memoized so it stays polynomial
  - bounded character-reference lookahead

## Layout of the code

| File | Role |
| --- | --- |
| `policy.rs` | Which URLs may be opened; address bar input handling |
| `net.rs` | HTTPS (rustls), redirect handling, charset detection |
| `html.rs` / `dom.rs` | HTML tokenizer and tree builder |
| `css.rs` / `style.rs` | CSS parsing, cascade, computed styles |
| `layout.rs` | Block/inline layout, line breaking (including CJK) |
| `font.rs` / `paint.rs` | Fonts (Hiragino) and software rendering |
| `loader.rs` | Page loading pipeline, internal pages |
| `selector.rs` | Selector parsing and matching |
| `form.rs` | Form submission (GET / urlencoded POST) |
| `main.rs` | Window, input, history, focused text fields |

## Limitations and honest caveats

- **Layout support**:
  - block and inline flow, including `inline-block` / `inline-flex` / `inline-grid` boxes placed in lines
  - flexbox (row/column, wrap, grow/shrink, justify/align, gap)
  - CSS grid (px/%/fr/auto/minmax/repeat including auto-fill, named areas, line placement, auto-placement; rows are sized to content)
  - tables (content-sized columns, colspan, legacy attributes like `width` and `bgcolor`)
  - floats (text wraps around them; tables/flex/grid sit beside them) and `clear`
  - positioning: `relative`/`sticky` offsets, `absolute` against the nearest positioned ancestor, `fixed` against the page (it scrolls with the page)
  - `::before` / `::after` with string, `attr()` and quote content (icon-font glyphs are dropped)
- **Selectors**: type, class, ID, attribute (`=`, `~=`, `|=`, `^=`, `$=`, `*=`, `i` flag), all four combinators, structural pseudo-classes (`:first-child`, `:nth-child()`, `*-of-type`, `:empty`, ...), `:not()` / `:is()` / `:where()`, `:checked` / `:disabled`, and escaped class names. Interaction and history states (`:hover`, `:focus`, `:visited`, ...) never match.
  - media queries are evaluated against the real window width, so layouts switch as it is resized
- **Not supported yet**: text selection inside fields, toggling checkboxes/selects, `z-index`, `overflow` clipping, `bottom` inside non-root containing blocks (falls back to the static position), complex selectors inside `:not()`/`:is()`, `:has()`, images, IME input, text selection, tabs.
- **Dependencies are not unsafe-free**:
  - minifb calls the macOS windowing APIs.
  - fontdue uses SIMD.
  - The TLS crypto backend (ring) contains assembly and C.
- **Where `unsafe` is excluded**: this crate's own code has none. Everything that handles attacker-controlled input (HTML, CSS, network data) is safe Rust, but the full dependency tree is not unsafe-free.
