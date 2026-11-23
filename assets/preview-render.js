// The preview page engine. The host evaluates this file once per loaded shell
// page and then calls `window.__lw.render(...)` for every update.
//
// Document text never runs here. It is rebuilt node by node from an allowlist
// into a frame that is sandboxed without scripts. Everything in this file is
// the host's own code, working on that frame from outside.
(() => {
  "use strict";
  if (window.__lw) return;
  const $ = id => document.getElementById(id);
  const frame = $("sheet");
  const TAGS = new Set("a abbr address article aside b bdi bdo blockquote br caption cite code col colgroup dd del details dfn div dl dt em figcaption figure footer h1 h2 h3 h4 h5 h6 header hr i img input ins kbd label li main mark ol p pre q rp rt ruby s samp section small span strong style sub summary sup table tbody td tfoot th thead time tr u ul var wbr".split(" "));
  const ATTRS = new Set("alt class colspan dir height id lang open reversed rowspan scope start style title width".split(" "));
  const DROP = new Set("script noscript iframe frame frameset object embed base meta link title audio video canvas template math".split(" "));
  const BLOCKS = "p,li,td,th,h1,h2,h3,h4,h5,h6,pre,blockquote,dd,dt,div,figcaption,summary,caption,section,details,figure,tr";
  // Cue timing in milliseconds: the box opens, holds while typing continues,
  // then both edges close on its middle.
  const ENTER = 110, FADE = 70, IDLE = 550, FOLD = 140, LINGER = 900;
  const FOLD_EASE = "cubic-bezier(.35,0,.32,1)";
  // Typed text stays warm for INK and pasted text stays washed for ECHO. The
  // host keeps its traces for the same two times. A deletion's ghost lasts
  // GHOST, and a link is peeked at after the pointer rests on it for DWELL.
  const INK = 900, ECHO = 1200, GHOST = 720, DWELL = 240, SLOTS = 8;
  const SPACE = /\s/;
  // Scripts written without spaces between words. A stretch of these is split
  // by the platform's word segmenter instead of at white space.
  const DENSE = /[฀-໿က-႟ក-៿぀-ヿ㐀-䶿一-鿿豈-﫿ｦ-ﾟ]/;
  const calmSystem = matchMedia("(prefers-reduced-motion: reduce)");
  const S = {
    doc: null, ui: null, view: "", rev: -1, mapped: false, live: false,
    edit: null, played: 0, span: null,
    opts: {
      style: "pill", letters: false, margin: false, dim: 23, calm: false, click: true,
      ink: true, echo: true, ghosts: true, peek: true, mirror: true, follow: false, tasks: true,
    },
    host: "", anim: null, barAnim: null, poolAnim: null, holdEnd: 0, queued: false,
    // Traces being painted, the selection and caret the host last reported,
    // and the caret position the page last scrolled to.
    slots: [], want: null, followed: -1, peeks: 0,
  };
  const post = message => {
    const host = window.chrome && window.chrome.webview;
    if (S.live && host) host.postMessage(message);
  };
  const article = () => (S.doc ? S.doc.querySelector("article") : null);

  // ---- text positions: source bytes are UTF-8, the page counts UTF-16 units ----
  const u8 = c => (c < 0x80 ? 1 : c < 0x800 ? 2 : 3);
  const pairAt = (text, i) => {
    const c = text.charCodeAt(i);
    return c >= 0xd800 && c < 0xdc00 && i + 1 < text.length && (text.charCodeAt(i + 1) & 0xfc00) === 0xdc00;
  };
  /** Index of the character boundary at or before `bytes` UTF-8 bytes. */
  function index16(text, bytes) {
    let b = 0, i = 0;
    while (i < text.length) {
      const pair = pairAt(text, i), w = pair ? 4 : u8(text.charCodeAt(i));
      if (b + w > bytes) break;
      b += w;
      i += pair ? 2 : 1;
    }
    return i;
  }
  /** UTF-8 length of the first `end` UTF-16 units, never splitting a pair. */
  function bytes8(text, end) {
    let b = 0;
    for (let i = 0; i < end && i < text.length; i++) {
      if (pairAt(text, i)) {
        if (i + 1 >= end) break;
        b += 4;
        i++;
      } else b += u8(text.charCodeAt(i));
    }
    return b;
  }

  // ---- building the page from sanitized nodes ----
  function build(d, p) {
    const out = d.createDocumentFragment();
    out.limited = false;
    if (p.kind === "SVG") {
      if (p.content) {
        const img = d.createElement("img");
        img.className = "diagram"; img.alt = "SVG diagram";
        img.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(p.content);
        out.append(img);
      }
      return out;
    }
    // A template is inert during parsing. Only explicit reading elements
    // and attributes are recreated in the sandbox. Source code never runs.
    const template = d.createElement("template");
    template.innerHTML = p.content;
    // Source positions are accepted only with this render's own nonce, so
    // markup written in the document cannot claim a position.
    const marker = p.mapped && p.nonce ? "lw" + p.nonce + ":" : null;
    // `base` is the source offset of the block being read, `top` the offset
    // recorded on the top-level node that holds it.
    let base = 0, top = 0, remaining = 20000;
    const put = (parent, node) => {
      parent.append(node);
      if (parent === out) { node.__b = base; top = base; }
    };
    const todo = [...template.content.childNodes].reverse().map(n => [n, out]);
    while (todo.length && remaining-- > 0) {
      const [n, parent] = todo.pop();
      if (n.nodeType === Node.TEXT_NODE) {
        const last = parent.lastChild;
        if (last && last.nodeType === Node.TEXT_NODE) last.appendData(n.data);
        else put(parent, d.createTextNode(n.data));
        continue;
      }
      if (n.nodeType === Node.COMMENT_NODE) {
        if (marker && n.data.startsWith(marker)) {
          const value = Number(n.data.slice(marker.length));
          if (Number.isSafeInteger(value) && value >= 0) base = value;
          remaining++;
        }
        continue;
      }
      if (n.nodeType !== Node.ELEMENT_NODE) continue;
      const tag = n.localName.toLowerCase();
      if (DROP.has(tag)) continue;
      let target = parent;
      if (tag === "svg") {
        const img = d.createElement("img"); img.alt = n.getAttribute("aria-label") || "Diagram";
        img.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(new XMLSerializer().serializeToString(n));
        put(parent, img); continue;
      }
      const o = Number(n.getAttribute("data-o")), l = Number(n.getAttribute("data-l"));
      if (marker && tag === "span" && n.getAttribute("data-lw") === p.nonce
          && Number.isSafeInteger(o) && Number.isSafeInteger(l) && o >= 0 && l >= 0) {
        // A text run with its place in the source, kept relative to its
        // top-level block so text typed above it leaves the node unchanged.
        target = d.createElement("span");
        put(parent, target);
        target.setAttribute("data-o", o + base - top);
        target.setAttribute("data-l", l);
        if (n.hasAttribute("data-x")) target.setAttribute("data-x", "");
        remaining++;
      } else if (TAGS.has(tag)) {
        if (tag === "input" && (n.getAttribute("type") || "").toLowerCase() !== "checkbox") continue;
        target = d.createElement(tag);
        for (const a of n.attributes) if (ATTRS.has(a.name)) target.setAttribute(a.name, a.value);
        // A link inside the page. The frame's own address makes it a jump
        // within this document instead of a navigation to the shell.
        const href = tag === "a" ? n.getAttribute("href") || "" : "";
        if (href.startsWith("#")) target.setAttribute("href", "about:srcdoc" + href);
        // Any other destination is kept as inert text, to show on hover.
        else if (href) target.setAttribute("data-href", href.slice(0, 2048));
        if (tag === "img") {
          const src = (n.getAttribute("src") || "").trim();
          if (/^data:image\/(png|jpeg|gif|webp|avif|bmp|svg\+xml)[;,]/i.test(src)) target.src = src;
          else if (src && !/^[a-z][a-z0-9+.-]*:|^[\\/]/i.test(src)) {
            if (URL.canParse(src, p.base)) {
              const url = new URL(src, p.base);
              if (url.href.startsWith(p.base)) target.src = url.href;
            }
          }
        }
        if (tag === "input") {
          target.type = "checkbox"; target.defaultChecked = n.hasAttribute("checked"); target.disabled = true;
          // A task box that knows where its `[ ]` is can be ticked from here.
          const at = Number(n.getAttribute("data-o"));
          if (marker && n.getAttribute("data-lw") === p.nonce && Number.isSafeInteger(at) && at >= 0) {
            target.setAttribute("data-o", at + base - top);
            target.disabled = !(p.live && S.opts.tasks);
          }
        }
        put(parent, target);
      }
      for (let i = n.childNodes.length - 1; i >= 0; i--) todo.push([n.childNodes[i], target]);
    }
    out.limited = todo.length > 0;
    return out;
  }

  /** Replace only what changed and keep the reader's place on the page. */
  function patch(root, out, same, win) {
    if (!same) {
      root.replaceChildren(out);
      if (win.scrollX || win.scrollY) win.scrollTo({left: 0, top: 0, behavior: "instant"});
      return;
    }
    const x = win.scrollX, y = win.scrollY;
    // Keep unchanged paragraphs, selected text and decoded images attached.
    // Compare only sanitized nodes, including attributes such as checkbox state.
    let first = root.firstChild;
    while (first && out.firstChild && first.isEqualNode(out.firstChild)) {
      first.__b = out.firstChild.__b;
      first = first.nextSibling;
      out.firstChild.remove();
    }
    let after = null;
    let last = root.lastChild;
    while (first && first !== after && last && out.lastChild && last.isEqualNode(out.lastChild)) {
      last.__b = out.lastChild.__b;
      after = last;
      last = last.previousSibling;
      out.lastChild.remove();
    }
    // The reader's place is the first block still on screen. If that block is
    // the one being replaced, hold the bottom edge of the block before it.
    let hold = null;
    if (y > 0) {
      const kids = root.children;
      let lo = 0, hi = kids.length;
      while (lo < hi) {
        const mid = (lo + hi) >>> 1;
        if (kids[mid].getBoundingClientRect().bottom <= 0) lo = mid + 1; else hi = mid;
      }
      let el = kids[lo] || null, edge = "top";
      for (let n = first; el && n && n !== after; n = n.nextSibling) {
        if (n !== el) continue;
        el = first.previousSibling;
        while (el && el.nodeType !== Node.ELEMENT_NODE) el = el.previousSibling;
        edge = "bottom";
        break;
      }
      if (el) hold = {el, edge, at: el.getBoundingClientRect()[edge]};
    }
    while (first && first !== after) {
      const next = first.nextSibling;
      first.remove();
      first = next;
    }
    if (out.firstChild) root.insertBefore(out, after);
    let top = y;
    if (hold && hold.el.isConnected) top = win.scrollY + hold.el.getBoundingClientRect()[hold.edge] - hold.at;
    if (win.scrollX !== x || Math.abs(win.scrollY - top) > 0.5) win.scrollTo({left: x, top, behavior: "instant"});
  }

  // ---- finding rendered text for a source position ----
  /** The mapped text runs inside one top-level block, in source order. */
  function runsOf(block) {
    if (block.nodeType !== Node.ELEMENT_NODE) return [];
    let list = block.__runs;
    if (!list) {
      list = [];
      const add = el => {
        const node = el.firstChild;
        if (!node || node.nodeType !== Node.TEXT_NODE || node.nextSibling) return;
        const l = Number(el.getAttribute("data-l"));
        // "Exact" means the run shows its source bytes unchanged, so a
        // position inside it can be found character by character.
        list.push({
          el, node, o: Number(el.getAttribute("data-o")), l,
          exact: !el.hasAttribute("data-x") && bytes8(node.data, node.length) === l,
        });
      };
      if (block.hasAttribute("data-o")) add(block);
      for (const el of block.querySelectorAll("span[data-o]")) add(el);
      block.__runs = list;
    }
    return list;
  }
  /** The run that starts at or before a source byte, and the run after it. */
  function seek(root, at) {
    const kids = root.children;
    const made = (block, r) => {
      const s = (block.__b || 0) + r.o;
      return {el: r.el, node: r.node, s, e: s + r.l, exact: r.exact, block};
    };
    let lo = 0, hi = kids.length - 1, k = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if ((kids[mid].__b || 0) <= at) { k = mid; lo = mid + 1; } else hi = mid - 1;
    }
    let prev = null, next = null;
    if (k >= 0) {
      const block = kids[k], list = runsOf(block), b = block.__b || 0;
      let a = 0, z = list.length - 1, j = -1;
      while (a <= z) {
        const mid = (a + z) >> 1;
        if (b + list[mid].o <= at) { j = mid; a = mid + 1; } else z = mid - 1;
      }
      if (j >= 0) prev = made(block, list[j]);
      if (j + 1 < list.length) next = made(block, list[j + 1]);
    }
    for (let i = k - 1; !prev && i >= 0 && i >= k - 40; i--) {
      const list = runsOf(kids[i]);
      if (list.length) prev = made(kids[i], list[list.length - 1]);
    }
    for (let i = k + 1; !next && i < kids.length && i <= k + 40; i++) {
      const list = runsOf(kids[i]);
      if (list.length) next = made(kids[i], list[0]);
    }
    return {prev, next, block: k >= 0 ? kids[k] : kids[0] || null};
  }
  /** A place in the page for a source byte: inside a run, or beside the gap. */
  function point(root, at, back) {
    const {prev, next, block} = seek(root, at);
    const inside = r => {
      const n = r.node.length;
      const i = r.exact ? index16(r.node.data, at - r.s) : (at - r.s) * 2 < r.e - r.s ? 0 : n;
      return {node: r.node, i, gap: 0, block: r.block};
    };
    if (prev && at <= prev.e && !(at === prev.e && !back && next && next.s === at)) return inside(prev);
    const before = prev ? {node: prev.node, i: prev.node.length, gap: at - prev.e, block: prev.block} : null;
    const behind = next ? {node: next.node, i: 0, gap: next.s - at, block: next.block} : null;
    const pick = back ? before || behind : behind || before;
    return pick || {node: null, i: 0, gap: Infinity, block};
  }
  /** The character shown for a source byte, or "" when it is not page text. */
  function charAt(root, at) {
    if (at < 0) return "";
    const r = seek(root, at).prev;
    if (!r || !r.exact || at >= r.e) return "";
    const text = r.node.data;
    return text[index16(text, at - r.s)] || "";
  }
  /**
   * What a deletion left behind, judged on the text as it was before:
   * the left part of a word, the right part of a word, or a clean join.
   */
  function classify(root, e) {
    const solid = at => { const ch = charAt(root, at); return ch !== "" && !SPACE.test(ch); };
    const a = e.at, z = e.at + e.del;
    if (solid(a - 1) && solid(a)) return "left";
    if (solid(z - 1) && solid(z)) return "right";
    return "join";
  }

  // ---- where an edit shows on the page ----
  function rectsOf(node, a, b) {
    const range = node.ownerDocument.createRange();
    range.setStart(node, a); range.setEnd(node, b);
    return range.getClientRects();
  }
  /** The caret's place before character `i` of a text node, in frame coordinates. */
  function caretRect(node, i) {
    const n = node.length, text = node.data;
    i = Math.max(0, Math.min(n, i));
    const flat = rectsOf(node, i, i)[0];
    if (flat && flat.height) return {x: flat.left, top: flat.top, bottom: flat.bottom};
    // Some layouts give an empty range no box. Measure a neighbour instead.
    if (i > 0 && text[i - 1] !== "\n") {
      const list = rectsOf(node, i > 1 && pairAt(text, i - 2) ? i - 2 : i - 1, i), r = list[list.length - 1];
      if (r && r.height) return {x: r.right, top: r.top, bottom: r.bottom};
    }
    if (i < n) {
      const r = rectsOf(node, i, pairAt(text, i) ? i + 2 : i + 1)[0];
      if (r && r.height) return {x: r.left, top: r.top, bottom: r.bottom};
    }
    const r = node.parentElement ? node.parentElement.getBoundingClientRect() : null;
    return r && r.height ? {x: r.left, top: r.top, bottom: r.bottom} : null;
  }
  /** The word that holds character `i`, as a range, or null on white space. */
  function wordAround(node, i) {
    const text = node.data;
    if (i < 0 || i >= text.length || SPACE.test(text[i])) return null;
    const lo = Math.max(0, i - 256), hi = Math.min(text.length, i + 256);
    let a = i, b = i + 1;
    while (a > lo && !SPACE.test(text[a - 1])) a--;
    while (b < hi && !SPACE.test(text[b])) b++;
    const d = node.ownerDocument, range = d.createRange();
    const piece = text.slice(a, b);
    if (DENSE.test(piece) && window.Intl && Intl.Segmenter) {
      for (const part of new Intl.Segmenter(undefined, {granularity: "word"}).segment(piece)) {
        const from = a + part.index, to = from + part.segment.length;
        if (from <= i && i < to) { range.setStart(node, from); range.setEnd(node, to); return range; }
      }
    }
    range.setStart(node, a); range.setEnd(node, b);
    if (a > 0 && b < text.length) return range;
    // A word can continue through a formatting boundary: half of it in bold,
    // a link in the middle. Follow it, but never out of its own block.
    const host = (node.parentElement && node.parentElement.closest(BLOCKS)) || node.parentElement;
    if (!host) return range;
    const walk = d.createTreeWalker(host, NodeFilter.SHOW_TEXT);
    if (a === 0) {
      walk.currentNode = node;
      for (let hops = 0, n; hops < 6 && (n = walk.previousNode()); hops++) {
        const s = n.data;
        let j = s.length;
        while (j > 0 && !SPACE.test(s[j - 1])) j--;
        if (j < s.length) range.setStart(n, j);
        if (j > 0) break;
      }
    }
    if (b === text.length) {
      walk.currentNode = node;
      for (let hops = 0, n; hops < 6 && (n = walk.nextNode()); hops++) {
        const s = n.data;
        let j = 0;
        while (j < s.length && !SPACE.test(s[j])) j++;
        if (j > 0) range.setEnd(n, j);
        if (j < s.length) break;
      }
    }
    return range;
  }
  /** One box around the given rectangles; with a caret tip, only its line. */
  function union(rects, tip) {
    let box = null;
    for (const r of rects) {
      if (!r.height || (tip && !(tip.top < r.bottom - 1 && tip.bottom > r.top + 1))) continue;
      if (!box) box = {left: r.left, top: r.top, right: r.right, bottom: r.bottom};
      else {
        box.left = Math.min(box.left, r.left); box.top = Math.min(box.top, r.top);
        box.right = Math.max(box.right, r.right); box.bottom = Math.max(box.bottom, r.bottom);
      }
    }
    return box;
  }
  /** Keep a box inside every scrolling ancestor, as the text itself is. */
  function clipBox(box, el, root) {
    const win = frame.contentWindow;
    for (let a = el; a && a !== root; a = a.parentElement) {
      if (a.scrollWidth <= a.clientWidth && a.scrollHeight <= a.clientHeight) continue;
      const cs = win.getComputedStyle(a);
      if (cs.overflowX === "visible" && cs.overflowY === "visible") continue;
      const r = a.getBoundingClientRect();
      const left = Math.min(Math.max(box.left, r.left), r.right), top = Math.min(Math.max(box.top, r.top), r.bottom);
      box = {
        left, top,
        right: Math.max(left, Math.min(box.right, r.right)),
        bottom: Math.max(top, Math.min(box.bottom, r.bottom)),
      };
    }
    return box;
  }
  /** A whole block, for source the page does not show as text. */
  function coarse(block) {
    if (!block || !block.isConnected) return null;
    const r = block.getBoundingClientRect();
    if (!r.height) return null;
    return {
      box: {left: r.left, top: r.top, right: r.right, bottom: r.bottom},
      tip: {x: r.left, top: r.top, bottom: Math.min(r.bottom, r.top + 24)},
      thin: false, coarse: true,
    };
  }
  /**
   * Resolve an edit to what the reader should look at: a box and the caret
   * tip, in frame coordinates, or null when the page shows nothing for it.
   * `thin` marks one position (a join, a break). `coarse` is a whole block.
   */
  function target(root, e) {
    const d = root.ownerDocument, letters = !!S.opts.letters;
    const start = e.at, end = e.at + e.add;
    const solid = ch => ch !== "" && ch !== undefined && !SPACE.test(ch);
    const whole = n => { const r = d.createRange(); r.selectNodeContents(n); return r; };
    // The word a blank or a closing mark has just completed.
    const lastWord = n => {
      const text = n.data;
      let j = text.length - 1;
      while (j >= 0 && (text[j] === " " || text[j] === "\t")) j--;
      return wordAround(n, j);
    };
    let node = null, c = 0, range = null, wide = false, big = null, block = null;
    const set = p => { node = p.node; c = p.i; block = p.block; };
    if (e.kind !== "type") {
      // Undo and redo: all of the text that came back, or the join it left.
      if (e.add > 0) {
        const a = point(root, start, false), z = point(root, end, true);
        block = z.block || a.block;
        if (a.node && z.node && a.gap < e.add && z.gap < e.add) {
          set(z);
          if (e.add > 8192 && a.block && z.block) big = [a.block, z.block];
          else {
            const r = d.createRange();
            r.setStart(a.node, a.i); r.setEnd(z.node, z.i);
            if (!r.collapsed) { range = r; wide = true; }
          }
        } else if (z.node && z.gap <= 3) set(z);
        else if (a.node && a.gap <= 3) set(a);
      } else {
        const p = point(root, start, true), q = point(root, start, false);
        if (p.node && p.gap <= 3) set(p);
        else if (q.node && q.gap <= 3) set(q);
        else block = p.block || q.block;
      }
    } else if (e.add > 0) {
      const found = seek(root, end - 1), prev = found.prev, next = found.next;
      block = found.block;
      if (prev && end <= prev.e) {
        // The last typed character is on the page.
        node = prev.node; block = prev.block;
        if (!prev.exact) { c = node.length; range = whole(node); }
        else {
          const text = node.data;
          c = index16(text, end - prev.s);
          if (letters) {
            const a = index16(text, Math.max(0, start - prev.s));
            if (a < c && /\S/.test(text.slice(a, c))) {
              range = d.createRange();
              range.setStart(node, a); range.setEnd(node, c);
            }
          } else {
            // A blank holds the word it completed.
            let j = c - 1;
            while (j >= 0 && (text[j] === " " || text[j] === "\t")) j--;
            range = wordAround(node, j);
          }
        }
      } else if (prev && prev.block === found.block && end - prev.e <= 3 && e.tail !== 2) {
        // A closing mark or a trimmed blank right after text.
        node = prev.node; c = node.length; block = prev.block;
        if (!letters) range = prev.exact ? lastWord(node) : whole(node);
      } else if (next && next.s >= end && next.s - end <= 3) {
        // An opening mark right before text.
        node = next.node; c = 0; block = next.block;
        if (!letters && e.tail !== 2) range = next.exact ? wordAround(node, 0) : whole(node);
      } else if (prev && end - prev.e <= 3) {
        node = prev.node; c = node.length; block = prev.block;
      }
    } else {
      // A deletion: follow the part of the word that survived, or mark the join.
      const p = point(root, start, true), q = point(root, start, false);
      const before = p.node && p.gap === 0 && p.i > 0 ? p.node.data[p.i - 1] : "";
      const after = q.node && q.gap === 0 ? q.node.data[q.i] : "";
      let mode = e.mode || (solid(before) ? "left" : "join");
      if ((mode === "left" && !solid(before)) || (mode === "right" && !solid(after))) mode = "join";
      if (mode === "left") { set(p); if (!letters) range = wordAround(p.node, p.i - 1); }
      else if (mode === "right") { set(q); if (!letters) range = wordAround(q.node, q.i); }
      else if (p.node && p.gap <= 3) set(p);
      else if (q.node && q.gap <= 3) set(q);
      else block = p.block || q.block;
    }
    if (!node) return coarse(block);
    const tip = caretRect(node, c);
    if (!tip) return coarse(block);
    let box = null, thin = false;
    if (big) box = union([big[0].getBoundingClientRect(), big[1].getBoundingClientRect()], null);
    else if (range) {
      const rects = range.getClientRects();
      box = (wide ? null : union(rects, tip)) || union(rects, null);
    }
    if (!box) {
      thin = true;
      box = {left: tip.x - 1, right: tip.x + 1, top: tip.top, bottom: tip.bottom};
    } else if (S.opts.style === "block" && !wide && !big) {
      const holder = node.parentElement && node.parentElement.closest(BLOCKS);
      if (holder && holder !== root && root.contains(holder)) {
        const r = holder.getBoundingClientRect();
        box = {left: r.left, top: r.top, right: r.right, bottom: r.bottom};
      }
    }
    return {box: clipBox(box, node.parentElement, root), tip, thin, coarse: false};
  }
  /** A marker at one source position, for "show the caret in the preview". */
  function targetAt(root, at) {
    const p = point(root, at, true), q = point(root, at, false);
    const pick = p.node && p.gap <= q.gap ? p : q.node ? q : p;
    if (!pick.node || pick.gap > 64) return coarse(pick.block);
    const tip = caretRect(pick.node, pick.i);
    if (!tip) return coarse(pick.block);
    const box = {left: tip.x - 1, right: tip.x + 1, top: tip.top, bottom: tip.bottom};
    return {box: clipBox(box, pick.node.parentElement, root), tip, thin: true, coarse: false};
  }

  // ---- the cue itself: one timeline per edit, no timers ----
  function stop() {
    for (const key of ["anim", "barAnim", "poolAnim"]) {
      if (S[key]) S[key].cancel();
      S[key] = null;
    }
  }
  /** "live" while open or holding, "folding" while closing, else "none". */
  function phase() {
    const a = S.anim || S.barAnim;
    if (!a || a.playState !== "running") return "none";
    return (a.currentTime || 0) < S.holdEnd ? "live" : "folding";
  }
  /** Start the fold now: the source caret has left the edit. */
  function rest() {
    for (const key of ["anim", "barAnim", "poolAnim"]) {
      const a = S[key];
      if (a && a.playState === "running" && (a.currentTime || 0) < S.holdEnd) a.currentTime = S.holdEnd;
    }
    return true;
  }
  /**
   * Draw the cue on a target. A cue that is already open moves with the text
   * in this same frame and only its hold restarts, so typing never replays
   * the entry and a word that wraps is followed without a sweep.
   */
  function present(t, hold) {
    const d = S.doc, ui = S.ui, win = frame.contentWindow, o = S.opts;
    if (!d || !ui || !t) { stop(); return; }
    const vw = win.innerWidth, vh = win.innerHeight;
    const spot = o.style !== "off" && !(t.coarse && o.style !== "block");
    // Offscreen edits are offered by the Show edit button, never chased.
    if (t.box.bottom <= 0 || t.box.top >= vh || !(spot || o.margin)) { stop(); return; }
    const base = ui.cue.getBoundingClientRect();
    const root = d.documentElement, sw = root.scrollWidth, sh = root.scrollHeight;
    const blockish = !t.thin && (o.style === "block" || t.coarse);
    const px = blockish ? 6 : t.thin ? 0 : 1, py = blockish ? 2 : 1;
    // Nothing the cue draws may widen or lengthen the page.
    const w = Math.min(Math.max(2, t.box.right - t.box.left) + px * 2, sw);
    const y = t.box.top - base.top - py;
    const h = Math.max(1, Math.min(t.box.bottom - t.box.top + py * 2, sh - y));
    const cx = (t.box.left + t.box.right) / 2 - base.left;
    const x = Math.max(0, Math.min(cx - w / 2, sw - w));
    const calm = o.calm || calmSystem.matches;
    const state = phase();
    // A cue caught while closing reopens from where it is, unless the text
    // moved to another line: then it enters afresh, never across the page.
    let from = null;
    if (state === "folding" && !calm && S.anim) {
      const cs = win.getComputedStyle(ui.spot);
      const now = {x: parseFloat(cs.left), y: parseFloat(cs.top), w: parseFloat(cs.width), h: parseFloat(cs.height), o: parseFloat(cs.opacity)};
      if (Math.abs(now.y - y) < 1.5 && Math.abs(now.h - h) < 1.5 && now.w >= 0) from = now;
    }
    stop();
    const total = hold + (calm ? 0 : FOLD);
    S.holdEnd = hold;
    const at = (left, width) => ({left: left + "px", top: y + "px", width: width + "px", height: h + "px"});
    const full = at(x, w);
    const start = state === "live" ? 1 : from ? from.o : 0;
    let frames, fade;
    if (calm) {
      frames = [{...full, opacity: 1}, {...full, opacity: 1}];
      fade = [{opacity: 1}, {opacity: 1}];
    } else {
      const lit = (hold + FOLD * 0.45) / total;
      const tail = [
        {...full, opacity: 1, offset: hold / total, easing: FOLD_EASE},
        {opacity: 1, offset: lit},
        {...at(x + w / 2, 0), opacity: 0, offset: 1},
      ];
      if (state === "live") frames = [{...full, opacity: 1, offset: 0}, ...tail];
      else if (from) {
        frames = [
          {...at(from.x, from.w), opacity: from.o, offset: 0, easing: "ease-out"},
          {...full, opacity: 1, offset: ENTER / total},
          ...tail,
        ];
      } else {
        // Grow out of the caret: a sliver at the tip opens to the full box.
        const tipX = Math.max(x, Math.min(t.tip.x - base.left, x + w));
        frames = [
          {...at(tipX - 0.5, 1), opacity: 0, offset: 0, easing: "ease-out"},
          {opacity: 1, offset: FADE / total},
          {...full, offset: ENTER / total},
          ...tail,
        ];
      }
      fade = [{opacity: start, offset: 0}, {opacity: 1, offset: FADE / total}, {opacity: 1, offset: lit}, {opacity: 0, offset: 1}];
    }
    ui.cue.style.setProperty("--lw-reach", Math.ceil((Math.hypot(vw, vh) + 60) / 80) * 80 + "px");
    ui.cue.style.setProperty("--lw-dim", o.dim + "%");
    if (spot) {
      ui.spot.dataset.style = o.style;
      ui.spot.toggleAttribute("data-thin", t.thin);
      S.anim = ui.spot.animate(frames, {duration: total});
    }
    // The pool and the bar take their size from the timeline too, so they
    // are empty boxes again the moment it ends, with no clean-up to run.
    const sized = box => fade.map(step => ({...step, ...box}));
    if (spot && o.style === "beam" && o.dim > 0) {
      // A pool of light: one box, kept inside the page, with a clear middle.
      const reach = Math.hypot(vw, vh) + 60, cy = y + h / 2;
      const left = Math.max(0, cx - reach), top = Math.max(0, cy - reach);
      ui.pool.style.setProperty("--lw-bx", cx - left + "px");
      ui.pool.style.setProperty("--lw-by", cy - top + "px");
      S.poolAnim = ui.pool.animate(sized({
        left: left + "px", top: top + "px",
        width: Math.max(0, Math.min(sw, cx + reach) - left) + "px",
        height: Math.max(0, Math.min(sh, cy + reach) - top) + "px",
      }), {duration: total});
    }
    if (o.margin) {
      const sheet = article(), edge = sheet.getBoundingClientRect();
      const pad = parseFloat(win.getComputedStyle(sheet).paddingLeft) || 16;
      S.barAnim = ui.bar.animate(sized({
        left: Math.max(2, edge.left - base.left + Math.max(2, pad - 11)) + "px",
        top: y + "px", width: "3px", height: h + "px",
      }), {duration: total});
    }
  }

  // ---- traces: typed text settles from a warm color, pasted text from a wash ----
  // The text is painted through the highlight registry. No node is added to
  // the page, so nothing shifts and nothing becomes part of a selection. Each
  // trace owns one slot. The fade is an animated variable on the blocks that
  // hold the text, restarted at the trace's age whenever a block is replaced.
  function quiet(slot) {
    for (const a of slot.anims) a.cancel();
    slot.anims = []; slot.els = null;
  }
  function release(slot, reg) {
    if (slot.name) reg.delete(slot.name);
    quiet(slot);
    slot.id = 0; slot.name = null; slot.born = 0;
  }
  /** The top-level blocks from one to another, or the whole page if many. */
  function between(root, a, b) {
    const list = [];
    for (let n = a; n && list.length < 24; n = n.nextElementSibling) {
      list.push(n);
      if (n === b) return list;
    }
    return a === b || !b ? list.slice(0, 1) : [root];
  }
  function spanOf(root, from, to) {
    const a = point(root, from, false), z = point(root, to, true);
    if (!a.node || !z.node) return null;
    const range = S.doc.createRange();
    range.setStart(a.node, a.i); range.setEnd(z.node, z.i);
    return range.collapsed ? null : {range, a: a.block, z: z.block};
  }
  /** Paint the traces the host reported with this page. */
  function paint(marks, root, win) {
    const reg = win.CSS && win.CSS.highlights;
    if (!reg || !win.Highlight) return;
    const o = S.opts, now = performance.now(), used = new Set();
    for (const [id, pasted, from, to, age] of marks) {
      const life = pasted ? ECHO : INK;
      if ((pasted ? !o.echo : !o.ink) || !(age < life)) continue;
      let slot = S.slots.find(x => x.id === id);
      if (!slot) {
        slot = S.slots.find(x => !x.id) || S.slots.reduce((a, b) => (a.born <= b.born ? a : b));
        release(slot, reg);
        slot.id = id;
      }
      used.add(slot);
      const name = (pasted ? "lw-echo" : "lw-ink") + slot.n;
      if (slot.name && slot.name !== name) reg.delete(slot.name);
      slot.name = name;
      const span = spanOf(root, from, to);
      if (!span) { reg.delete(name); quiet(slot); continue; }
      reg.set(name, new win.Highlight(span.range));
      const els = between(root, span.a, span.z), born = now - age;
      const kept = slot.els && slot.els.length === els.length && els.every((el, i) => el === slot.els[i])
        && Math.abs(born - slot.born) < 40 && slot.anims.every(a => a.playState === "running");
      if (kept) continue;
      quiet(slot);
      // Typed text settles into the color it has where it stands. That color
      // rides on the same timeline, so no page node is touched.
      const prop = (pasted ? "--lw-e" : "--lw-k") + slot.n, tone = "--lw-c" + slot.n;
      const holder = span.range.startContainer.parentElement;
      const own = holder ? win.getComputedStyle(holder).color : "";
      const step = (value, offset) => ({[prop]: value, [tone]: own, offset});
      slot.anims = els.map(el => {
        const a = el.animate([step("100%", 0), step("100%", pasted ? 0.18 : 0.3), step("0%", 1)], {duration: life, easing: "ease-out"});
        a.currentTime = age;
        return a;
      });
      slot.els = els; slot.born = born;
      // When the fade is over the text is plain page text again.
      const last = slot.anims[0];
      if (last) last.onfinish = () => { if (slot.anims[0] === last) release(slot, reg); };
    }
    for (const slot of S.slots) if (slot.id && !used.has(slot)) release(slot, reg);
  }
  /** Rules for the slots. They follow the reading styles in the page head. */
  function slotRules(d) {
    let css = "";
    for (let i = 0; i < SLOTS; i++) {
      css += `::highlight(lw-ink${i}){color:color-mix(in srgb,var(--warm) var(--lw-k${i},0%),var(--lw-c${i},var(--text)))}`
        + `::highlight(lw-echo${i}){background-color:color-mix(in srgb,var(--accent) calc(var(--lw-e${i},0%)*.3),transparent);`
        + `text-decoration:underline 2px color-mix(in srgb,var(--accent) var(--lw-e${i},0%),transparent)}`;
    }
    const style = d.createElement("style");
    style.textContent = css;
    return style;
  }

  // ---- deletion ghosts: what was removed lingers above the join ----
  /** The rendered text a typed deletion is about to remove, or "". */
  function doomed(root, e) {
    const span = spanOf(root, e.at, e.at + e.del);
    if (!span) return "";
    const text = span.range.toString().replace(/\s+/g, " ").trim();
    return text.replace(/\s/g, "").length >= 2 ? text.slice(0, 120) : "";
  }
  /** Float the removed text over the place it left. The page text stays put. */
  function ghost(root, e, text, win) {
    const ui = S.ui, d = S.doc;
    const p = point(root, e.at, true), q = point(root, e.at, false);
    const pick = p.node && p.gap <= (q.node ? q.gap : Infinity) ? p : q;
    const tip = pick.node ? caretRect(pick.node, pick.i) : null;
    if (!ui || !tip || tip.bottom <= 0 || tip.top >= win.innerHeight) return;
    const el = d.createElement("div");
    el.className = "lw-ghost";
    el.textContent = text;
    const host = pick.node.parentElement, cs = host ? win.getComputedStyle(host) : null;
    if (cs) {
      el.style.fontFamily = cs.fontFamily; el.style.fontWeight = cs.fontWeight; el.style.fontStyle = cs.fontStyle;
      el.style.fontSize = (parseFloat(cs.fontSize) || 15) * 0.86 + "px";
    }
    // Nothing the ghost draws may widen or lengthen the page.
    const sw = d.documentElement.scrollWidth;
    el.style.maxWidth = Math.max(60, Math.min(sw - 8, 520)) + "px";
    while (ui.ghosts.childElementCount > 3) ui.ghosts.firstElementChild.remove();
    ui.ghosts.append(el);
    const base = ui.ghosts.getBoundingClientRect(), w = el.offsetWidth, h = el.offsetHeight;
    el.style.left = Math.max(2, Math.min(tip.x - base.left - 5, sw - w - 2)) + "px";
    el.style.top = Math.max(0, tip.top - base.top - h + 4) + "px";
    const calm = S.opts.calm || calmSystem.matches;
    const a = el.animate(calm
      ? [{opacity: 1}, {opacity: 1, offset: 0.85}, {opacity: 0}]
      : [{opacity: 0, transform: "translateY(5px)"}, {opacity: 1, transform: "none", offset: 0.14},
        {opacity: 0.92, transform: "none", offset: 0.5}, {opacity: 0, transform: "translateY(-7px)"}],
    {duration: GHOST, easing: "ease-out"});
    a.onfinish = a.oncancel = () => el.remove();
  }

  // ---- the source selection and caret, as the host last reported them ----
  /** Scroll so a target is comfortably in view. False if it already was. */
  function bring(win, t, band) {
    const vh = win.innerHeight;
    if (t.tip.top >= vh * band && t.tip.bottom <= vh * (1 - band)) return false;
    const top = Math.max(0, win.scrollY + t.tip.top - vh * 0.38);
    const far = Math.abs(top - win.scrollY) > vh * 2.5;
    win.scrollTo({left: win.scrollX, top, behavior: far || S.opts.calm || calmSystem.matches ? "instant" : "smooth"});
    return true;
  }
  /**
   * Mirror the source selection and, if asked to, keep the source caret in
   * view. News for a revision the page has not drawn yet waits for it.
   */
  function follow() {
    const w = S.want, root = article(), win = frame.contentWindow;
    const reg = win && win.CSS && win.CSS.highlights;
    const live = !!(w && root && S.mapped && w.rev === S.rev);
    if (reg && win.Highlight) {
      const span = live && S.opts.mirror && w.z > w.a ? spanOf(root, w.a, w.z) : null;
      if (span) reg.set("lw-sel", new win.Highlight(span.range)); else reg.delete("lw-sel");
    }
    if (live && S.opts.follow && w.c >= 0 && w.c !== S.followed) {
      S.followed = w.c;
      const t = targetAt(root, w.c);
      if (t) bring(win, t, 0.1);
    }
  }
  function track(rev, a, z, c) {
    S.want = {rev, a, z, c};
    follow();
    return true;
  }

  // ---- peek: a link's destination, or the Markdown behind a block ----
  const card = $("peek"), stage = $("stage");
  const P = {el: null, kind: "", timer: 0, token: 0, x: 0, y: 0};
  function hidePeek() {
    clearTimeout(P.timer);
    P.el = null; P.kind = "";
    card.classList.remove("on");
  }
  const part = (cls, text) => {
    const n = document.createElement(cls === "pre" ? "pre" : "div");
    if (cls !== "pre") n.className = cls;
    n.textContent = text;
    return n;
  };
  /** Show the card under a box given in frame coordinates, or above it. */
  function showPeek(r, parts) {
    card.replaceChildren(...parts);
    card.style.left = "0px"; card.style.top = "0px";
    const w = card.offsetWidth, h = card.offsetHeight, W = stage.clientWidth, H = stage.clientHeight;
    let y = r.bottom + 8;
    if (y + h > H - 8) y = Math.max(8, r.top - h - 8);
    card.style.left = Math.max(8, Math.min(r.left, W - w - 8)) + "px";
    card.style.top = y + "px";
    card.classList.add("on");
  }
  const slug = text => text.trim().toLowerCase().replace(/[^\p{L}\p{N}\s_-]/gu, "").replace(/\s+/g, "-");
  /** The element a link inside the page leads to: by id, else by heading text. */
  function landing(d, id) {
    let name = id;
    try { name = decodeURIComponent(id); } catch (_) { name = id; }
    const to = d.getElementById(id) || d.getElementById(name);
    if (to) return to;
    const want = name.toLowerCase();
    for (const h of d.querySelectorAll("h1,h2,h3,h4,h5,h6")) if (slug(h.textContent) === want) return h;
    return null;
  }
  function linkPeek(a) {
    if (P.el !== a || !a.isConnected) return;
    const href = a.getAttribute("href") || "";
    let parts;
    if (href.startsWith("about:srcdoc#")) {
      const to = landing(S.doc, href.slice(13));
      if (to) {
        const after = to.nextElementSibling;
        let text = after ? after.textContent.replace(/\s+/g, " ").trim() : "";
        if (text.length > 190) text = text.slice(0, 190).replace(/\s\S*$/, "") + "…";
        parts = [part("k", "On this page"), part("t", to.textContent.trim())];
        if (text) parts.push(part("x", text));
      } else parts = [part("k", "On this page"), part("x", "No heading matches " + href.slice(12))];
    } else {
      const to = a.getAttribute("data-href") || "";
      let host = "";
      try { host = new URL(to).host; } catch (_) { host = ""; }
      parts = [part("k", host ? "Link" : "Link to a file or a relative address")];
      if (host) parts.push(part("t", host));
      parts.push(part("x", to || "(empty)"));
    }
    P.kind = "link";
    showPeek(a.getBoundingClientRect(), parts);
  }
  /** Ask the host for the source of one top-level block. */
  function blockPeek(block, x, y) {
    const at = block.__b || 0, next = block.nextElementSibling;
    const len = next ? (next.__b || 0) - at : 4096;
    if (len <= 0) return;
    P.el = block; P.kind = "wait"; P.x = x; P.y = y; P.token = ++S.peeks;
    post({t: "peek", view: S.view, rev: S.rev, at, len: Math.min(len, 4096), token: P.token});
  }
  /** The host's answer to `blockPeek`. */
  function peeked(token, text) {
    if (token !== P.token || P.kind !== "wait" || !P.el || !P.el.isConnected) return true;
    let shown = String(text).replace(/\s+$/, "");
    if (shown.length > 1500) shown = shown.slice(0, 1500) + "…";
    P.kind = "src";
    showPeek({left: P.x, top: P.y - 6, bottom: P.y + 12}, [part("k", "Markdown source"), part("pre", shown)]);
    return true;
  }
  function onMove(ev) {
    if (!S.opts.peek || !S.live) return;
    const el = ev.target && ev.target.nodeType === Node.ELEMENT_NODE ? ev.target : null, root = article();
    if (!el || !root || !root.contains(el)) { if (P.kind) hidePeek(); return; }
    if ((ev.ctrlKey || ev.metaKey) && S.mapped) {
      let block = el;
      while (block && block.parentNode !== root) block = block.parentNode;
      if (block && block.nodeType === Node.ELEMENT_NODE) {
        if (P.el !== block || (P.kind !== "src" && P.kind !== "wait")) { hidePeek(); blockPeek(block, ev.clientX, ev.clientY); }
        return;
      }
    }
    const a = el.closest("a[href],a[data-href]");
    if (a) {
      if (P.el !== a) { hidePeek(); P.el = a; P.kind = "dwell"; P.timer = setTimeout(linkPeek, DWELL, a); }
      return;
    }
    if (P.kind) hidePeek();
  }
  function onKeyUp(ev) {
    if ((ev.key === "Control" || ev.key === "Meta") && (P.kind === "src" || P.kind === "wait")) hidePeek();
  }

  // ---- the newest edit: where it is, and the button that offers it ----
  function measure() {
    const root = article(), win = frame.contentWindow;
    const t = root && S.edit && S.mapped ? target(root, S.edit) : null;
    S.span = t ? {top: t.box.top + win.scrollY, bottom: t.box.bottom + win.scrollY} : null;
    return t;
  }
  const panel = $("cue-panel"), toggle = $("cue-toggle"), jump = $("show-edit");
  /** Offer the newest edit when it is above or below the reading view. */
  function sync() {
    const e = S.edit, o = S.opts, win = frame.contentWindow;
    let side = 0;
    if (S.live && e && S.span && win && (o.style !== "off" || o.margin)) {
      side = S.span.bottom - win.scrollY <= 0 ? -1 : S.span.top - win.scrollY >= win.innerHeight ? 1 : 0;
    }
    if (!side) {
      if (!jump.hidden) jump.hidden = true;
      return;
    }
    const word = e.kind === "undo" ? "undo" : e.kind === "redo" ? "redo" : "edit";
    const label = (side < 0 ? "↑ " : "↓ ") + "Show " + word;
    if (jump.textContent !== label) {
      jump.textContent = label;
      jump.setAttribute("aria-label", "Show " + word + (side < 0 ? ", above" : ", below"));
    }
    jump.classList.toggle("above", side < 0);
    jump.hidden = false;
  }
  function later(ev) {
    if (ev && ev.type !== "scroll") S.stale = true;
    if (S.queued) return;
    S.queued = true;
    requestAnimationFrame(() => {
      S.queued = false;
      if (S.stale) { S.stale = false; measure(); }
      sync();
    });
  }
  const settle = (win, t) => win.scrollTo({left: win.scrollX, top: Math.max(0, win.scrollY + t.tip.top - win.innerHeight * 0.38), behavior: "instant"});
  /** Bring the newest edit into view and play its cue once more. */
  function show() {
    const win = frame.contentWindow;
    let t = measure();
    if (!t) return false;
    if (t.box.top < 0 || t.box.bottom > win.innerHeight) {
      settle(win, t);
      t = measure();
    }
    stop();
    present(t, LINGER);
    sync();
    return true;
  }
  /** Bring a source position into view and mark it. */
  function reveal(at) {
    const root = article(), win = frame.contentWindow;
    if (!root || !S.mapped) return false;
    let t = targetAt(root, at);
    if (!t) return false;
    if (t.tip.top < win.innerHeight * 0.08 || t.tip.bottom > win.innerHeight * 0.92) {
      settle(win, t);
      t = targetAt(root, at);
    }
    stop();
    present(t, LINGER);
    sync();
    return true;
  }
  /** The source position of the first text in the reading view, as a jump. */
  function where() {
    const root = article();
    if (!root || !S.mapped) return null;
    const kids = root.children;
    let lo = 0, hi = kids.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (kids[mid].getBoundingClientRect().bottom <= 0) lo = mid + 1; else hi = mid;
    }
    for (let i = lo; i < kids.length && i < lo + 8; i++) {
      const list = runsOf(kids[i]);
      let a = 0, z = list.length;
      while (a < z) {
        const mid = (a + z) >>> 1;
        if (list[mid].el.getBoundingClientRect().bottom <= 0) a = mid + 1; else z = mid;
      }
      if (a < list.length) return {t: "jump", view: S.view, rev: S.rev, at: (kids[i].__b || 0) + list[a].o, len: 0};
    }
    return kids[lo] ? {t: "jump", view: S.view, rev: S.rev, at: kids[lo].__b || 0, len: 0} : null;
  }

  // ---- reading the page with the pointer ----
  /** The source position under the pointer, or null off the text. */
  function hit(ev) {
    const d = S.doc, root = article();
    const el = ev.target && ev.target.nodeType === Node.ELEMENT_NODE ? ev.target : null;
    if (!d || !root || !el || el === root || !root.contains(el)) return null;
    const top = n => {
      while (n && n.parentNode !== root) n = n.parentNode;
      return n;
    };
    let node = null, offset = 0;
    if (d.caretPositionFromPoint) {
      const c = d.caretPositionFromPoint(ev.clientX, ev.clientY);
      if (c) { node = c.offsetNode; offset = c.offset; }
    } else if (d.caretRangeFromPoint) {
      const r = d.caretRangeFromPoint(ev.clientX, ev.clientY);
      if (r) { node = r.startContainer; offset = r.startOffset; }
    }
    const run = node && node.nodeType === Node.TEXT_NODE && el.contains(node) ? node.parentElement : null;
    if (run && run.hasAttribute("data-o") && !node.nextSibling && !node.previousSibling) {
      const block = top(run);
      if (!block) return null;
      const text = node.data, l = Number(run.getAttribute("data-l"));
      const s = (block.__b || 0) + Number(run.getAttribute("data-o"));
      const exact = !run.hasAttribute("data-x") && bytes8(text, text.length) === l;
      return {at: exact ? s + bytes8(text, offset) : offset * 2 < text.length ? s : s + l, node, offset, s, exact};
    }
    // Not page text (a picture, a rule, a checkbox, an empty cell): the first
    // text of the same cell, item or block, else the start of the block.
    const block = top(el);
    if (!block) return null;
    const near = el.closest("td,th,li,p,h1,h2,h3,h4,h5,h6,dd,dt,tr");
    const first = (near && near !== root && root.contains(near) ? near : block).querySelector("span[data-o]");
    return {at: (block.__b || 0) + (first ? Number(first.getAttribute("data-o")) : 0)};
  }
  const jumps = ev => S.mapped && (S.opts.click || ev.altKey);
  function onClick(ev) {
    const d = S.doc;
    if (!d || ev.button !== 0) return;
    const box = ev.target && ev.target.closest ? ev.target.closest("input[type=checkbox][data-o]") : null;
    if (box) {
      // The source is what changes. The box turns when the page is redrawn
      // from it, so the two can never disagree.
      ev.preventDefault();
      const root = article();
      let block = box;
      while (block && block.parentNode !== root) block = block.parentNode;
      if (block && !box.disabled) post({t: "task", view: S.view, rev: S.rev, at: (block.__b || 0) + Number(box.getAttribute("data-o"))});
      return;
    }
    const link = ev.target && ev.target.closest ? ev.target.closest("a[href]") : null;
    if (link) {
      // Nothing in this pane navigates. A link to a place on the page
      // scrolls there; with Alt it leads to its own source instead.
      ev.preventDefault();
      const href = link.getAttribute("href") || "";
      if (href.startsWith("about:srcdoc#") && !(ev.altKey && S.mapped)) {
        const to = landing(d, href.slice(13));
        if (to) to.scrollIntoView();
        return;
      }
    }
    if (!jumps(ev) || ev.detail > 1) return;
    // A drag that selected text is reading, not pointing.
    const selection = d.getSelection();
    if (selection && !selection.isCollapsed) return;
    const h = hit(ev);
    if (h) post({t: "jump", view: S.view, rev: S.rev, at: h.at, len: 0});
  }
  function onDouble(ev) {
    if (!jumps(ev) || ev.button !== 0) return;
    const h = hit(ev);
    if (!h || !h.node || !h.exact) return;
    const text = h.node.data;
    let i = Math.min(h.offset, text.length - 1);
    if (i > 0 && SPACE.test(text[i])) i--;
    if (i < 0 || SPACE.test(text[i])) return;
    let a = i, b = i + 1;
    while (a > 0 && !SPACE.test(text[a - 1])) a--;
    while (b < text.length && !SPACE.test(text[b])) b++;
    // Leave the comma or the bracket around the word out of the selection.
    const token = text.slice(a, b);
    const lead = token.match(/^[^\p{L}\p{N}_]*/u)[0].length, trail = token.match(/[^\p{L}\p{N}_]*$/u)[0].length;
    if (lead + trail < token.length) { a += lead; b -= trail; }
    post({t: "jump", view: S.view, rev: S.rev, at: h.s + bytes8(text, a), len: bytes8(text.slice(a, b), b - a)});
  }
  function openPanel(open) {
    panel.hidden = !open;
    toggle.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) (panel.querySelector("input:checked") || panel.querySelector("input")).focus();
  }
  function onDown(ev) {
    if (!panel.hidden) openPanel(false);
    if (P.kind) hidePeek();
    // The second press of a double click selects the word in the source,
    // so the page does not also select it here.
    if (ev.detail === 2 && ev.button === 0 && jumps(ev) && hit(ev)) ev.preventDefault();
  }
  function onKey(ev) {
    if (ev.key !== "Escape" || ev.defaultPrevented) return;
    ev.preventDefault();
    if (P.kind) hidePeek();
    if (!panel.hidden) { openPanel(false); toggle.focus(); }
    else post({t: "focus"});
  }
  function onResize(ev) {
    // The text has moved under the cue. The button keeps offering the edit.
    stop();
    later(ev);
  }
  /** Attach to the reading frame's document: cue elements and listeners. */
  function wire(d) {
    stop();
    S.doc = d;
    const win = frame.contentWindow;
    const make = name => {
      const el = d.createElement("div");
      el.className = name;
      return el;
    };
    const cue = make("lw-cue"), pool = make("lw-pool"), spot = make("lw-spot"), bar = make("lw-bar");
    const ghosts = make("lw-ghosts");
    cue.setAttribute("aria-hidden", "true");
    ghosts.setAttribute("aria-hidden", "true");
    cue.append(pool, spot, bar);
    d.body.append(cue, ghosts);
    S.ui = {cue, pool, spot, bar, ghosts};
    S.slots = Array.from({length: SLOTS}, (_, n) => ({n, id: 0, name: null, els: null, anims: [], born: 0}));
    S.want = null; S.followed = -1;
    hidePeek();
    if (win.CSS && win.CSS.registerProperty) {
      for (let i = 0; i < SLOTS; i++) {
        for (const k of ["k", "e"]) {
          try {
            win.CSS.registerProperty({name: `--lw-${k}${i}`, syntax: "<percentage>", inherits: true, initialValue: "0%"});
          } catch (_) { /* already registered for this page */ }
        }
      }
    }
    d.addEventListener("mousemove", onMove);
    d.addEventListener("keyup", onKeyUp);
    d.documentElement.addEventListener("mouseleave", hidePeek);
    win.addEventListener("scroll", () => { if (P.kind) hidePeek(); }, {passive: true});
    d.addEventListener("click", onClick);
    d.addEventListener("dblclick", onDouble);
    d.addEventListener("mousedown", onDown);
    d.addEventListener("keydown", onKey);
    d.addEventListener("load", later, true);
    win.addEventListener("scroll", later, {passive: true});
    win.addEventListener("resize", onResize);
  }

  // ---- the controls in the preview header ----
  const EXTRAS = ["ink", "echo", "ghosts", "peek", "mirror", "follow", "tasks"];
  /** Bring what is on the page in line with settings that just changed. */
  function settle_opts() {
    const o = S.opts, root = article(), win = frame.contentWindow;
    const reg = win && win.CSS && win.CSS.highlights;
    if (root) for (const box of root.querySelectorAll("input[data-o]")) box.disabled = !(S.live && o.tasks);
    if (reg) for (const slot of S.slots) {
      if (slot.id && slot.name && (slot.name.startsWith("lw-ink") ? !o.ink : !o.echo)) release(slot, reg);
    }
    if (!o.peek && P.kind) hidePeek();
    if (!o.follow) S.followed = -1;
    follow();
  }
  function reflect() {
    const o = S.opts;
    for (const radio of panel.querySelectorAll('input[name="lw-style"]')) radio.checked = radio.value === o.style;
    $("cue-extent").value = o.letters ? "letters" : "word";
    $("cue-dim").value = String(o.dim);
    $("cue-dim-value").textContent = o.dim + "%";
    $("cue-margin").checked = o.margin;
    $("cue-calm").checked = o.calm;
    $("cue-click").checked = o.click;
    for (const key of EXTRAS) $("fx-" + key).checked = o[key];
  }
  function collect() {
    const picked = panel.querySelector('input[name="lw-style"]:checked');
    return {
      style: picked ? picked.value : "pill",
      letters: $("cue-extent").value === "letters",
      margin: $("cue-margin").checked,
      dim: Math.max(0, Math.min(45, Math.round(Number($("cue-dim").value) || 0))),
      calm: $("cue-calm").checked,
      click: $("cue-click").checked,
      ...Object.fromEntries(EXTRAS.map(key => [key, $("fx-" + key).checked])),
    };
  }
  /**
   * Take settings from the host. They are applied only when they differ from
   * the last ones the host sent, so a page that was already on its way
   * cannot undo a choice just made in these controls.
   */
  function adopt(cue) {
    const text = JSON.stringify(cue);
    if (text === S.host) return false;
    S.host = text;
    const style = String(cue.style || "pill");
    S.opts = {
      style: ["pill", "beam", "underline", "block", "off"].includes(style) ? style : "pill",
      letters: !!cue.letters, margin: !!cue.margin,
      dim: Math.max(0, Math.min(45, Math.round(Number(cue.dim) || 0))),
      calm: !!cue.calm, click: cue.click !== false,
      ink: cue.ink !== false, echo: cue.echo !== false, ghosts: cue.ghosts !== false, peek: cue.peek !== false,
      mirror: cue.mirror !== false, follow: !!cue.follow, tasks: cue.tasks !== false,
    };
    reflect();
    settle_opts();
    return true;
  }
  /** Show the newest edit with the settings as they are now. */
  function demo() {
    const t = measure();
    if (t) present(t, LINGER); else stop();
    sync();
  }
  function options(cue) {
    if (cue && adopt(cue)) {
      if (phase() !== "none") demo();
      else sync();
    }
    return true;
  }
  toggle.addEventListener("click", () => openPanel(panel.hidden));
  jump.addEventListener("click", ev => {
    show();
    // After a pointer press, typing continues in the source.
    if (ev.detail > 0) post({t: "focus"});
  });
  document.addEventListener("keydown", onKey);
  document.addEventListener("mousedown", ev => {
    if (!panel.hidden && !panel.contains(ev.target) && ev.target !== toggle) openPanel(false);
  });
  panel.addEventListener("input", () => {
    S.opts = collect();
    $("cue-dim-value").textContent = S.opts.dim + "%";
    settle_opts();
    demo();
  });
  panel.addEventListener("change", () => {
    S.opts = collect();
    post({t: "cue", ...S.opts});
  });
  reflect();

  // ---- one update from the host ----
  function render(p) {
    const d = frame.contentDocument, win = frame.contentWindow;
    if (!d || !d.body || !win) return false;
    const root = d.querySelector("article");
    if (!root) return false;
    S.live = !!p.live;
    if (S.doc !== d) wire(d);
    const light = !!p.light;
    document.documentElement.classList.toggle("light", light);
    d.documentElement.classList.toggle("light", light);
    $("format").textContent = p.kind;
    if (!d.head.firstElementChild || !d.head.firstElementChild.nextElementSibling) {
      d.head.append(d.importNode($("reading").content, true), slotRules(d));
    }
    // Settings first: the page is built with them.
    if (p.cue) adopt(p.cue);
    const out = build(d, p);
    const notice = $("notice");
    const text = ((p.notice || "") + (out.limited ? " Preview limited to the first 20,000 nodes." : "")).trim();
    if (notice.textContent !== text) notice.textContent = text;
    notice.hidden = !text;
    const mapped = !!p.mapped;
    const same = S.view === p.view && S.mapped === mapped;
    const e = mapped && p.edit ? p.edit : null;
    // A deletion is judged on the text the reader was looking at, which is
    // still on the page at this point if no update was skipped.
    if (e && same && e.kind === "type" && !e.add && e.del > 0 && S.rev === e.prev) e.mode = classify(root, e);
    // So is what a deletion took: its text is read before it is gone.
    const lost = e && same && S.opts.ghosts && e.kind === "type" && e.del >= 2 && S.rev === e.prev && !e.quiet
      ? doomed(root, e) : "";
    patch(root, out, same, win);
    if (S.view !== p.view) S.followed = -1;
    S.view = p.view; S.rev = p.rev; S.mapped = mapped;
    toggle.hidden = !(S.live && mapped);
    if (toggle.hidden && !panel.hidden) openPanel(false);
    if (P.kind && (!P.el || !P.el.isConnected)) hidePeek();
    paint(mapped && Array.isArray(p.marks) ? p.marks : [], root, win);
    // An edit plays once. Drawing the same page again (a theme change, a
    // return to its tab) keeps it available without replaying it.
    const known = !!(e && S.edit && e.serial === S.edit.serial);
    const fresh = !!e && e.serial !== S.played && !e.quiet;
    if (known && S.edit.mode && !e.mode) e.mode = S.edit.mode;
    if (e) S.played = e.serial;
    S.edit = e;
    let t = measure();
    // With Follow on, an edit out of view is brought in before its cue plays.
    if (fresh && t && S.opts.follow && (t.box.top < 0 || t.box.bottom > win.innerHeight)) {
      settle(win, t);
      t = measure();
    }
    // Pasted text is marked by its wash, not by the typing cue.
    if (fresh && e.kind === "paste" && S.opts.echo) stop();
    else if (fresh) present(t, e.kind === "type" ? IDLE : LINGER);
    else if (!known) stop();
    if (fresh && lost && e.serial !== S.ghosted) { S.ghosted = e.serial; ghost(root, e, lost, win); }
    follow();
    sync();
    return true;
  }

  window.__lw = {render, options, show, rest, reveal, where, track, peeked};
})();
