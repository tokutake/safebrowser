// The browser-side API that pages expect (window, document, DOM classes, events, timers),
// built on the native bridge in js.rs. It runs once per page, before the page's own scripts.
(function (global) {
  'use strict';

  const op = global.__dom;
  const evalNative = global.__eval;
  const logNative = global.__log;
  const randomNative = global.__random;
  delete global.__dom;
  delete global.__eval;
  delete global.__log;
  delete global.__random;

  const ID = Symbol('id');
  const LISTENERS = Symbol('listeners');
  const CACHE = Symbol('cache');
  const idList = (s) => (s === '' ? [] : s.split(',').map(Number));
  const illegal = () => { throw new TypeError('Illegal constructor'); };
  const hidden = (obj, name, value) => Object.defineProperty(obj, name, { value, writable: true, configurable: true, enumerable: false });
  const toStr = (v) => (v === null || v === undefined ? '' : String(v));

  // ---- console ------------------------------------------------------------------------
  function format(v) {
    if (typeof v === 'string') return v;
    if (v instanceof Error) return v.stack || String(v);
    if (typeof v === 'object' && v !== null) {
      try { return JSON.stringify(v); } catch (e) { return String(v); }
    }
    return String(v);
  }
  const counts = {};
  const consoleApi = {};
  for (const level of ['log', 'info', 'warn', 'error', 'debug', 'trace', 'dir', 'table']) {
    consoleApi[level] = (...args) => logNative(level, args.map(format).join(' '));
  }
  Object.assign(consoleApi, {
    group() {}, groupCollapsed() {}, groupEnd() {}, time() {}, timeEnd() {}, timeLog() {}, clear() {},
    assert(c, ...a) { if (!c) logNative('assert', a.map(format).join(' ')); },
    count(l = 'default') { counts[l] = (counts[l] || 0) + 1; logNative('count', l + ': ' + counts[l]); },
  });
  global.console = consoleApi;

  // ---- events -------------------------------------------------------------------------
  class Event {
    constructor(type, init = {}) {
      this.type = String(type);
      this.bubbles = !!init.bubbles;
      this.cancelable = !!init.cancelable;
      this.composed = !!init.composed;
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this.isTrusted = false;
      this.timeStamp = now();
      hidden(this, '_stop', false);
      hidden(this, '_stopNow', false);
    }
    preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
    stopPropagation() { this._stop = true; }
    stopImmediatePropagation() { this._stop = true; this._stopNow = true; }
    composedPath() { return []; }
  }
  class CustomEvent extends Event {
    constructor(type, init = {}) { super(type, init); this.detail = init.detail === undefined ? null : init.detail; }
  }
  const eventClass = (name, defaults = {}) => class extends Event {
    constructor(type, init = {}) { super(type, init); Object.assign(this, defaults, init); }
  };
  const UIEvent = eventClass('UIEvent', { detail: 0, view: null });
  const MouseEvent = eventClass('MouseEvent', { clientX: 0, clientY: 0, screenX: 0, screenY: 0, button: 0, buttons: 0, ctrlKey: false, shiftKey: false, altKey: false, metaKey: false });
  const KeyboardEvent = eventClass('KeyboardEvent', { key: '', code: '', ctrlKey: false, shiftKey: false, altKey: false, metaKey: false, repeat: false });
  const FocusEvent = eventClass('FocusEvent', { relatedTarget: null });
  const InputEvent = eventClass('InputEvent', { data: null, inputType: '' });
  const ErrorEvent = eventClass('ErrorEvent', { message: '', filename: '', lineno: 0, colno: 0, error: null });
  const MessageEvent = eventClass('MessageEvent', { data: null, origin: '', source: null });
  const PopStateEvent = eventClass('PopStateEvent', { state: null });
  const HashChangeEvent = eventClass('HashChangeEvent', { oldURL: '', newURL: '' });

  function invoke(target, event) {
    const handlers = target[LISTENERS] && target[LISTENERS].get(event.type);
    event.currentTarget = target;
    const own = target['on' + event.type];
    if (typeof own === 'function') {
      try { own.call(target, event); } catch (e) { reportError(e); }
    }
    if (!handlers) return;
    for (const entry of [...handlers]) {
      if (event._stopNow) break;
      if (entry.once) removeListener(target, event.type, entry.fn);
      try {
        if (typeof entry.fn === 'function') entry.fn.call(target, event);
        else if (entry.fn && typeof entry.fn.handleEvent === 'function') entry.fn.handleEvent(event);
      } catch (e) { reportError(e); }
    }
  }
  function removeListener(target, type, fn) {
    const list = target[LISTENERS] && target[LISTENERS].get(type);
    if (!list) return;
    const i = list.findIndex((e) => e.fn === fn);
    if (i >= 0) list.splice(i, 1);
  }
  class EventTarget {
    constructor() {}
    addEventListener(type, fn, options) {
      const t = this ?? global;
      if (!fn) return;
      if (!t[LISTENERS]) hidden(t, LISTENERS, new Map());
      if (!t[LISTENERS].has(type)) t[LISTENERS].set(type, []);
      const list = t[LISTENERS].get(type);
      if (list.some((e) => e.fn === fn)) return;
      list.push({ fn, once: typeof options === 'object' && options !== null && !!options.once });
    }
    removeEventListener(type, fn) { removeListener(this ?? global, type, fn); }
    dispatchEvent(event) {
      const t = this ?? global;
      event.target = t;
      let node = t;
      while (node) {
        invoke(node, event);
        if (event._stop || !event.bubbles) break;
        node = node.parentNode || (node === document ? global : null);
      }
      event.currentTarget = null;
      return !event.defaultPrevented;
    }
  }
  for (const k of ['addEventListener', 'removeEventListener', 'dispatchEvent']) {
    hidden(global, k, EventTarget.prototype[k]);
  }

  // ---- timers (virtual clock: nothing ever sleeps) --------------------------------------
  let clock = 0;
  let timerSeq = 0;
  let timers = [];
  const startReal = Date.now();
  function now() { return clock; }
  function addTimer(fn, delay, args, repeat) {
    delay = Math.max(0, Number(delay) || 0);
    const t = { id: ++timerSeq, at: clock + delay, fn, args, repeat: repeat ? Math.max(delay, 4) : 0 };
    timers.push(t);
    return t.id;
  }
  global.setTimeout = (fn, delay, ...args) => addTimer(fn, delay, args, false);
  global.setInterval = (fn, delay, ...args) => addTimer(fn, delay, args, true);
  global.clearTimeout = global.clearInterval = (id) => { timers = timers.filter((t) => t.id !== id); };
  global.requestAnimationFrame = (fn) => addTimer(() => fn(clock), 16, [], false);
  global.cancelAnimationFrame = global.clearTimeout;
  global.requestIdleCallback = (fn) => addTimer(() => fn({ didTimeout: false, timeRemaining: () => 50 }), 1, [], false);
  global.cancelIdleCallback = global.clearTimeout;
  if (typeof global.queueMicrotask !== 'function') global.queueMicrotask = (fn) => { Promise.resolve().then(fn); };
  global.performance = { now, timeOrigin: startReal, mark() {}, measure() {}, getEntriesByType() { return []; }, getEntriesByName() { return []; } };

  // Runs the earliest timer. Intervals stop after 10 s of virtual time so they cannot run forever.
  global.__nextTimer = () => {
    if (!timers.length) return false;
    let best = 0;
    for (let i = 1; i < timers.length; i++) {
      if (timers[i].at < timers[best].at || (timers[i].at === timers[best].at && timers[i].id < timers[best].id)) best = i;
    }
    const t = timers[best];
    clock = Math.max(clock, t.at);
    if (t.repeat && clock < 10000) t.at = clock + t.repeat;
    else timers.splice(best, 1);
    try { typeof t.fn === 'function' && t.fn(...t.args); } catch (e) { reportError(e); }
    return true;
  };

  function reportError(e) {
    logNative('error', 'uncaught: ' + format(e));
    try {
      const ev = new ErrorEvent('error', { message: e && e.message ? e.message : String(e), error: e, cancelable: true });
      global.dispatchEvent(ev);
    } catch (_) { /* ignore */ }
  }
  global.__reportError = (message) => reportError(new Error(message));

  // ---- DOM wrappers ---------------------------------------------------------------------
  const wrappers = new Map();
  const wrap = (id) => {
    if (id < 0) return null;
    let w = wrappers.get(id);
    if (w) return w;
    const type = op('type', id);
    let proto;
    if (type === 9) proto = Document.prototype;
    else if (type === 3) proto = Text.prototype;
    else {
      const tag = op('tag', id);
      proto = tag === '#fragment' ? DocumentFragment.prototype : (tagClasses[tag] || (SVG_TAGS.has(tag) ? Element.prototype : HTMLElement.prototype)).prototype;
    }
    w = Object.create(proto);
    hidden(w, ID, id);
    wrappers.set(id, w);
    return w;
  };
  const idOf = (node) => {
    if (!node || typeof node[ID] !== 'number') throw new TypeError('Argument is not a Node');
    return node[ID];
  };
  const SVG_TAGS = new Set(['svg', 'path', 'g', 'circle', 'rect']);

  const attrOf = (id, name) => op('getattr', id, name);

  class Node extends EventTarget {
    constructor() { illegal(); }
    get nodeType() { return op('type', this[ID]); }
    get nodeName() { const t = this.nodeType; return t === 9 ? '#document' : t === 3 ? '#text' : this.tagName; }
    get parentNode() { return wrap(op('parent', this[ID])); }
    get parentElement() { const p = this.parentNode; return p && p.nodeType === 1 ? p : null; }
    get childNodes() { return idList(op('children', this[ID])).map(wrap); }
    get firstChild() { return this.childNodes[0] || null; }
    get lastChild() { const c = this.childNodes; return c[c.length - 1] || null; }
    get nextSibling() { return wrap(op('sibling', this[ID], 1)); }
    get previousSibling() { return wrap(op('sibling', this[ID], -1)); }
    get ownerDocument() { return this.nodeType === 9 ? null : document; }
    get isConnected() { return op('connected', this[ID]); }
    get textContent() { return this.nodeType === 9 ? null : op('text', this[ID]); }
    set textContent(v) { op('settext', this[ID], toStr(v)); }
    get nodeValue() { return this.nodeType === 3 ? op('text', this[ID]) : null; }
    set nodeValue(v) { if (this.nodeType === 3) op('settext', this[ID], toStr(v)); }
    hasChildNodes() { return op('children', this[ID]) !== ''; }
    contains(other) {
      for (let n = other; n; n = n.parentNode) if (n === this) return true;
      return false;
    }
    appendChild(child) { return this.insertBefore(child, null); }
    insertBefore(child, ref) {
      if (child instanceof DocumentFragment) {
        for (const c of child.childNodes) this.insertBefore(c, ref);
        return child;
      }
      try {
        op('insert', this[ID], idOf(child), ref ? idOf(ref) : -1);
      } catch (e) {
        throw new DOMException(String(e.message || e), 'HierarchyRequestError');
      }
      afterInsert(child);
      return child;
    }
    removeChild(child) {
      if (child.parentNode !== this) throw new DOMException('The node to be removed is not a child of this node.', 'NotFoundError');
      op('remove', idOf(child));
      return child;
    }
    replaceChild(neu, old) {
      const next = old.nextSibling;
      this.removeChild(old);
      this.insertBefore(neu, next);
      return old;
    }
    cloneNode(deep = false) { return wrap(op('clone', this[ID], !!deep)); }
    normalize() {}
    getRootNode() { let n = this; while (n.parentNode) n = n.parentNode; return n; }
    isEqualNode(o) { return !!o && this.nodeName === o.nodeName && op('outer', this[ID]) === op('outer', o[ID]); }
    isSameNode(o) { return this === o; }
    compareDocumentPosition() { return 0; }
  }
  Node.ELEMENT_NODE = 1; Node.TEXT_NODE = 3; Node.COMMENT_NODE = 8; Node.DOCUMENT_NODE = 9; Node.DOCUMENT_FRAGMENT_NODE = 11;

  class CharacterData extends Node {
    get data() { return op('text', this[ID]); }
    set data(v) { op('settext', this[ID], toStr(v)); }
    get length() { return this.data.length; }
  }
  class Text extends CharacterData {}
  class Comment extends CharacterData {}

  // Child lists shared by elements, fragments and the document
  const ParentNode = {
    get children() { return this.childNodes.filter((n) => n.nodeType === 1); },
    get firstElementChild() { return this.children[0] || null; },
    get lastElementChild() { const c = this.children; return c[c.length - 1] || null; },
    get childElementCount() { return this.children.length; },
    append(...nodes) { for (const n of nodes) this.appendChild(typeof n === 'object' ? n : document.createTextNode(String(n))); },
    prepend(...nodes) {
      const first = this.firstChild;
      for (const n of nodes) this.insertBefore(typeof n === 'object' ? n : document.createTextNode(String(n)), first);
    },
    replaceChildren(...nodes) { for (const c of this.childNodes) this.removeChild(c); this.append(...nodes); },
    querySelector(sel) { const r = op('query', this[ID], String(sel), false); return r === '' ? null : wrap(Number(r)); },
    querySelectorAll(sel) { return idList(op('query', this[ID], String(sel), true)).map(wrap); },
    getElementsByTagName(tag) { tag = String(tag).toLowerCase(); return this.querySelectorAll(tag === '*' ? '*' : tag); },
    getElementsByClassName(names) { return this.querySelectorAll(String(names).trim().split(/\s+/).map((c) => '.' + c).join('')); },
  };
  function mixin(target, source) {
    for (const k of Object.getOwnPropertyNames(source)) Object.defineProperty(target.prototype, k, Object.getOwnPropertyDescriptor(source, k));
  }

  class DocumentFragment extends Node {
    getElementById(id) { return this.querySelector('#' + id); }
  }
  mixin(DocumentFragment, ParentNode);

  // ---- element helpers -------------------------------------------------------------------
  function parseDecls(text) {
    const map = new Map();
    for (const part of text.split(';')) {
      const i = part.indexOf(':');
      if (i > 0) map.set(part.slice(0, i).trim().toLowerCase(), part.slice(i + 1).trim());
    }
    return map;
  }
  const kebab = (k) => {
    if (k === 'cssFloat') return 'float';
    const s = k.replace(/[A-Z]/g, (m) => '-' + m.toLowerCase());
    return /^(webkit|moz|ms)-/.test(s) ? '-' + s : s;
  };
  function makeStyle(el) {
    const read = () => parseDecls(el.getAttribute('style') || '');
    const write = (m) => {
      const s = [...m].map(([k, v]) => k + ': ' + v).join('; ');
      if (s) el.setAttribute('style', s + ';'); else el.removeAttribute('style');
    };
    const api = {
      getPropertyValue(n) { return read().get(String(n).toLowerCase()) ?? ''; },
      setProperty(n, v) {
        const m = read();
        if (v === '' || v === null || v === undefined) m.delete(String(n).toLowerCase()); else m.set(String(n).toLowerCase(), String(v));
        write(m);
      },
      removeProperty(n) { const m = read(); const old = m.get(String(n).toLowerCase()) ?? ''; m.delete(String(n).toLowerCase()); write(m); return old; },
      item(i) { return [...read().keys()][i] || ''; },
      toString() { return el.getAttribute('style') || ''; },
    };
    return new Proxy(api, {
      get(t, k) {
        if (k === 'cssText') return el.getAttribute('style') || '';
        if (k === 'length') return read().size;
        if (typeof k === 'symbol' || k in t) return t[k];
        return t.getPropertyValue(kebab(k));
      },
      set(t, k, v) {
        if (k === 'cssText') el.setAttribute('style', String(v)); else t.setProperty(kebab(k), v);
        return true;
      },
    });
  }
  class DOMTokenList {
    constructor(el, attr) { hidden(this, 'el', el); hidden(this, 'attr', attr); }
    get tokens() { return (this.el.getAttribute(this.attr) || '').split(/\s+/).filter(Boolean); }
    save(list) { this.el.setAttribute(this.attr, list.join(' ')); }
    get length() { return this.tokens.length; }
    get value() { return this.el.getAttribute(this.attr) || ''; }
    set value(v) { this.el.setAttribute(this.attr, v); }
    item(i) { return this.tokens[i] ?? null; }
    contains(t) { return this.tokens.includes(String(t)); }
    add(...ts) { const l = this.tokens; for (const t of ts) if (!l.includes(String(t))) l.push(String(t)); this.save(l); }
    remove(...ts) { this.save(this.tokens.filter((t) => !ts.map(String).includes(t))); }
    toggle(t, force) {
      t = String(t);
      const has = this.contains(t);
      const want = force === undefined ? !has : !!force;
      if (want && !has) this.add(t); else if (!want && has) this.remove(t);
      return want;
    }
    replace(a, b) { if (!this.contains(a)) return false; this.save(this.tokens.map((t) => (t === String(a) ? String(b) : t))); return true; }
    forEach(fn) { this.tokens.forEach(fn); }
    [Symbol.iterator]() { return this.tokens[Symbol.iterator](); }
    toString() { return this.value; }
  }
  function makeDataset(el) {
    const toAttr = (k) => 'data-' + k.replace(/[A-Z]/g, (m) => '-' + m.toLowerCase());
    const toProp = (a) => a.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    const names = () => el.getAttributeNames().filter((n) => n.startsWith('data-')).map(toProp);
    return new Proxy({}, {
      get: (t, k) => (typeof k === 'symbol' ? undefined : el.hasAttribute(toAttr(k)) ? el.getAttribute(toAttr(k)) : undefined),
      set: (t, k, v) => { el.setAttribute(toAttr(k), String(v)); return true; },
      has: (t, k) => typeof k === 'string' && el.hasAttribute(toAttr(k)),
      deleteProperty: (t, k) => { el.removeAttribute(toAttr(k)); return true; },
      ownKeys: () => names(),
      getOwnPropertyDescriptor: (t, k) => (typeof k === 'string' && el.hasAttribute(toAttr(k)) ? { value: el.getAttribute(toAttr(k)), writable: true, enumerable: true, configurable: true } : undefined),
    });
  }
  const cached = (obj, key, make) => {
    if (!obj[CACHE]) hidden(obj, CACHE, {});
    return obj[CACHE][key] || (obj[CACHE][key] = make());
  };

  class Element extends Node {
    get tagName() { return op('tag', this[ID]).toUpperCase(); }
    get localName() { return op('tag', this[ID]); }
    get namespaceURI() { return 'http://www.w3.org/1999/xhtml'; }
    get id() { return attrOf(this[ID], 'id') ?? ''; }
    set id(v) { this.setAttribute('id', v); }
    get className() { return attrOf(this[ID], 'class') ?? ''; }
    set className(v) { this.setAttribute('class', v); }
    get classList() { return cached(this, 'classList', () => new DOMTokenList(this, 'class')); }
    get style() { return cached(this, 'style', () => makeStyle(this)); }
    set style(v) { this.setAttribute('style', String(v)); }
    get dataset() { return cached(this, 'dataset', () => makeDataset(this)); }
    get attributes() { return this.getAttributeNames().map((name) => ({ name, value: this.getAttribute(name), nodeName: name, nodeValue: this.getAttribute(name) })); }
    getAttribute(n) { return attrOf(this[ID], String(n)); }
    getAttributeNS(ns, n) { return this.getAttribute(n); }
    setAttribute(n, v) { op('setattr', this[ID], String(n), toStr(v)); if (String(n).toLowerCase() === 'src') srcChanged(this); }
    setAttributeNS(ns, n, v) { this.setAttribute(n, v); }
    removeAttribute(n) { op('removeattr', this[ID], String(n)); }
    removeAttributeNS(ns, n) { this.removeAttribute(n); }
    hasAttribute(n) { return attrOf(this[ID], String(n)) !== null; }
    hasAttributes() { return op('attrnames', this[ID]) !== ''; }
    getAttributeNames() { const s = op('attrnames', this[ID]); return s === '' ? [] : s.split(' '); }
    toggleAttribute(n, force) {
      const has = this.hasAttribute(n);
      const want = force === undefined ? !has : !!force;
      if (want && !has) this.setAttribute(n, ''); else if (!want && has) this.removeAttribute(n);
      return want;
    }
    get nextElementSibling() { for (let n = this.nextSibling; n; n = n.nextSibling) if (n.nodeType === 1) return n; return null; }
    get previousElementSibling() { for (let n = this.previousSibling; n; n = n.previousSibling) if (n.nodeType === 1) return n; return null; }
    get innerHTML() { return op('inner', this[ID]); }
    set innerHTML(v) {
      const target = this.localName === 'template' ? this.content : this;
      op('setinner', target[ID], toStr(v));
    }
    get outerHTML() { return op('outer', this[ID]); }
    get innerText() { return this.textContent; }
    set innerText(v) { this.textContent = v; }
    get content() {
      if (this.localName !== 'template') return undefined;
      return cached(this, 'content', () => document.createDocumentFragment());
    }
    get shadowRoot() { return null; }
    attachShadow() { const d = document.createElement('div'); this.appendChild(d); return d; }
    matches(sel) { return op('matches', this[ID], String(sel)); }
    webkitMatchesSelector(sel) { return this.matches(sel); }
    closest(sel) { for (let n = this; n && n.nodeType === 1; n = n.parentNode) if (n.matches(sel)) return n; return null; }
    remove() { op('remove', this[ID]); }
    before(...nodes) { const p = this.parentNode; if (p) for (const n of nodes) p.insertBefore(typeof n === 'object' ? n : document.createTextNode(String(n)), this); }
    after(...nodes) { const p = this.parentNode; const next = this.nextSibling; if (p) for (const n of nodes) p.insertBefore(typeof n === 'object' ? n : document.createTextNode(String(n)), next); }
    replaceWith(...nodes) { this.before(...nodes); this.remove(); }
    insertAdjacentElement(where, el) {
      switch (String(where).toLowerCase()) {
        case 'beforebegin': this.before(el); break;
        case 'afterbegin': this.insertBefore(el, this.firstChild); break;
        case 'beforeend': this.appendChild(el); break;
        case 'afterend': this.after(el); break;
        default: throw new DOMException('Invalid position', 'SyntaxError');
      }
      return el;
    }
    insertAdjacentHTML(where, html) {
      const holder = document.createElement('div');
      holder.innerHTML = html;
      for (const c of holder.childNodes) this.insertAdjacentElement(where, c);
    }
    insertAdjacentText(where, text) { this.insertAdjacentElement(where, document.createTextNode(text)); }
    // Nothing is laid out while scripts run, so geometry is zero.
    getBoundingClientRect() { return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 }; }
    getClientRects() { return []; }
    get offsetWidth() { return 0; } get offsetHeight() { return 0; } get offsetTop() { return 0; } get offsetLeft() { return 0; }
    get clientWidth() { return 0; } get clientHeight() { return 0; } get clientTop() { return 0; } get clientLeft() { return 0; }
    get scrollWidth() { return 0; } get scrollHeight() { return 0; }
    get scrollTop() { return 0; } set scrollTop(v) {} get scrollLeft() { return 0; } set scrollLeft(v) {}
    get offsetParent() { return null; }
    scrollIntoView() {} scrollTo() {} scrollBy() {} focus() {} blur() {} animate() { return { cancel() {}, finished: Promise.resolve() }; }
    click() { this.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); }
    checkVisibility() { return true; }
    getAnimations() { return []; }
  }
  mixin(Element, ParentNode);

  // Properties that mirror attributes
  function reflect(cls, prop, attr, kind) {
    Object.defineProperty(cls.prototype, prop, {
      configurable: true,
      get() {
        const v = attrOf(this[ID], attr);
        if (kind === 'bool') return v !== null;
        if (kind === 'int') return v === null ? 0 : parseInt(v, 10) || 0;
        return v ?? '';
      },
      set(v) {
        if (kind === 'bool') { if (v) this.setAttribute(attr, ''); else this.removeAttribute(attr); } else this.setAttribute(attr, toStr(v));
      },
    });
  }
  class HTMLElement extends Element {}
  for (const [p, a] of [['title', 'title'], ['lang', 'lang'], ['dir', 'dir'], ['href', 'href'], ['src', 'src'], ['alt', 'alt'], ['type', 'type'], ['name', 'name'],
    ['placeholder', 'placeholder'], ['htmlFor', 'for'], ['target', 'target'], ['rel', 'rel'], ['action', 'action'], ['method', 'method'], ['media', 'media'],
    ['srcset', 'srcset'], ['sizes', 'sizes'], ['role', 'role'], ['content', 'content'], ['charset', 'charset'], ['crossOrigin', 'crossorigin'], ['integrity', 'integrity'],
    ['loading', 'loading'], ['download', 'download'], ['accept', 'accept'], ['autocomplete', 'autocomplete'], ['pattern', 'pattern'], ['label', 'label']]) {
    if (p === 'content') continue; // `template.content` is a getter of its own
    reflect(Element, p, a, 'string');
  }
  for (const [p, a] of [['hidden', 'hidden'], ['disabled', 'disabled'], ['checked', 'checked'], ['selected', 'selected'], ['readOnly', 'readonly'],
    ['required', 'required'], ['async', 'async'], ['defer', 'defer'], ['multiple', 'multiple'], ['autofocus', 'autofocus'], ['open', 'open'], ['noModule', 'nomodule']]) {
    reflect(Element, p, a, 'bool');
  }
  for (const [p, a] of [['tabIndex', 'tabindex'], ['width', 'width'], ['height', 'height'], ['colSpan', 'colspan'], ['rowSpan', 'rowspan'], ['maxLength', 'maxlength']]) {
    reflect(Element, p, a, 'int');
  }
  Object.defineProperty(Element.prototype, 'value', {
    configurable: true,
    get() {
      const tag = this.localName;
      if (tag === 'textarea') return this.textContent;
      if (tag === 'select') { const o = this.querySelector('option[selected]') || this.querySelector('option'); return o ? (o.getAttribute('value') ?? o.textContent) : ''; }
      return attrOf(this[ID], 'value') ?? '';
    },
    set(v) { if (this.localName === 'textarea') this.textContent = v; else this.setAttribute('value', toStr(v)); },
  });
  Object.defineProperty(Element.prototype, 'text', {
    configurable: true,
    get() { return this.textContent; },
    set(v) { this.textContent = v; },
  });
  Object.defineProperty(Element.prototype, 'complete', { configurable: true, get() { return true; } });
  Object.defineProperty(Element.prototype, 'naturalWidth', { configurable: true, get() { return 0; } });
  Object.defineProperty(Element.prototype, 'naturalHeight', { configurable: true, get() { return 0; } });
  Object.defineProperty(Element.prototype, 'currentSrc', { configurable: true, get() { return this.getAttribute('src') || ''; } });
  Object.defineProperty(Element.prototype, 'form', { configurable: true, get() { return this.closest('form'); } });
  Object.defineProperty(Element.prototype, 'options', { configurable: true, get() { return this.querySelectorAll('option'); } });
  Object.defineProperty(Element.prototype, 'selectedIndex', { configurable: true, get() { const o = [...this.options]; const i = o.findIndex((x) => x.hasAttribute('selected')); return o.length ? Math.max(i, 0) : -1; } });
  Object.defineProperty(Element.prototype, 'elements', { configurable: true, get() { return this.querySelectorAll('input,select,textarea,button'); } });

  // An <img> whose source is set "loads" right away: pages that wait for load can continue.
  function srcChanged(el) {
    if (el.localName === 'img') addTimer(() => el.dispatchEvent(new Event('load')), 0, [], false);
  }

  const tagClasses = {};
  for (const [name, tags] of Object.entries({
    HTMLDivElement: ['div'], HTMLSpanElement: ['span'], HTMLAnchorElement: ['a'], HTMLImageElement: ['img'], HTMLInputElement: ['input'],
    HTMLButtonElement: ['button'], HTMLFormElement: ['form'], HTMLScriptElement: ['script'], HTMLStyleElement: ['style'], HTMLLinkElement: ['link'],
    HTMLBodyElement: ['body'], HTMLHeadElement: ['head'], HTMLHtmlElement: ['html'], HTMLSelectElement: ['select'], HTMLOptionElement: ['option'],
    HTMLTextAreaElement: ['textarea'], HTMLParagraphElement: ['p'], HTMLUListElement: ['ul'], HTMLLIElement: ['li'], HTMLTableElement: ['table'],
    HTMLTemplateElement: ['template'], HTMLCanvasElement: ['canvas'], HTMLIFrameElement: ['iframe'], HTMLLabelElement: ['label'], HTMLMetaElement: ['meta'],
    HTMLTitleElement: ['title'], HTMLHeadingElement: ['h1', 'h2', 'h3', 'h4', 'h5', 'h6'],
  })) {
    const cls = { [name]: class extends HTMLElement {} }[name];
    global[name] = cls;
    for (const t of tags) tagClasses[t] = cls;
  }
  global.Image = function Image(w, h) {
    const img = document.createElement('img');
    if (w !== undefined) img.setAttribute('width', w);
    if (h !== undefined) img.setAttribute('height', h);
    return img;
  };
  global.Option = function Option(text, value) {
    const o = document.createElement('option');
    if (text !== undefined) o.textContent = text;
    if (value !== undefined) o.setAttribute('value', value);
    return o;
  };

  // Scripts added to the page run when they are inserted, unless they were already run.
  function afterInsert(node) {
    if (!node.isConnected) return;
    const scripts = node.localName === 'script' ? [node] : (node.nodeType === 1 ? node.querySelectorAll('script') : []);
    for (const s of scripts) {
      if (op('started', s[ID])) continue;
      const type = (s.getAttribute('type') || '').trim().toLowerCase();
      if (!['', 'text/javascript', 'application/javascript', 'text/ecmascript', 'application/ecmascript'].includes(type)) continue;
      if (s.getAttribute('src')) {
        // No network access from scripts: report the load as failed
        addTimer(() => s.dispatchEvent(new Event('error')), 0, [], false);
      } else {
        try { evalNative(s.textContent); } catch (e) { reportError(e); }
      }
    }
  }

  // ---- document ---------------------------------------------------------------------------
  class Document extends Node {
    get documentElement() { return wrap(op('findtag', 'html')); }
    get head() { return wrap(op('findtag', 'head')); }
    get body() { return wrap(op('findtag', 'body')); }
    get title() { const t = wrap(op('findtag', 'title')); return t ? t.textContent.trim().replace(/\s+/g, ' ') : ''; }
    set title(v) {
      let t = wrap(op('findtag', 'title'));
      if (!t) { t = this.createElement('title'); this.head.appendChild(t); }
      t.textContent = v;
    }
    get readyState() { return readyState; }
    get currentScript() { return wrap(op('currentscript')); }
    get location() { return global.location; }
    get defaultView() { return global; }
    get URL() { return global.location.href; }
    get documentURI() { return global.location.href; }
    get baseURI() { return global.location.href; }
    get domain() { return global.location.hostname; }
    get referrer() { return ''; }
    get cookie() { return ''; }
    set cookie(v) {}
    get characterSet() { return 'UTF-8'; }
    get charset() { return 'UTF-8'; }
    get contentType() { return 'text/html'; }
    get compatMode() { return 'CSS1Compat'; }
    get hidden() { return false; }
    get visibilityState() { return 'visible'; }
    get activeElement() { return this.body; }
    get forms() { return this.querySelectorAll('form'); }
    get images() { return this.querySelectorAll('img'); }
    get links() { return this.querySelectorAll('a[href], area[href]'); }
    get scripts() { return this.querySelectorAll('script'); }
    get styleSheets() { return []; }
    get fonts() { return { ready: Promise.resolve(), load() { return Promise.resolve([]); }, check() { return true; }, addEventListener() {} }; }
    hasFocus() { return true; }
    createElement(tag) {
      const t = String(tag).toLowerCase();
      if (!/^[a-z][^\s/>]*$/.test(t)) throw new DOMException('Invalid tag name', 'InvalidCharacterError');
      return wrap(op('create', t));
    }
    createElementNS(ns, tag) { return this.createElement(tag); }
    createTextNode(text) { return wrap(op('createtext', toStr(text))); }
    createComment() { return wrap(op('createtext', '')); }
    createDocumentFragment() { return wrap(op('create', '#fragment')); }
    createEvent(kind) { return { initEvent(type, bubbles, cancelable) { Object.assign(this, new Event(type, { bubbles, cancelable })); } }; }
    createRange() { return { selectNodeContents() {}, collapse() {}, setStart() {}, setEnd() {}, getBoundingClientRect() { return { top: 0, left: 0, width: 0, height: 0 }; }, createContextualFragment(html) { const t = document.createElement('template'); t.innerHTML = html; return t.content; } }; }
    getSelection() { return { rangeCount: 0, removeAllRanges() {}, addRange() {}, toString() { return ''; } }; }
    getElementById(id) { const r = op('byid', String(id)); return r < 0 ? null : wrap(r); }
    getElementsByName(n) { return this.querySelectorAll('[name="' + String(n).replace(/"/g, '') + '"]'); }
    importNode(node, deep) { return node.cloneNode(deep); }
    adoptNode(node) { return node; }
    elementFromPoint() { return null; }
    elementsFromPoint() { return []; }
    execCommand() { return false; }
    write(...html) { op('write', html.map(toStr).join('')); }
    writeln(...html) { this.write(html.join('\n') + '\n'); }
    open() { return this; }
    close() {}
  }
  mixin(Document, ParentNode);
  Object.defineProperty(Document.prototype, 'children', { configurable: true, get() { const e = this.documentElement; return e ? [e] : []; } });

  class DOMException extends Error {
    constructor(message = '', name = 'Error') { super(message); this.name = name; }
  }

  let readyState = 'loading';
  const document = wrap(0);
  global.document = document;
  global.window = global.self = global.top = global.parent = global.frames = global;
  global.globalThis = global;

  // ---- window -------------------------------------------------------------------------------
  global.__domContentLoaded = () => {
    readyState = 'interactive';
    document.dispatchEvent(new Event('readystatechange'));
    document.dispatchEvent(new Event('DOMContentLoaded', { bubbles: true }));
  };
  global.__load = () => {
    readyState = 'complete';
    document.dispatchEvent(new Event('readystatechange'));
    global.dispatchEvent(new Event('load'));
    global.dispatchEvent(new Event('pageshow'));
  };

  class URLSearchParams {
    constructor(init = '') {
      this.list = [];
      if (typeof init === 'string') {
        for (const part of init.replace(/^\?/, '').split('&')) {
          if (!part) continue;
          const i = part.indexOf('=');
          const dec = (s) => { try { return decodeURIComponent(s.replace(/\+/g, ' ')); } catch (e) { return s; } };
          this.list.push(i < 0 ? [dec(part), ''] : [dec(part.slice(0, i)), dec(part.slice(i + 1))]);
        }
      } else if (init && typeof init === 'object') {
        for (const [k, v] of (Symbol.iterator in init ? init : Object.entries(init))) this.list.push([String(k), String(v)]);
      }
    }
    get(k) { const e = this.list.find((p) => p[0] === k); return e ? e[1] : null; }
    getAll(k) { return this.list.filter((p) => p[0] === k).map((p) => p[1]); }
    has(k) { return this.list.some((p) => p[0] === k); }
    set(k, v) { this.delete(k); this.list.push([String(k), String(v)]); }
    append(k, v) { this.list.push([String(k), String(v)]); }
    delete(k) { this.list = this.list.filter((p) => p[0] !== k); }
    forEach(fn) { this.list.forEach(([k, v]) => fn(v, k, this)); }
    keys() { return this.list.map((p) => p[0])[Symbol.iterator](); }
    values() { return this.list.map((p) => p[1])[Symbol.iterator](); }
    entries() { return this.list.map((p) => [...p])[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    toString() { return this.list.map(([k, v]) => encodeURIComponent(k) + '=' + encodeURIComponent(v)).join('&'); }
  }
  class URL {
    constructor(url, base) {
      const m = /^([a-zA-Z][a-zA-Z0-9+.-]*:)(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(String(url));
      if (!m) {
        if (base === undefined) throw new TypeError('Invalid URL');
        const b = new URL(base);
        const u = String(url);
        let path;
        if (u.startsWith('//')) return new URL(b.protocol + u);
        if (u.startsWith('/')) path = u;
        else if (u.startsWith('?') || u.startsWith('#') || u === '') path = b.pathname + (u.startsWith('#') ? b.search : '') + u;
        else path = b.pathname.replace(/[^/]*$/, '') + u;
        return new URL(b.protocol + '//' + b.host + path.replace(/\/\.\//g, '/'));
      }
      const [, protocol, host = '', pathname, search = '', hash = ''] = m;
      this.protocol = protocol.toLowerCase();
      this.host = host.toLowerCase();
      const hp = this.host.split(':');
      this.hostname = hp[0];
      this.port = hp[1] || '';
      this.pathname = pathname || (host ? '/' : '');
      this.search = search === '?' ? '' : search;
      this.hash = hash === '#' ? '' : hash;
      this.origin = host ? this.protocol + '//' + this.host : 'null';
      this.username = ''; this.password = '';
      this.searchParams = new URLSearchParams(this.search);
    }
    get href() { const q = this.searchParams.toString(); return this.protocol + (this.host ? '//' + this.host : '') + this.pathname + (q ? '?' + q : '') + this.hash; }
    toString() { return this.href; }
    toJSON() { return this.href; }
    static canParse(u, b) { try { new URL(u, b); return true; } catch (e) { return false; } }
  }
  global.URL = URL;
  global.URLSearchParams = URLSearchParams;

  const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  global.btoa = (s) => {
    s = String(s);
    let out = '';
    for (let i = 0; i < s.length; i += 3) {
      const a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2);
      if (a > 255 || b > 255 || c > 255) throw new DOMException('The string contains characters outside of the Latin1 range.', 'InvalidCharacterError');
      const n = (a << 16) | ((b || 0) << 8) | (c || 0);
      out += B64[(n >> 18) & 63] + B64[(n >> 12) & 63] + (i + 1 < s.length ? B64[(n >> 6) & 63] : '=') + (i + 2 < s.length ? B64[n & 63] : '=');
    }
    return out;
  };
  global.atob = (s) => {
    s = String(s).replace(/[\s=]+/g, '');
    let out = '', bits = 0, acc = 0;
    for (const ch of s) {
      const v = B64.indexOf(ch);
      if (v < 0) throw new DOMException('The string to be decoded is not correctly encoded.', 'InvalidCharacterError');
      acc = (acc << 6) | v; bits += 6;
      if (bits >= 8) { bits -= 8; out += String.fromCharCode((acc >> bits) & 255); }
    }
    return out;
  };

  function makeStorage() {
    const data = new Map();
    const api = {
      getItem: (k) => (data.has(String(k)) ? data.get(String(k)) : null),
      setItem: (k, v) => { data.set(String(k), String(v)); },
      removeItem: (k) => { data.delete(String(k)); },
      clear: () => data.clear(),
      key: (i) => [...data.keys()][i] ?? null,
    };
    return new Proxy(api, {
      get: (t, k) => (k === 'length' ? data.size : k in t ? t[k] : typeof k === 'string' && data.has(k) ? data.get(k) : undefined),
      set: (t, k, v) => { data.set(String(k), String(v)); return true; },
      deleteProperty: (t, k) => { data.delete(String(k)); return true; },
      has: (t, k) => k in t || data.has(k),
      ownKeys: () => [...data.keys()],
      getOwnPropertyDescriptor: (t, k) => (data.has(k) ? { value: data.get(k), writable: true, enumerable: true, configurable: true } : undefined),
    });
  }
  // In memory only: nothing persists beyond the page.
  global.localStorage = makeStorage();
  global.sessionStorage = makeStorage();

  global.__init = (href) => {
    const u = new URL(href);
    global.location = {
      href: u.href, protocol: u.protocol, host: u.host, hostname: u.hostname, port: u.port, pathname: u.pathname,
      search: u.search, hash: u.hash, origin: u.origin,
      assign() {}, replace() {}, reload() {}, toString() { return u.href; },
    };
  };
  global.navigator = {
    userAgent: 'Mozilla/5.0 (compatible; SafeBrowser/0.1)', appName: 'Netscape', appVersion: '5.0', platform: 'Linux', vendor: '', product: 'Gecko',
    language: 'en-US', languages: ['en-US', 'en'], onLine: true, cookieEnabled: false, doNotTrack: '1', hardwareConcurrency: 1, maxTouchPoints: 0,
    sendBeacon() { return false; }, javaEnabled() { return false; }, clipboard: { writeText() { return Promise.reject(new DOMException('Denied', 'NotAllowedError')); }, readText() { return Promise.reject(new DOMException('Denied', 'NotAllowedError')); } },
  };
  global.screen = { width: 1280, height: 800, availWidth: 1280, availHeight: 800, colorDepth: 24, pixelDepth: 24, orientation: { type: 'landscape-primary', angle: 0 } };
  global.history = { length: 1, state: null, scrollRestoration: 'auto', pushState(s) { this.state = s; }, replaceState(s) { this.state = s; }, back() {}, forward() {}, go() {} };
  Object.assign(global, {
    innerWidth: 1280, innerHeight: 800, outerWidth: 1280, outerHeight: 800, devicePixelRatio: 1, scrollX: 0, scrollY: 0, pageXOffset: 0, pageYOffset: 0,
    isSecureContext: true, name: '', closed: false,
    scrollTo() {}, scrollBy() {}, scroll() {}, focus() {}, blur() {}, stop() {}, print() {}, close() {},
    alert(m) { logNative('alert', toStr(m)); }, confirm() { return false; }, prompt() { return null; },
    open() { return null; }, postMessage() {}, getSelection() { return document.getSelection(); },
    matchMedia(q) { return { matches: false, media: String(q), onchange: null, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }; },
    getComputedStyle(el) {
      const s = el.style;
      return new Proxy({}, {
        get(t, k) {
          if (k === 'getPropertyValue') return (n) => s.getPropertyValue(n);
          if (typeof k === 'symbol') return undefined;
          const v = s[k];
          return v === undefined ? '' : v;
        },
      });
    },
    customElements: { define() {}, get() { return undefined; }, whenDefined() { return Promise.resolve(); }, upgrade() {} },
    crypto: {
      getRandomValues(arr) { for (let i = 0; i < arr.length; i++) arr[i] = Math.floor(randomNative() * 4294967296); return arr; },
      randomUUID() { const h = () => Math.floor(randomNative() * 65536).toString(16).padStart(4, '0'); return h() + h() + '-' + h() + '-4' + h().slice(1) + '-a' + h().slice(1) + '-' + h() + h() + h(); },
      subtle: {},
    },
    structuredClone(v) { return v === undefined ? v : JSON.parse(JSON.stringify(v)); },
    reportError,
  });
  for (const name of ['MutationObserver', 'ResizeObserver', 'PerformanceObserver']) {
    global[name] = class { constructor(cb) { this.cb = cb; } observe() {} unobserve() {} disconnect() {} takeRecords() { return []; } };
  }
  // There is no viewport to scroll, so everything counts as visible: lazy-loading code proceeds.
  global.IntersectionObserver = class {
    constructor(cb) { this.cb = cb; }
    observe(target) {
      addTimer(() => this.cb([{ target, isIntersecting: true, intersectionRatio: 1, time: clock, boundingClientRect: target.getBoundingClientRect(), intersectionRect: target.getBoundingClientRect(), rootBounds: null }], this), 0, [], false);
    }
    unobserve() {} disconnect() {} takeRecords() { return []; }
  };

  // Network access from scripts is refused: pages are rendered from what was fetched up front.
  const blocked = () => new TypeError('Network access from scripts is disabled in SafeBrowser');
  global.fetch = () => Promise.reject(blocked());
  global.XMLHttpRequest = class extends EventTarget {
    constructor() { super(); this.readyState = 0; this.status = 0; this.responseText = ''; this.response = ''; }
    open() { this.readyState = 1; }
    setRequestHeader() {}
    getResponseHeader() { return null; }
    getAllResponseHeaders() { return ''; }
    abort() {}
    send() {
      addTimer(() => {
        this.readyState = 4;
        for (const t of ['readystatechange', 'error', 'loadend']) {
          const ev = new Event(t);
          if (typeof this['on' + t] === 'function') this['on' + t](ev);
          this.dispatchEvent(ev);
        }
      }, 0, [], false);
    }
  };
  global.WebSocket = class extends EventTarget { constructor() { super(); throw blocked(); } };
  global.EventSource = global.WebSocket;
  global.Worker = global.SharedWorker = global.WebSocket;

  if (typeof global.Intl === 'undefined') {
    global.Intl = {
      DateTimeFormat: class { format(d) { return String(d); } resolvedOptions() { return { locale: 'en-US', timeZone: 'UTC' }; } formatToParts() { return []; } },
      NumberFormat: class { format(n) { return String(n); } resolvedOptions() { return { locale: 'en-US' }; } },
      Collator: class { compare(a, b) { return a < b ? -1 : a > b ? 1 : 0; } },
      PluralRules: class { select(n) { return n === 1 ? 'one' : 'other'; } },
      RelativeTimeFormat: class { format(v, u) { return v + ' ' + u; } },
      ListFormat: class { format(l) { return [...l].join(', '); } },
    };
  }

  Object.assign(global, {
    EventTarget, Event, CustomEvent, UIEvent, MouseEvent, KeyboardEvent, FocusEvent, InputEvent, ErrorEvent, MessageEvent, PopStateEvent, HashChangeEvent,
    Node, CharacterData, Text, Comment, Element, HTMLElement, Document, DocumentFragment, DOMTokenList, DOMException,
    HTMLDocument: Document, SVGElement: Element,
  });
  // Keep the helpers non-enumerable so `for (k in window)` loops stay quiet.
  for (const k of ['EventTarget', 'Event', 'CustomEvent', 'Node', 'Element', 'HTMLElement', 'Document', 'DocumentFragment', 'DOMException', 'URL', 'URLSearchParams']) {
    Object.defineProperty(global, k, { enumerable: false });
  }
})(globalThis);
