/*
 * Terminology explorers (HealthyMind) — the browser half of
 * crates/hts-ui/src/explore.rs. Vanilla JS, no dependencies.
 *
 * One explorer per kind of code system (config.kind):
 *   snomed — lazy is-a tree + focus graph (parents above, children below,
 *            attribute groups and mappings beside, SVG edges between them)
 *   icd    — chapter tiles sized by code count + chapter → block → category
 *            → code tree, English and Chinese titles side by side
 *   loinc  — clusters along the LOINC axes, drill-down by click, term list
 *            with part chips
 *   ndc    — product type → labeler → product → package cluster cards
 *   ucum   — units grouped by dimension
 *   tree   — any hierarchical system: lazy tree
 *   flat   — any flat system: grouped by a shared property, or a list
 * Every explorer shares the search box, the concept side panel (designations
 * with zh-Hant highlighted, properties, parents/children, mappings, copy
 * buttons) and the breadcrumb.
 *
 * Data comes from two JSON routes: config.dataUrl (`$hm-explore`, paged and
 * capped server-side) and config.conceptUrl ($lookup + $translate). Layout
 * and motion ideas (focus graph, EdgeLayer path math, cluster drill-down,
 * row-enter stagger) follow the Taiwan-Health-MCP terminology explorer.
 */
(function () {
  "use strict";

  var root = document.getElementById("tx-explorer");
  var cfgEl = document.getElementById("tx-explorer-config");
  if (!root || !cfgEl) return;
  var cfg;
  try {
    cfg = JSON.parse(cfgEl.textContent);
  } catch (e) {
    return;
  }
  var S = cfg.strings || {};
  var reduced = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var nf;
  try {
    nf = new Intl.NumberFormat(cfg.lang || undefined);
  } catch (e) {
    nf = new Intl.NumberFormat();
  }
  var zhUi = /^zh/i.test(cfg.lang || "");

  function t(key, n) {
    var s = S[key] || key;
    if (n !== undefined) s = s.replace("__N__", fmt(n));
    return s;
  }
  function countLabel(n, capped) {
    return capped ? fmt(Math.max(0, n - 1)) + "+" : fmt(n);
  }
  function fmt(n) {
    return typeof n === "number" ? nf.format(n) : String(n == null ? "" : n);
  }

  /* ---------- DOM helpers ---------- */

  function el(tag, attrs) {
    var node = document.createElement(tag);
    if (attrs) {
      for (var k in attrs) {
        if (!Object.prototype.hasOwnProperty.call(attrs, k)) continue;
        var v = attrs[k];
        if (v === null || v === undefined || v === false) continue;
        if (k === "class") node.className = v;
        else if (k === "text") node.textContent = v;
        else if (k === "style" && typeof v === "object") {
          for (var sk in v) node.style.setProperty(sk, v[sk]);
        } else if (k.slice(0, 2) === "on" && typeof v === "function") {
          node.addEventListener(k.slice(2), v);
        } else node.setAttribute(k, v === true ? "" : v);
      }
    }
    for (var i = 2; i < arguments.length; i++) append(node, arguments[i]);
    return node;
  }
  function append(node, child) {
    if (child === null || child === undefined || child === false) return;
    if (Array.isArray(child)) {
      child.forEach(function (c) { append(node, c); });
    } else if (typeof child === "string" || typeof child === "number") {
      node.appendChild(document.createTextNode(String(child)));
    } else node.appendChild(child);
  }
  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
    return node;
  }
  var SVGNS = "http://www.w3.org/2000/svg";
  function icon(name) {
    var paths = {
      chev: "M9 6l6 6-6 6",
      search: "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14zM21 21l-4.3-4.3",
      back: "M15 6l-6 6 6 6",
      fwd: "M9 6l6 6-6 6",
      home: "M3 11l9-7 9 7M5 10v10h14V10",
      tree: "M4 5h6M4 12h6M4 19h6M14 5h6v4h-6zM14 15h6v4h-6zM10 5v14",
      close: "M6 6l12 12M18 6L6 18",
      copy: "M9 9h10v10H9zM5 15V5h10",
      x: "M7 7l10 10M17 7L7 17"
    };
    var svg = document.createElementNS(SVGNS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    var p = document.createElementNS(SVGNS, "path");
    p.setAttribute("d", paths[name] || "");
    svg.appendChild(p);
    return svg;
  }
  function highlight(text, q) {
    text = text || "";
    if (!q || q.length < 2) return document.createTextNode(text);
    var i = text.toLowerCase().indexOf(q.toLowerCase());
    if (i < 0) return document.createTextNode(text);
    var span = document.createElement("span");
    span.appendChild(document.createTextNode(text.slice(0, i)));
    span.appendChild(el("mark", { class: "tx-hl" }, text.slice(i, i + q.length)));
    span.appendChild(document.createTextNode(text.slice(i + q.length)));
    return span;
  }
  function skeletonRows(n) {
    var out = [];
    for (var i = 0; i < n; i++) {
      out.push(el("li", { class: "tx-row tx-row--skel", "aria-hidden": "true" },
        el("span", { class: "tx-ex-skel", style: { width: (62 - i * 12) + "%" } })));
    }
    return out;
  }
  function errorBox(retry) {
    return el("div", { class: "tx-err", role: "alert" }, t("tx-explore-error"),
      retry ? el("button", { type: "button", onclick: retry }, t("tx-explore-retry")) : null);
  }
  function toast(msg) {
    var old = root.querySelector(".tx-toast");
    if (old) old.remove();
    var n = el("div", { class: "tx-toast", role: "status" }, msg);
    root.appendChild(n);
    setTimeout(function () { n.remove(); }, 1600);
  }
  function copy(text) {
    var done = function () { toast(t("tx-explore-copied")); };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallbackCopy(text); done(); });
    } else {
      fallbackCopy(text);
      done();
    }
  }
  function fallbackCopy(text) {
    var ta = el("textarea", { style: { position: "fixed", opacity: "0" } });
    ta.value = text;
    document.body.appendChild(ta);
    ta.focus();
    ta.select();
    try { document.execCommand("copy"); } catch (e) { /* ignore */ }
    ta.remove();
  }
  function palette(i) {
    return "var(--tx-c" + ((((i % 12) + 12) % 12) + 1) + ")";
  }
  // Greedy circle packing: largest first, each circle at the first free spot
  // on an outward elliptical spiral (wider than tall, `aspect` = width/height).
  function packCircles(circles, aspect) {
    var gap = 6;
    var ax = Math.max(1, Math.min(2.4, aspect || 1.6));
    var placed = [];
    circles.forEach(function (c) {
      if (!placed.length) {
        c.x = 0; c.y = 0;
        placed.push(c);
        return;
      }
      for (var k = 0; k < 6000; k++) {
        var th = k * 0.18;
        var d = 1.4 * th;
        var x = d * Math.cos(th) * ax;
        var y = d * Math.sin(th);
        var ok = true;
        for (var j = 0; j < placed.length; j++) {
          var p = placed[j];
          var dx = p.x - x, dy = p.y - y, m = p.r + c.r + gap;
          if (dx * dx + dy * dy < m * m) { ok = false; break; }
        }
        if (ok) { c.x = x; c.y = y; break; }
      }
      if (c.x === undefined) { c.x = 0; c.y = d + c.r; }
      placed.push(c);
    });
    return circles;
  }
  function hashIndex(s) {
    var h = 0;
    for (var i = 0; i < (s || "").length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
    return Math.abs(h);
  }

  /* ---------- data ---------- */

  var cache = new Map();
  function qs(params) {
    var u = new URLSearchParams();
    Object.keys(params).forEach(function (k) {
      var v = params[k];
      if (v === undefined || v === null || v === "") return;
      if (Array.isArray(v)) v.forEach(function (x) { u.append(k, x); });
      else u.append(k, String(v));
    });
    return u.toString();
  }
  function getJSON(url) {
    if (cache.has(url)) return cache.get(url);
    var p = fetch(url, { headers: { Accept: "application/json" }, credentials: "same-origin" })
      .then(function (r) {
        return r.json().catch(function () { return {}; }).then(function (body) {
          if (!r.ok) {
            var err = new Error((body && body.error) || ("HTTP " + r.status));
            err.status = r.status;
            throw err;
          }
          return body;
        });
      });
    cache.set(url, p);
    p.catch(function () { cache.delete(url); });
    if (cache.size > 400) cache.delete(cache.keys().next().value);
    return p;
  }
  function api(params, system) {
    var base = { system: system || cfg.system };
    if (!system || system === cfg.system) base.version = cfg.version;
    return getJSON(cfg.dataUrl + "?" + qs(Object.assign(base, params)));
  }
  // The ConceptMap elements of a concept, both directions (cached by getJSON).
  function mappingsOf(system, code) {
    return api({ mode: "mappings", code: code, count: 60 }, system).then(function (r) { return r.items || []; });
  }
  function conceptDetail(system, code, version) {
    var p = { system: system, code: code, version: version };
    if (zhUi) p.lang = "zh-Hant";
    return getJSON(cfg.conceptUrl + "?" + qs(p));
  }

  /* ---------- URL state ---------- */

  var url = new URL(window.location.href);
  function setParam(k, v) {
    if (v === undefined || v === null || v === "") url.searchParams.delete(k);
    else url.searchParams.set(k, v);
    try { history.replaceState(history.state, "", url.toString()); } catch (e) { /* ignore */ }
  }
  function getParam(k) {
    return url.searchParams.get(k);
  }

  /* ---------- search box ---------- */

  function SearchBox(opts) {
    var wrap = el("div", { class: "tx-search" });
    var input = el("input", {
      class: "tx-search__input",
      type: "search",
      placeholder: t("tx-explore-search-placeholder"),
      "aria-label": t("tx-explore-search-placeholder"),
      autocomplete: "off",
      role: "combobox",
      "aria-expanded": "false",
      "aria-autocomplete": "list"
    });
    var ic = icon("search");
    ic.setAttribute("class", "tx-search__icon");
    wrap.appendChild(ic);
    wrap.appendChild(input);
    var pop = null;
    var spin = null;
    var items = [];
    var active = -1;
    var timer = null;
    var seq = 0;
    function close() {
      if (pop) pop.remove();
      pop = null;
      input.setAttribute("aria-expanded", "false");
      active = -1;
    }
    function busy(on) {
      if (on && !spin) {
        spin = el("span", { class: "tx-search__spin", "aria-hidden": "true" });
        wrap.appendChild(spin);
      } else if (!on && spin) {
        spin.remove();
        spin = null;
      }
    }
    function show(nodes) {
      if (!pop) {
        pop = el("div", { class: "tx-pop", role: "listbox" });
        wrap.appendChild(pop);
      }
      clear(pop);
      append(pop, nodes);
      input.setAttribute("aria-expanded", "true");
    }
    function render(list, q) {
      items = list;
      active = list.length ? 0 : -1;
      if (!list.length) {
        show(el("div", { class: "tx-pop__msg" }, t("tx-explore-no-results")));
        return;
      }
      show(list.map(function (it, i) {
        var sub = it.alt || it.designation || "";
        return el("button", {
          type: "button",
          class: "tx-pop__item",
          role: "option",
          "aria-selected": i === active ? "true" : "false",
          onmousedown: function (e) { e.preventDefault(); },
          onclick: function () { pick(i); }
        },
        el("span", { class: "tx-pop__code" }, it.code),
        el("span", { class: "tx-pop__label" }, highlight(it.display || it.code, q)),
        sub ? el("span", { class: "tx-pop__sub" }, highlight(sub, q)) : null);
      }));
    }
    function mark() {
      if (!pop) return;
      var opts2 = pop.querySelectorAll(".tx-pop__item");
      for (var i = 0; i < opts2.length; i++) {
        opts2[i].setAttribute("aria-selected", i === active ? "true" : "false");
        if (i === active) opts2[i].scrollIntoView({ block: "nearest" });
      }
    }
    function pick(i) {
      var it = items[i];
      if (!it) return;
      close();
      input.blur();
      opts.onPick(it);
    }
    function run() {
      var q = input.value.trim();
      if (!q) {
        close();
        return;
      }
      var my = ++seq;
      busy(true);
      show(el("div", { class: "tx-pop__msg" }, t("tx-explore-searching")));
      opts.search(q).then(function (list) {
        if (my !== seq) return;
        busy(false);
        render(list || [], q);
      }, function () {
        if (my !== seq) return;
        busy(false);
        show(el("div", { class: "tx-pop__msg" }, t("tx-explore-error")));
      });
    }
    input.addEventListener("input", function () {
      clearTimeout(timer);
      timer = setTimeout(run, 250);
    });
    input.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown") {
        if (!pop) run();
        active = Math.min(items.length - 1, active + 1);
        mark();
        e.preventDefault();
      } else if (e.key === "ArrowUp") {
        active = Math.max(0, active - 1);
        mark();
        e.preventDefault();
      } else if (e.key === "Enter") {
        if (pop && active >= 0) {
          pick(active);
          e.preventDefault();
        } else run();
      } else if (e.key === "Escape") {
        close();
      }
    });
    input.addEventListener("blur", function () { setTimeout(close, 120); });
    return wrap;
  }

  function defaultSearch(extra, system) {
    return function (q) {
      return api(Object.assign({ mode: "search", q: q, count: 20 }, extra || {}), system)
        .then(function (r) { return r.items || []; });
    };
  }

  /* ---------- side panel ---------- */

  function Panel(shell, opts) {
    var card = null;
    var token = 0;
    function close() {
      if (card) card.remove();
      card = null;
      shell.classList.remove("has-panel");
      if (opts.onClose) opts.onClose();
    }
    function frame() {
      if (!card) {
        card = el("aside", { class: "tx-card tx-panel", "aria-label": t("tx-explore-details") });
        shell.appendChild(card);
        shell.classList.add("has-panel");
      }
      return card;
    }
    function relList(list, system) {
      return el("div", { class: "tx-rel" }, list.map(function (r) {
        return el("button", {
          type: "button",
          onclick: function () { if (opts.onNavigate) opts.onNavigate(r.code, r.display, system); }
        }, el("code", null, r.code), el("span", null, r.display || ""));
      }));
    }
    function section(title, body) {
      return el("section", null, el("h4", { class: "tx-sec__title" }, title), body);
    }
    function show(c) {
      // c: {system, code, display, version, fallbackSystem}
      var my = ++token;
      var box = frame();
      clear(box);
      var head = el("div", { class: "tx-card__head" }, t("tx-explore-details"),
        el("button", {
          type: "button",
          class: "tx-iconbtn tx-panel__close",
          "aria-label": t("tx-explore-close"),
          onclick: close
        }, icon("close")));
      var body = el("div", { class: "tx-panel__body" },
        el("div", { class: "tx-panel__code" }, c.code),
        c.display ? el("p", { class: "tx-panel__display" }, c.display) : null,
        el("span", { class: "tx-ex-skel", style: { width: "70%" } }),
        el("span", { class: "tx-ex-skel", style: { width: "45%" } }));
      append(box, [head, body]);
      var load = function (system) {
        return conceptDetail(system, c.code, system === cfg.system ? cfg.version : undefined);
      };
      var p = load(c.system || cfg.system);
      if (c.fallbackSystem) {
        p = p.catch(function () { return load(c.fallbackSystem); });
      }
      p.then(function (d) {
        if (my !== token || !card) return;
        renderDetail(body, d, c);
      }, function () {
        if (my !== token || !card) return;
        clear(body);
        append(body, [el("div", { class: "tx-panel__code" }, c.code),
          c.display ? el("p", { class: "tx-panel__display" }, c.display) : null,
          errorBox(function () { show(c); })]);
      });
      if (window.matchMedia("(max-width: 1100px)").matches) {
        box.scrollTop = 0;
      }
    }
    function renderDetail(body, d, c) {
      clear(body);
      var display = d.display || c.display || "";
      var designations = d.designations || [];
      var zh = designations.filter(function (x) { return /^zh/i.test(x.language || ""); });
      // Traditional Chinese (zh-Hant, zh-TW) before other Chinese variants.
      zh.sort(function (a, b) { return zhRank(a.language) - zhRank(b.language); });
      var zhText = zh.length ? zh[0].value : (c.alt && /[\u3400-\u9fff]/.test(c.alt) ? c.alt : "");
      if (zhText === display) zhText = "";
      var coding = { system: d.system || c.system, code: d.code || c.code };
      if (d.version) coding.version = d.version;
      if (display) coding.display = display;
      var chips = [];
      if (d.inactive === true) chips.push(el("span", { class: "tx-badge", style: { "--c": "var(--tx-c11)" } }, t("tx-explore-inactive")));
      if (d.inactive === false) chips.push(el("span", { class: "tx-badge", style: { "--c": "var(--tx-c3)" } }, t("tx-explore-active")));
      if (d.abstract) chips.push(el("span", { class: "tx-badge", style: { "--c": "var(--tx-c8)" } }, t("tx-explore-abstract")));
      append(body, [
        el("div", null,
          el("div", { class: "tx-panel__code" }, d.code || c.code),
          chips.length ? el("div", { class: "tx-focus__badges", style: { "margin-top": "6px" } }, chips) : null),
        display ? el("p", { class: "tx-panel__display" }, display) : null,
        zhText ? el("p", { class: "tx-zh-line", lang: "zh-Hant" }, zhText) : null,
        el("div", { class: "tx-actions" },
          el("button", { type: "button", class: "btn btn--sm", onclick: function () { copy(coding.code); } }, t("tx-explore-copy-code")),
          el("button", { type: "button", class: "btn btn--sm", onclick: function () { copy(JSON.stringify(coding, null, 2)); } }, t("tx-explore-copy-coding")),
          d.permalink ? el("a", { class: "btn btn--sm", href: d.permalink }, t("tx-explore-open-concept")) : null)
      ]);
      if (d.definition) body.appendChild(section(t("tx-explore-definition"), el("p", { class: "tx-def" }, d.definition)));
      if (d.parents && d.parents.length) body.appendChild(section(t("tx-explore-parents"), relList(d.parents, d.system)));
      if (d.children && d.children.length) {
        var more = d.childCount > d.children.length ? " (" + fmt(d.childCount) + ")" : "";
        body.appendChild(section(t("tx-explore-children") + more, relList(d.children, d.system)));
      }
      if (designations.length) {
        var sorted = zh.concat(designations.filter(function (x) { return zh.indexOf(x) < 0; }));
        body.appendChild(section(t("tx-explore-designations"), el("ul", { class: "tx-dlist" }, sorted.map(function (x) {
          return el("li", { class: /^zh/i.test(x.language || "") ? "is-zh" : "" },
            el("span", { class: "tx-lang" }, x.language || "—"),
            el("span", null, el("span", { class: "tx-dlist__v", lang: x.language || null }, x.value),
              x.use ? el("span", { class: "tx-dlist__use" }, USE_NAMES[x.use] || x.use) : null));
        }))));
      }
      var props = (d.properties || []).filter(function (p) {
        return p.code !== "parent" && p.code !== "child" && !(opts.hideProperty && opts.hideProperty(p));
      });
      if (props.length) {
        body.appendChild(section(t("tx-explore-properties"), el("div", { class: "tx-props" }, props.map(function (p) {
          return el("span", { class: "tx-prop" }, el("b", null, p.code), el("span", null, p.value));
        }))));
      }
      // Mappings load after the identity (ConceptMap element index).
      var mapBox = el("div", { class: "tx-maps", "aria-busy": "true" },
        el("span", { class: "tx-ex-skel", style: { width: "60%" } }));
      body.appendChild(section(t("tx-explore-mappings"), mapBox));
      var mySys = d.system || c.system || cfg.system;
      var myToken = token;
      mappingsOf(mySys, d.code || c.code).then(function (maps) {
        if (myToken !== token) return;
        mapBox.removeAttribute("aria-busy");
        clear(mapBox);
        if (!maps.length) {
          mapBox.appendChild(el("p", { class: "tx-def", style: { color: "var(--muted)" } }, t("tx-explore-mappings-none")));
          return;
        }
        maps.forEach(function (m) {
          mapBox.appendChild(el("div", { class: "tx-map", "data-dir": m.direction },
            el("span", { class: "tx-map__rel" }, (m.direction === "reverse" ? t("tx-explore-mapped-from") : t("tx-explore-maps-to")) +
              (m.relationship ? " · " + m.relationship : "")),
            el("span", null, el("span", { class: "tx-map__code" }, m.code), " ", m.display || ""),
            el("span", { class: "tx-map__sys" }, m.system + (m.map ? " — " + (m.mapTitle || m.map) : ""))));
        });
      }, function () {
        if (myToken !== token) return;
        mapBox.removeAttribute("aria-busy");
        clear(mapBox);
        mapBox.appendChild(el("p", { class: "tx-def", style: { color: "var(--muted)" } }, t("tx-explore-error")));
      });
      if (opts.onDetail) opts.onDetail(d);
    }
    return { show: show, close: close, isOpen: function () { return !!card; } };
  }

  function zhRank(lang) {
    lang = (lang || "").toLowerCase();
    if (/hant|tw|hk/.test(lang)) return 0;
    if (lang === "zh") return 1;
    return 2;
  }
  var USE_NAMES = {
    "900000000000013009": "Synonym",
    "900000000000003001": "Fully specified name",
    "900000000000550004": "Definition",
    "900000000000548007": "Preferred"
  };

  /* ---------- lazy tree ---------- */

  function Tree(opts) {
    // opts: load(code|null, offset) -> Promise<{items,total}>, row(item) -> {code,label,alt,altMissing,count,dot}
    //       onSelect(item), rootCode, bilingual
    var list = el("ul", { class: "tx-tree" + (opts.bilingual ? " tx-tree--bilingual" : ""), role: "tree", tabindex: "0" });
    var selected = null;
    var focused = null;
    var PAGE = opts.page || 100;

    function makeNode(item, depth, delay) {
      var r = opts.row(item);
      var hasKids = (item.children || 0) > 0;
      var li = el("li", { class: "tx-node", role: "none", "data-code": item.code });
      var chev = el("button", {
        type: "button",
        class: "tx-row__chev",
        tabindex: "-1",
        "aria-hidden": "true",
        "data-leaf": hasKids ? null : "1",
        onclick: function (e) { e.stopPropagation(); toggle(li); }
      }, icon("chev"));
      var text = el("span", { class: "tx-row__text" },
        el("span", { class: "tx-row__label", title: r.label || null }, r.label),
        r.alt ? el("span", { class: "tx-row__alt" + (r.altPlain ? " tx-row__alt--plain" : ""), lang: r.altPlain ? null : "zh-Hant" }, r.alt)
          : r.altMissing ? el("span", { class: "tx-row__alt tx-row__alt--missing" }, r.altMissing) : null);
      var row = el("div", {
        class: "tx-row",
        role: "treeitem",
        tabindex: "-1",
        title: opts.hideCode ? r.code + " — " + (item.display || "") : null,
        "aria-level": String(depth + 1),
        "aria-expanded": hasKids ? "false" : null,
        "aria-selected": "false",
        style: { "--d": delay + "ms" },
        onclick: function () { select(li, true); },
        ondblclick: function () { if (hasKids) toggle(li); }
      }, chev,
      r.dot ? el("span", { class: "tx-row__dot", style: { "--c": r.dot }, title: r.dotTitle || null }) : null,
      opts.hideCode ? null : el("span", { class: "tx-row__code" }, r.code),
      text,
      hasKids && r.count !== false ? el("span", { class: "tx-row__count", title: t("tx-explore-children") }, fmt(r.count != null ? r.count : item.children)) : null);
      li.appendChild(row);
      li._item = item;
      li._depth = depth;
      li._row = row;
      if (hasKids) {
        var ul = el("ul", { role: "group" });
        li.appendChild(el("div", { class: "tx-tree__wrap" }, ul));
        li._group = ul;
      }
      return li;
    }

    function fill(container, parentCode, depth, offset) {
      var skel = skeletonRows(offset ? 1 : 3);
      append(container, skel);
      return opts.load(parentCode, offset).then(function (res) {
        skel.forEach(function (s) { s.remove(); });
        var items = res.items || [];
        items.forEach(function (it, i) {
          if (container.querySelector(':scope > li[data-code="' + cssEsc(it.code) + '"]')) return;
          container.appendChild(makeNode(it, depth, reduced ? 0 : Math.min(i, 14) * 16));
        });
        var shown = offset + items.length;
        var total = res.total || shown;
        if (shown < total) {
          var more = el("li", { role: "none" }, el("button", {
            type: "button",
            class: "tx-more",
            onclick: function () { more.remove(); fill(container, parentCode, depth, shown); }
          }, t("tx-explore-load-more") + " · " + t("tx-explore-more-remaining", total - shown)));
          container.appendChild(more);
        }
        if (res.note) container.appendChild(el("li", { role: "none" }, el("div", { class: "tx-note" }, res.note)));
        return res;
      }, function () {
        skel.forEach(function (s) { s.remove(); });
        var box = el("li", { role: "none" }, errorBox(function () { box.remove(); fill(container, parentCode, depth, offset); }));
        container.appendChild(box);
      });
    }

    function expand(li) {
      if (!li._group) return Promise.resolve();
      li.classList.add("is-open");
      li._row.setAttribute("aria-expanded", "true");
      if (!li._loaded) {
        li._loaded = fill(li._group, li._item.code, li._depth + 1, 0);
      }
      return li._loaded;
    }
    function collapse(li) {
      li.classList.remove("is-open");
      if (li._row) li._row.setAttribute("aria-expanded", "false");
    }
    function toggle(li) {
      if (li.classList.contains("is-open")) collapse(li);
      else expand(li);
    }
    function focusRow(li) {
      if (focused && focused._row) {
        focused._row.classList.remove("is-focus");
        focused._row.setAttribute("tabindex", "-1");
      }
      focused = li;
      if (li && li._row) {
        li._row.setAttribute("tabindex", "0");
        li._row.classList.add("is-focus");
        li._row.focus({ preventScroll: true });
      }
    }
    function focusKey(li) {
      focusRow(li);
      if (li && li._row) li._row.scrollIntoView({ block: "nearest" });
    }
    function select(li, user) {
      if (selected && selected._row) selected._row.setAttribute("aria-selected", "false");
      selected = li;
      li._row.setAttribute("aria-selected", "true");
      focusRow(li);
      if (user && opts.onSelect) opts.onSelect(li._item, li);
    }
    function visibleRows() {
      return Array.prototype.filter.call(list.querySelectorAll("li.tx-node"), function (li) {
        var p = li.parentElement;
        while (p && p !== list) {
          if (p.classList.contains("tx-node") && !p.classList.contains("is-open")) return false;
          p = p.parentElement;
        }
        return true;
      });
    }
    list.addEventListener("keydown", function (e) {
      var rows = visibleRows();
      if (!rows.length) return;
      var i = rows.indexOf(focused);
      if (i < 0) i = 0;
      var cur = rows[i];
      if (e.key === "ArrowDown") focusKey(rows[Math.min(rows.length - 1, i + 1)]);
      else if (e.key === "ArrowUp") focusKey(rows[Math.max(0, i - 1)]);
      else if (e.key === "Home") focusKey(rows[0]);
      else if (e.key === "End") focusKey(rows[rows.length - 1]);
      else if (e.key === "ArrowRight") {
        if (cur._group && !cur.classList.contains("is-open")) expand(cur);
        else if (cur._group) {
          var first = cur._group.querySelector(":scope > li.tx-node");
          if (first) focusKey(first);
        }
      } else if (e.key === "ArrowLeft") {
        if (cur.classList.contains("is-open")) collapse(cur);
        else {
          var parent = cur.parentElement && cur.parentElement.closest("li.tx-node");
          if (parent) focusKey(parent);
        }
      } else if (e.key === "Enter" || e.key === " ") select(cur, true);
      else return;
      e.preventDefault();
    });
    list.addEventListener("focus", function () {
      if (!focused) {
        var first = list.querySelector("li.tx-node");
        if (first) focusRow(first);
      }
    });

    function start() {
      clear(list);
      selected = null;
      focused = null;
      return fill(list, opts.rootCode || null, 0, 0).then(function (res) {
        if (opts.autoExpand) {
          var tops = Array.prototype.slice.call(list.querySelectorAll(":scope > li.tx-node"));
          if (tops.length && tops.length <= 3) {
            return Promise.all(tops.filter(function (li) { return li._group; }).map(expand)).then(function () { return res; });
          }
        }
        return res;
      });
    }

    // Reveal a path of {code, display} (root first): open each level, adding
    // the next node when it is not on the loaded page.
    function reveal(path) {
      var container = list;
      var depth = 0;
      var idx = 0;
      // Skip path entries above the tree's own root.
      if (opts.rootCode) {
        var at = path.findIndex(function (p) { return p.code === opts.rootCode; });
        if (at >= 0) idx = at + 1;
      }
      function step() {
        if (idx >= path.length) return Promise.resolve(null);
        var p = path[idx];
        var li = container.querySelector(':scope > li[data-code="' + cssEsc(p.code) + '"]');
        if (!li) {
          var isLast = idx === path.length - 1;
          var item = Object.assign({ children: isLast ? (p.children || 0) : 1 }, p);
          li = makeNode(item, depth, 0);
          li.classList.add("tx-node--inserted");
          container.insertBefore(li, container.firstChild);
        }
        if (idx < path.length - 1) li._row.classList.add("is-onpath");
        if (idx === path.length - 1) {
          select(li, false);
          scrollWithin(li._row);
          return Promise.resolve(li);
        }
        return expand(li).then(function () {
          container = li._group;
          depth += 1;
          idx += 1;
          return step();
        });
      }
      Array.prototype.forEach.call(list.querySelectorAll(".is-onpath"), function (r) { r.classList.remove("is-onpath"); });
      var ready = list.querySelector("li.tx-node") ? Promise.resolve() : (list._starting || Promise.resolve());
      return ready.then(step);
    }
    function selectCode(code) {
      var li = list.querySelector('li.tx-node[data-code="' + cssEsc(code) + '"]');
      if (li) select(li, false);
    }
    return { el: list, start: function () { list._starting = start(); return list._starting; }, reveal: reveal, selectCode: selectCode };
  }
  // Centre `row` inside its scrolling card without moving the page.
  function scrollWithin(row) {
    var box = row.closest(".tx-card__body");
    if (!box || box.scrollHeight <= box.clientHeight + 2) {
      var r = row.getBoundingClientRect();
      if (r.top < 0 || r.bottom > window.innerHeight) row.scrollIntoView({ block: "center", behavior: reduced ? "auto" : "smooth" });
      return;
    }
    var rb = box.getBoundingClientRect();
    var rr = row.getBoundingClientRect();
    var top = box.scrollTop + (rr.top - rb.top) - box.clientHeight / 2 + rr.height / 2;
    // Sideways only when the row starts past the middle of the box.
    var left = rr.left - rb.left > box.clientWidth / 2 ? box.scrollLeft + (rr.left - rb.left) - 24 : box.scrollLeft;
    box.scrollTo({ top: Math.max(0, top), left: Math.max(0, left), behavior: reduced ? "auto" : "smooth" });
  }
  function isWide() {
    return !window.matchMedia("(max-width: 1100px)").matches;
  }
  function cssEsc(s) {
    return window.CSS && CSS.escape ? CSS.escape(s) : String(s).replace(/["\\]/g, "\\$&");
  }

  function crumbs(container, path, onPick) {
    clear(container);
    path.forEach(function (p, i) {
      if (i) container.appendChild(el("span", { class: "tx-crumbs__sep", "aria-hidden": "true" }, "›"));
      var last = i === path.length - 1;
      container.appendChild(el("button", {
        type: "button",
        class: "tx-crumbs__item",
        title: p.code + (p.display ? " — " + p.display : ""),
        "aria-current": last ? "true" : null,
        style: { "animation-delay": (reduced ? 0 : i * 40) + "ms" },
        onclick: last ? null : function () { onPick(p, i); }
      }, p.label || p.display || p.code));
    });
  }

  function card(title, meta, body, extraClass) {
    var metaEl = el("span", { class: "tx-card__meta" }, meta || "");
    var c = el("section", { class: "tx-card" + (extraClass ? " " + extraClass : "") },
      el("div", { class: "tx-card__head" }, title, metaEl),
      el("div", { class: "tx-card__body" }, body));
    c._meta = metaEl;
    c._body = c.lastChild;
    return c;
  }

  /* ====================================================================
   * SNOMED CT — tree + focus graph
   * ================================================================== */

  var TAGS = [
    ["disorder", 11], ["finding", 4], ["body structure", 3], ["procedure", 1],
    ["regime/therapy", 6], ["organism", 7], ["substance", 10], ["product", 5],
    ["observable entity", 9], ["qualifier value", 12], ["morphologic abnormality", 8],
    ["situation", 7], ["event", 6], ["medicinal product", 5], ["clinical drug", 5],
    ["specimen", 9], ["physical object", 2], ["attribute", 2], ["environment", 3],
    ["record artifact", 2], ["occupation", 2], ["person", 2]
  ];
  function tagColor(tag) {
    if (!tag) return "var(--accent)";
    for (var i = 0; i < TAGS.length; i++) if (TAGS[i][0] === tag) return "var(--tx-c" + TAGS[i][1] + ")";
    return palette(hashIndex(tag));
  }

  function snomed() {
    var ROOT = "138875005";
    var history = [];
    var hpos = -1;
    var current = null;
    var bar = el("div", { class: "tx-ex-bar" });
    var crumbEl = el("nav", { class: "tx-crumbs", "aria-label": t("tx-explore-path") });
    var shell = el("div", { class: "tx-shell tx-shell--sn" });
    var graph = el("div", { class: "tx-graph" });
    var graphCard = card(t("tx-explore-focus"), "", graph);
    graphCard._body.style.padding = "0";
    var panel = Panel(shell, { onNavigate: function (code) { focus(code, true); }, hideProperty: function (p) { return /^\d+$/.test(p.code); } });
    var tree = Tree({
      load: function (code, offset) {
        return api({ mode: "children", code: code, count: 100, offset: offset });
      },
      row: function (it) {
        return { code: it.code, label: it.display, dot: tagColor(it.tag), dotTitle: it.tag, count: it.children };
      },
      onSelect: function (it) { focus(it.code, true); },
      autoExpand: true,
      hideCode: true
    });
    var treeCard = card(t("tx-explore-hierarchy"), "", tree.el, "tx-card--tree");
    shell.appendChild(treeCard);
    shell.appendChild(el("div", { class: "tx-main" }, graphCard));

    var back = el("button", { type: "button", class: "tx-iconbtn", "aria-label": t("tx-explore-back"), title: t("tx-explore-back"), disabled: true, onclick: function () { go(-1); } }, icon("back"));
    var fwd = el("button", { type: "button", class: "tx-iconbtn", "aria-label": t("tx-explore-forward"), title: t("tx-explore-forward"), disabled: true, onclick: function () { go(1); } }, icon("fwd"));
    var home = el("button", { type: "button", class: "tx-iconbtn", "aria-label": t("tx-explore-home"), title: t("tx-explore-home"), onclick: function () { focus(ROOT, true); } }, icon("home"));
    var treeBtn = el("button", { type: "button", class: "tx-iconbtn", "aria-label": t("tx-explore-hierarchy"), title: t("tx-explore-hierarchy"), "aria-pressed": "true", onclick: function () {
      var wide = !window.matchMedia("(max-width: 1100px)").matches;
      if (wide) {
        shell.classList.toggle("no-tree");
        treeBtn.setAttribute("aria-pressed", shell.classList.contains("no-tree") ? "false" : "true");
      } else {
        shell.classList.toggle("show-tree");
        treeBtn.setAttribute("aria-pressed", shell.classList.contains("show-tree") ? "true" : "false");
      }
    } }, icon("tree"));
    append(bar, [SearchBox({
      search: defaultSearch(),
      onPick: function (it) { focus(it.code, true, true); }
    }), el("span", { class: "tx-ex-bar__spacer" }), treeBtn, back, fwd, home]);
    append(root, [bar, crumbEl, shell]);

    function go(d) {
      var n = hpos + d;
      if (n < 0 || n >= history.length) return;
      hpos = n;
      focus(history[n], false);
    }
    function updateNav() {
      back.disabled = hpos <= 0;
      fwd.disabled = hpos >= history.length - 1;
    }

    function gnode(it, opts) {
      opts = opts || {};
      var b = el("button", {
        type: "button",
        class: "tx-gnode",
        "data-code": it.code,
        "data-inactive": it.active === false ? "1" : null,
        style: { "--accent-c": opts.color || tagColor(it.tag), "--d": (opts.delay || 0) + "ms" },
        title: it.code + " — " + (it.display || ""),
        onclick: opts.onclick || function () { focus(it.code, true, false, b); }
      },
      opts.label ? el("span", { class: "tx-gnode__label" }, opts.label) : null,
      el("span", { class: "tx-gnode__term" }, it.display || it.code),
      el("span", { class: "tx-gnode__meta" },
        el("code", null, it.code),
        it.tag ? el("span", null, it.tag) : null,
        it.children ? el("span", null, "▾ " + fmt(it.children)) : null,
        it.active === false ? el("span", null, t("tx-explore-inactive")) : null));
      return b;
    }

    var edgesSvg = null;
    var edgeSpecs = [];
    function drawEdges() {
      if (!edgesSvg) return;
      var box = graph.getBoundingClientRect();
      while (edgesSvg.firstChild) edgesSvg.removeChild(edgesSvg.firstChild);
      edgesSvg.setAttribute("width", box.width);
      edgesSvg.setAttribute("height", box.height);
      var stacked = box.width < 860;
      var topOf = {};
      var rects = edgeSpecs.map(function (e) {
        var a = (stacked && e.fromStacked ? e.fromStacked : e.from).getBoundingClientRect();
        var b = e.to.getBoundingClientRect();
        var ra = { x: a.left - box.left, y: a.top - box.top, w: a.width, h: a.height };
        var rb = { x: b.left - box.left, y: b.top - box.top, w: b.width, h: b.height };
        if (e.group) topOf[e.group] = Math.min(topOf[e.group] === undefined ? Infinity : topOf[e.group], rb.y);
        return [ra, rb];
      });
      edgeSpecs.forEach(function (e, i) {
        var a = rects[i][0];
        var b = rects[i][1];
        if (!a.w || !b.w) return;
        if (e.group && b.y > topOf[e.group] + 4) return;
        var shape = stacked ? "down" : e.shape;
        var d;
        if (shape === "down") {
          var x1 = a.x + a.w / 2;
          var y1 = a.y + a.h;
          var x2 = b.x + b.w / 2;
          var y2 = b.y;
          if (y2 < y1) {
            // target above source (stacked side nodes): connect top to bottom
            y1 = a.y;
            y2 = b.y + b.h;
          }
          var my = (y1 + y2) / 2;
          d = "M" + x1 + "," + y1 + " C" + x1 + "," + my + " " + x2 + "," + my + " " + x2 + "," + y2;
        } else {
          var right = b.x > a.x + a.w / 2;
          var sx = right ? a.x + a.w : a.x;
          var sy = a.y + a.h / 2;
          var tx = right ? b.x : b.x + b.w;
          var ty = b.y + b.h / 2;
          var mx = (sx + tx) / 2;
          d = "M" + sx + "," + sy + " C" + mx + "," + sy + " " + mx + "," + ty + " " + tx + "," + ty;
        }
        var len = Math.hypot(b.x - a.x, b.y - a.y) * 1.4 + 40;
        var path = document.createElementNS(SVGNS, "path");
        path.setAttribute("d", d);
        path.setAttribute("class", "tx-edge");
        path.setAttribute("data-kind", e.kind || "isa");
        path.style.setProperty("--len", Math.round(len));
        path.style.setProperty("--d", (reduced ? 0 : Math.min(i, 20) * 25) + "ms");
        edgesSvg.appendChild(path);
        if (e.flow && !reduced) {
          var f = document.createElementNS(SVGNS, "path");
          f.setAttribute("d", d);
          f.setAttribute("class", "tx-edge-flow");
          edgesSvg.appendChild(f);
        }
      });
    }
    var redrawTimer = null;
    function scheduleEdges(delay) {
      clearTimeout(redrawTimer);
      redrawTimer = setTimeout(function () { requestAnimationFrame(drawEdges); }, delay || 0);
    }
    if (window.ResizeObserver) {
      new ResizeObserver(function () { scheduleEdges(30); }).observe(graph);
    }

    var focusToken = 0;
    function focus(code, push, fromSearch, fromEl) {
      var my = ++focusToken;
      if (push) {
        history = history.slice(0, hpos + 1);
        if (history[history.length - 1] !== code) history.push(code);
        hpos = history.length - 1;
      }
      updateNav();
      setParam("code", code === ROOT ? null : code);
      graph.classList.add("tx-loading-dim");
      api({ mode: "node", code: code, count: 24 }).then(function (n) {
        if (my !== focusToken) return;
        graph.classList.remove("tx-loading-dim");
        var paint = function () { renderFocus(n); };
        if (document.startViewTransition && !reduced && fromEl) {
          var oldFocus = graph.querySelector(".tx-focus");
          if (oldFocus) oldFocus.style.viewTransitionName = "none";
          fromEl.style.viewTransitionName = "tx-focus";
          document.startViewTransition(function () { paint(); });
        } else paint();
        if (fromSearch || !current) tree.reveal(n.path || []);
        else tree.selectCode(code);
        current = n;
        if (panel.isOpen() || window.matchMedia("(min-width: 1680px)").matches) {
          panel.show({ system: cfg.system, code: n.code, display: n.display });
        }
      }, function () {
        if (my !== focusToken) return;
        graph.classList.remove("tx-loading-dim");
        clear(graph);
        graph.appendChild(errorBox(function () { focus(code, false); }));
      });
    }

    function renderFocus(n) {
      clear(graph);
      edgeSpecs = [];
      edgesSvg = document.createElementNS(SVGNS, "svg");
      edgesSvg.setAttribute("class", "tx-edges");
      edgesSvg.setAttribute("aria-hidden", "true");
      graph.appendChild(edgesSvg);
      crumbs(crumbEl, (n.path || []).map(function (p) { return { code: p.code, display: p.display }; }), function (p) { focus(p.code, true); });

      var accent = tagColor(n.tag);
      var focusCard = el("div", {
        class: "tx-focus",
        "data-inactive": n.active === false ? "1" : null,
        style: { "--accent-c": accent, "view-transition-name": reduced ? null : "tx-focus" }
      },
      el("span", { class: "tx-pulse", "aria-hidden": "true" }),
      el("div", { class: "tx-focus__badges" },
        n.tag ? el("span", { class: "tx-badge", style: { "--c": accent } }, n.tag) : null,
        el("span", { class: "tx-badge", style: { "--c": n.active === false ? "var(--tx-c11)" : "var(--tx-c3)" } }, n.active === false ? t("tx-explore-inactive") : t("tx-explore-active")),
        el("span", { class: "tx-badge", title: n.primitive ? t("tx-explore-primitive") : t("tx-explore-defined") }, n.primitive ? "⊑ " + t("tx-explore-primitive") : "≡ " + t("tx-explore-defined"))),
      el("h2", { class: "tx-focus__term" }, n.display || n.code),
      n.fsn && n.fsn !== n.display ? el("p", { class: "tx-focus__fsn" }, n.fsn) : null,
      el("div", { class: "tx-focus__foot" },
        el("span", { class: "tx-focus__code" }, n.code),
        el("button", { type: "button", class: "btn btn--sm", onclick: function () { copy(n.code); } }, t("tx-explore-copy-code")),
        el("button", { type: "button", class: "btn btn--sm", onclick: function () { panel.show({ system: cfg.system, code: n.code, display: n.display }); } }, t("tx-explore-details"))));

      // Parents
      var parents = n.parents || [];
      if (parents.length) {
        graph.appendChild(el("div", { class: "tx-tier-label" }, t("tx-explore-parents") + " · IS-A (" + fmt(parents.length) + ")"));
        var ptier = el("div", { class: "tx-tier" });
        parents.slice(0, 10).forEach(function (p, i) {
          var g = gnode(p, { delay: reduced ? 0 : i * 50 });
          ptier.appendChild(g);
          edgeSpecs.push({ from: g, to: focusCard, shape: "down", kind: "isa" });
        });
        graph.appendChild(ptier);
      }

      // Focus row: attribute groups on the right, mappings on the left.
      var groups = {};
      (n.attributes || []).forEach(function (a) {
        (groups[a.group] = groups[a.group] || []).push(a);
      });
      var left = el("div", { class: "tx-side" });
      var right = el("div", { class: "tx-side tx-side--right" });
      Object.keys(groups).sort(function (a, b) { return a - b; }).forEach(function (g, gi) {
        var box = el("div", { class: "tx-attrgroup", style: { "--d": (reduced ? 0 : gi * 60) + "ms" } },
          el("div", { class: "tx-attrgroup__title" }, g === "0" ? t("tx-explore-attributes") : t("tx-explore-group") + " " + g));
        groups[g].forEach(function (a) {
          if (a.concrete) {
            box.appendChild(el("div", { class: "tx-gnode", style: { "--accent-c": "var(--tx-c2)", cursor: "default" } },
              el("span", { class: "tx-gnode__label" }, a.typeDisplay || a.type),
              el("span", { class: "tx-gnode__term" }, a.value)));
          } else {
            box.appendChild(gnode({ code: a.value, display: a.valueDisplay, tag: a.valueTag }, { label: a.typeDisplay || a.type }));
          }
        });
        right.appendChild(box);
        edgeSpecs.push({ from: focusCard, to: box, shape: "side", kind: "attr" });
      });
      var row = el("div", { class: "tx-focusrow" }, left, focusCard, right);
      graph.appendChild(row);
      // Mappings (from the concept detail) arrive later.
      mappingsOf(cfg.system, n.code).then(function (all) {
        if (!current || current.code !== n.code) return;
        var maps = all.slice(0, 6);
        maps.forEach(function (m, i) {
          var g = el("div", { class: "tx-gnode", style: { "--accent-c": "var(--tx-c3)", "--d": (reduced ? 0 : i * 60) + "ms", cursor: "default" }, title: m.system },
            el("span", { class: "tx-gnode__label", style: { color: "var(--tx-c3)" } }, (m.direction === "reverse" ? t("tx-explore-mapped-from") : t("tx-explore-maps-to"))),
            el("span", { class: "tx-gnode__term" }, (m.display || m.code)),
            el("span", { class: "tx-gnode__meta" }, el("code", null, m.code), el("span", null, shortSystem(m.system))));
          left.appendChild(g);
          edgeSpecs.push({ from: focusCard, to: g, shape: "side", kind: "map", flow: true });
        });
        if (maps.length) scheduleEdges(60);
      }, function () { /* the panel shows the error */ });

      // Children
      var kids = n.children || { total: 0, items: [] };
      if (kids.total) {
        graph.appendChild(el("div", { class: "tx-tier-label" }, t("tx-explore-children") + " (" + fmt(kids.total) + ")"));
        var ctier = el("div", { class: "tx-tier" });
        var addKids = function (items, start) {
          items.forEach(function (c, i) {
            var g = gnode(c, { delay: reduced ? 0 : Math.min(i, 24) * 22 });
            ctier.insertBefore(g, moreBtn && moreBtn.parentNode ? moreBtn : null);
            edgeSpecs.push({ from: focusCard, fromStacked: row, to: g, shape: "down", kind: "isa", group: "children" });
          });
          var shown = start + items.length;
          if (shown < kids.total) {
            if (!moreBtn) {
              moreBtn = el("button", { type: "button", class: "tx-more", style: { margin: "0" } });
            }
            moreBtn.textContent = t("tx-explore-load-more") + " · " + t("tx-explore-more-remaining", kids.total - shown);
            moreBtn.onclick = function () {
              moreBtn.disabled = true;
              api({ mode: "children", code: n.code, count: 48, offset: shown }).then(function (r) {
                moreBtn.disabled = false;
                addKids(r.items || [], shown);
                scheduleEdges(40);
              }, function () { moreBtn.disabled = false; });
            };
            ctier.appendChild(moreBtn);
          } else if (moreBtn) moreBtn.remove();
        };
        var moreBtn = null;
        addKids(kids.items || [], 0);
        graph.appendChild(ctier);
      }

      // Legend of the semantic tags on screen.
      var seen = {};
      [n].concat(parents, kids.items || []).forEach(function (x) { if (x.tag) seen[x.tag] = 1; });
      var tags = Object.keys(seen);
      if (tags.length) {
        graph.appendChild(el("div", { class: "tx-legend" }, tags.map(function (tg) {
          return el("span", null, el("i", { style: { "--c": tagColor(tg) } }), tg);
        })));
      }
      scheduleEdges(10);
      if (!reduced) scheduleEdges(460);
    }

    tree.start();
    var start = getParam("code") || ROOT;
    focus(start, true, true);
  }

  function shortSystem(s) {
    s = s || "";
    var known = {
      "http://snomed.info/sct": "SNOMED CT",
      "http://loinc.org": "LOINC",
      "http://hl7.org/fhir/sid/icd-10-cm": "ICD-10-CM",
      "http://hl7.org/fhir/sid/icd-10": "ICD-10",
      "http://www.cms.gov/Medicare/Coding/ICD10": "ICD-10-PCS",
      "http://hl7.org/fhir/sid/ndc": "NDC",
      "http://www.nlm.nih.gov/research/umls/rxnorm": "RxNorm"
    };
    if (known[s]) return known[s];
    var tail = s.split(/[\/#]/).filter(Boolean).pop();
    return tail || s;
  }

  /* ====================================================================
   * ICD — chapter tiles + classification tree
   * ================================================================== */

  var ICD10CM_ZH = {
    1: "某些傳染病和寄生蟲病", 2: "腫瘤", 3: "血液和造血器官及涉及免疫機轉的疾患", 4: "內分泌、營養和代謝疾病",
    5: "精神、行為和神經發展障礙", 6: "神經系統疾病", 7: "眼和附屬器官疾病", 8: "耳和乳突疾病",
    9: "循環系統疾病", 10: "呼吸系統疾病", 11: "消化系統疾病", 12: "皮膚和皮下組織疾病",
    13: "肌肉骨骼系統和結締組織疾病", 14: "泌尿生殖系統疾病", 15: "妊娠、生產和產褥期", 16: "源於周產期的某些病況",
    17: "先天性畸形、變形和染色體異常", 18: "症狀、徵候和臨床與實驗室異常發現，他處未歸類者", 19: "傷害、中毒和外因的某些其他後果",
    20: "罹病和死亡的外因", 21: "影響健康狀態和與健康服務接觸的因素", 22: "特殊目的代碼"
  };

  function icd() {
    var structure = cfg.structure || cfg.system;
    var useStructure = !!cfg.structure;
    var alt = cfg.alt || null;
    // A Taiwan edition (tree from ICD-10-CM, names from the edition): the
    // Chinese name leads; otherwise English leads with Chinese beside it.
    var altLeads = useStructure;
    var isCM = structure === "http://hl7.org/fhir/sid/icd-10-cm";
    var bar = el("div", { class: "tx-ex-bar" });
    var crumbEl = el("nav", { class: "tx-crumbs", "aria-label": t("tx-explore-path") });
    var chapters = el("div", { class: "tx-chapters", role: "group", "aria-label": t("tx-explore-chapters") });
    var chapCard = card(t("tx-explore-chapters"), t("tx-explore-chapter-hint"), chapters);
    chapCard._body.style.padding = "0";
    var shell = el("div", { class: "tx-shell" });
    var main = el("div", { class: "tx-main" });
    var treeHost = el("div");
    var treeCard = card(t("tx-explore-tree"), "", treeHost, "tx-card--tree");
    var panel = Panel(shell, {
      onNavigate: function (code) { revealCode(code); }
    });
    append(main, [chapCard, treeCard]);
    shell.appendChild(main);
    var params = function (extra) {
      var p = Object.assign({}, extra);
      if (alt) p.alt = alt;
      if (!alt) p.lang = "zh";
      return p;
    };
    var sys = useStructure ? structure : undefined;
    append(bar, [SearchBox({
      search: function (q) {
        return api(params({ mode: "search", q: q, count: 20 }), sys).then(function (r) {
          return (r.items || []).map(function (it) { return labelled(it); });
        });
      },
      onPick: function (it) { revealCode(it.code); }
    })]);
    append(root, [bar, crumbEl, shell]);

    function labelled(it) {
      var zh = it.alt || it.designation || "";
      var copy2 = Object.assign({}, it);
      if (altLeads && zh) {
        copy2.display = zh;
        copy2.alt = it.display;
      } else copy2.alt = zh;
      return copy2;
    }
    function rowOf(it) {
      var zh = it.alt || it.designation || "";
      var en = it.display || "";
      var chap = /^CH:/.test(it.code);
      var missing = useStructure && !zh && !chap && !/^[A-Z]\d\d-[A-Z]\d/.test(it.code) ? t("tx-explore-not-in-edition") : null;
      if (chap && !zh && isCM) {
        var m = /^Chapter (\d+)/.exec(en);
        if (m && ICD10CM_ZH[m[1]]) zh = ICD10CM_ZH[m[1]];
      }
      return {
        code: chap ? "" : it.code,
        label: altLeads && zh ? zh : en,
        alt: altLeads ? (zh ? en : null) : zh,
        altPlain: altLeads,
        altMissing: missing,
        count: it.children
      };
    }
    var activeChapter = null;
    var rootCode = null;
    var tree = null;
    function buildTree(rootC) {
      tree = Tree({
        load: function (code, offset) {
          return api(params({ mode: "children", code: code, count: 200, offset: offset }), sys);
        },
        row: rowOf,
        bilingual: true,
        rootCode: rootC,
        onSelect: function (it) { selectItem(it); }
      });
      clear(treeHost);
      treeHost.appendChild(tree.el);
      tree.start();
    }
    function selectItem(it) {
      setParam("code", it.code);
      api(params({ mode: "node", code: it.code, count: 1 }), sys).then(function (n) {
        crumbs(crumbEl, (n.path || []).map(function (p) { return { code: p.code, display: shortLabel(p) }; }), function (p) { revealCode(p.code); });
      });
      panel.show({
        system: cfg.system,
        code: it.code,
        display: it.display,
        alt: it.alt,
        fallbackSystem: useStructure ? structure : null
      });
    }
    function shortLabel(p) {
      if (/^CH:/.test(p.code)) {
        var m = /^(Chapter \d+)/.exec(p.display || "");
        return m ? m[1] : p.code;
      }
      return p.code + (p.display ? " " + p.display : "");
    }
    function revealCode(code) {
      api(params({ mode: "node", code: code, count: 1 }), sys).then(function (n) {
        var path = n.path || [];
        var chap = path.find(function (p) { return p.code === activeChapter; }) ? activeChapter : null;
        var target = null;
        if (!chap && rootCode) {
          // The chapter is the node right below the system's root.
          var at = path.findIndex(function (p) { return p.code === rootCode; });
          target = at >= 0 ? path[at + 1] : path[0];
        }
        var ready = Promise.resolve();
        if (target && target.code !== activeChapter) {
          activeChapter = target.code;
          markChapter();
          buildTree(activeChapter);
        } else if (!tree) {
          buildTree(rootCode);
        }
        ready.then(function () {
          tree.reveal(path).then(function (li) {
            if (li && li._item) selectItem(Object.assign({}, li._item, { display: n.display, alt: n.alt || n.designation }));
          });
        });
      }, function () { toast(t("tx-explore-no-results")); });
    }
    function markChapter() {
      chapters.classList.toggle("has-active", !!activeChapter);
      Array.prototype.forEach.call(chapters.querySelectorAll(".tx-chap"), function (b) {
        b.setAttribute("aria-pressed", b.getAttribute("data-code") === activeChapter ? "true" : "false");
      });
      treeCard._meta.textContent = activeChapter ? "" : t("tx-explore-all-chapters");
    }

    // Overview: the root's children are the chapters.
    chapters.appendChild(el("div", { class: "tx-ex__boot", style: { border: "0", width: "100%" } },
      el("span", { class: "tx-ex-skel", style: { width: "70%" } }), el("span", { class: "tx-ex-skel", style: { width: "40%" } })));
    var rootsP = cfg.root
      ? Promise.resolve({ items: [{ code: cfg.root, children: 1 }] })
      : api({ mode: "children", count: 50 }, sys);
    rootsP.then(function (roots) {
      var top = roots.items || [];
      if (top.length === 1 && top[0].children) {
        rootCode = top[0].code;
        return api(params({ mode: "children", code: rootCode, count: 60, descendants: 1 }), sys);
      }
      return roots;
    }).then(function (res) {
      clear(chapters);
      var items = (res.items || []).slice();
      items.sort(function (a, b) {
        var na = /^CH:(\d+)$/.exec(a.code);
        var nb = /^CH:(\d+)$/.exec(b.code);
        if (na && nb) return na[1] - nb[1];
        return a.code < b.code ? -1 : a.code > b.code ? 1 : 0;
      });
      var max = 1;
      items.forEach(function (it) { max = Math.max(max, it.descendants || it.children || 1); });
      items.forEach(function (it, i) {
        var n = it.descendants || it.children || 0;
        var m = /^Chapter (\d+):\s*(.*?)\s*(\(([^)]+)\))?$/.exec(it.display || "");
        var no = m ? m[1] : String(i + 1);
        var title = m ? m[2] : (it.display || it.code);
        var range = m && m[4] ? m[4] : (/^CH:/.test(it.code) ? "" : it.code);
        var zh = it.alt || it.designation || (isCM ? ICD10CM_ZH[no] : "") || "";
        var w = Math.max(0.7, Math.round(Math.sqrt(n / max) * 7 * 10) / 10);
        var b = el("button", {
          type: "button",
          class: "tx-chap",
          "data-code": it.code,
          "aria-pressed": "false",
          style: { "--c": palette(i), "--w": String(w), "--d": (reduced ? 0 : i * 30) + "ms" },
          onclick: function () {
            if (activeChapter === it.code) {
              activeChapter = null;
              markChapter();
              buildTree(rootCode);
              return;
            }
            activeChapter = it.code;
            markChapter();
            buildTree(it.code);
            crumbs(crumbEl, [{ code: it.code, display: m ? "Chapter " + no : it.code }], function () {});
            if (window.matchMedia("(max-width: 760px)").matches) treeCard.scrollIntoView({ behavior: reduced ? "auto" : "smooth" });
          }
        },
        el("span", { class: "tx-chap__no" }, /^CH:/.test(it.code) ? (zhUi ? "第 " + no + " 章" : "Chapter " + no) : it.code),
        altLeads && zh ? el("span", { class: "tx-chap__title", lang: "zh-Hant" }, zh) : el("span", { class: "tx-chap__title" }, title),
        zh && zh !== title ? (altLeads ? el("span", { class: "tx-chap__en" }, title) : el("span", { class: "tx-chap__zh", lang: "zh-Hant" }, zh)) : null,
        el("span", { class: "tx-chap__foot" },
          el("span", { class: "tx-chap__range" }, range),
          el("span", null, fmt(n) + " " + t("tx-explore-codes"))));
        chapters.appendChild(b);
      });
      markChapter();
      var code = getParam("code");
      if (code) revealCode(code);
      else buildTree(rootCode);
    }, function () {
      clear(chapters);
      chapters.appendChild(errorBox(function () { location.reload(); }));
    });
  }

  /* ====================================================================
   * LOINC — clusters along the axes
   * ================================================================== */

  var AXES = ["CLASSTYPE", "CLASS", "COMPONENT", "SYSTEM", "PROPERTY", "TIME_ASPCT", "SCALE_TYP", "METHOD_TYP"];
  var AXIS_COLOR = {
    CLASSTYPE: "var(--tx-c10)", CLASS: "var(--tx-c9)", COMPONENT: "var(--tx-c1)", SYSTEM: "var(--tx-c3)",
    PROPERTY: "var(--tx-c6)", TIME_ASPCT: "var(--tx-c2)", SCALE_TYP: "var(--tx-c8)", METHOD_TYP: "var(--tx-c5)"
  };

  function loinc() {
    var filters = [];
    var groupBy = "CLASSTYPE";
    var termsOffset = 0;
    var PAGE = 25;
    var valueQ = "";
    var bar = el("div", { class: "tx-ex-bar" });
    var axisbar = el("div", { class: "tx-axisbar", role: "group", "aria-label": t("tx-explore-group-by") });
    var filterbar = el("div", { class: "tx-filters", "aria-label": t("tx-explore-filters") });
    var clusters = el("div", { class: "tx-clusters" });
    var valueInput = el("input", { type: "search", placeholder: t("tx-explore-filter-values"), "aria-label": t("tx-explore-filter-values") });
    var clusterNote = el("div", { class: "tx-note tx-clusters__note" });
    var clusterCard = card(t("tx-explore-clusters"), "", el("div", null, clusters));
    clusterCard.querySelector(".tx-card__head").appendChild(el("span", { class: "tx-valuesearch" }, valueInput));
    clusterCard._body.style.padding = "0";
    clusterCard._body.firstChild.appendChild(clusterNote);
    var terms = el("div", { class: "tx-terms" });
    var pager = el("div", { class: "tx-pager" });
    var termsCard = card(t("tx-explore-terms"), "", el("div", null, terms, pager));
    termsCard._body.style.padding = "0";
    var shell = el("div", { class: "tx-shell" });
    var main = el("div", { class: "tx-main" }, clusterCard, termsCard);
    shell.appendChild(main);
    var panel = Panel(shell, { onNavigate: function (code) { panel.show({ system: cfg.system, code: code }); } });
    // Second view: the LOINC multiaxial hierarchy (parts), as a lazy tree.
    var hierHost = el("div");
    var hierCard = card(t("tx-explore-hierarchy"), "", hierHost, "tx-card--tree");
    var hierTree = null;
    var seg = el("div", { class: "tx-seg", role: "group" });
    function view(which) {
      Array.prototype.forEach.call(seg.children, function (b) { b.setAttribute("aria-pressed", b.getAttribute("data-v") === which ? "true" : "false"); });
      var clusters = which === "clusters";
      axisbar.hidden = !clusters;
      filterbar.hidden = !clusters;
      clusterCard.hidden = !clusters;
      termsCard.hidden = !clusters;
      if (!clusters) {
        if (!hierCard.parentNode) main.appendChild(hierCard);
        hierCard.hidden = false;
        if (!hierTree) {
          hierTree = Tree({
            load: function (code, offset) { return api({ mode: "children", code: code, count: 200, offset: offset, order: "display" }); },
            row: function (it) { return { code: it.code, label: it.display, count: it.children }; },
            onSelect: function (it) { panel.show({ system: cfg.system, code: it.code, display: it.display }); },
            autoExpand: true
          });
          hierHost.appendChild(hierTree.el);
          hierTree.start();
        }
      } else hierCard.hidden = true;
      setParam("view", clusters ? null : "hierarchy");
    }
    [["clusters", t("tx-explore-clusters")], ["hierarchy", t("tx-explore-hierarchy")]].forEach(function (v) {
      seg.appendChild(el("button", { type: "button", "data-v": v[0], "aria-pressed": "false", onclick: function () { view(v[0]); } }, v[1]));
    });
    append(bar, [SearchBox({
      search: defaultSearch(),
      onPick: function (it) { panel.show({ system: cfg.system, code: it.code, display: it.display }); }
    }), el("span", { class: "tx-ex-bar__spacer" }), seg]);
    append(root, [bar, axisbar, filterbar, shell]);

    var timer = null;
    valueInput.addEventListener("input", function () {
      clearTimeout(timer);
      timer = setTimeout(function () { valueQ = valueInput.value.trim(); loadClusters(); }, 280);
    });

    function axisLabel(a) { return t("tx-explore-axis-" + a); }
    function valueLabel(axis, v) {
      if (axis === "CLASSTYPE") {
        var k = "tx-explore-loinc-classtype-" + v.value;
        if (S[k]) return S[k];
      }
      return v.display || v.value;
    }
    function filtered(axis) {
      return filters.some(function (f) { return f.p === axis; });
    }
    function saveState() {
      setParam("f", filters.length ? filters.map(function (f) { return f.p + "=" + f.v; }).join("|") : null);
      setParam("by", groupBy === "CLASSTYPE" ? null : groupBy);
    }
    function renderAxes() {
      clear(axisbar);
      axisbar.appendChild(el("span", { class: "tx-axisbar__label" }, t("tx-explore-group-by")));
      AXES.forEach(function (a) {
        axisbar.appendChild(el("button", {
          type: "button",
          class: "tx-axis",
          style: { "--c": AXIS_COLOR[a] },
          "aria-pressed": a === groupBy ? "true" : "false",
          disabled: filtered(a) ? true : null,
          onclick: function () { groupBy = a; valueQ = ""; valueInput.value = ""; refresh(); }
        }, axisLabel(a)));
      });
    }
    function renderFilters() {
      clear(filterbar);
      if (!filters.length) return;
      filters.forEach(function (f, i) {
        filterbar.appendChild(el("span", { class: "tx-fchip", style: { "--c": AXIS_COLOR[f.p] } },
          el("b", null, axisLabel(f.p)), el("span", { title: f.label }, f.label),
          el("button", { type: "button", "aria-label": t("tx-explore-close") + " " + f.label, onclick: function () {
            filters.splice(i, 1);
            if (!filtered(groupBy)) { /* keep */ }
            refresh();
          } }, icon("x"))));
      });
      filterbar.appendChild(el("button", { type: "button", class: "tx-linkbtn", onclick: function () {
        filters = [];
        groupBy = "CLASSTYPE";
        refresh();
      } }, t("tx-explore-clear-filters")));
    }
    function nextAxis() {
      for (var i = 0; i < AXES.length; i++) if (!filtered(AXES[i])) return AXES[i];
      return null;
    }
    function filterParams() {
      return filters.map(function (f) { return f.p + "=" + f.v; });
    }
    // Packed bubbles: area follows the term count, every bubble at least big
    // enough for a readable two-line label, the set packed to fill the card.
    var lastValues = [];
    function drawBubbles(values, animate) {
      clear(clusters);
      if (!values.length) return;
      var W = Math.max(280, clusters.clientWidth - 24);
      var phone = W < 520;
      var H = Math.max(240, Math.min(phone ? 560 : 440, W * (phone ? 1.3 : 0.42)));
      var total = values.reduce(function (a, v) { return a + v.count; }, 0) || 1;
      var rMin = phone ? 44 : 52;
      var rMax = Math.min(H * 0.42, W * 0.3);
      var area = W * H * 0.52;
      var circles = values.map(function (v, i) {
        var r = Math.sqrt(area * (v.count / total) / Math.PI);
        return { v: v, i: i, r: Math.max(rMin, Math.min(rMax, r)) };
      });
      packCircles(circles, W / H);
      var minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
      circles.forEach(function (c) {
        minX = Math.min(minX, c.x - c.r); maxX = Math.max(maxX, c.x + c.r);
        minY = Math.min(minY, c.y - c.r); maxY = Math.max(maxY, c.y + c.r);
      });
      var scale = Math.min(1, W / (maxX - minX));
      var height = (maxY - minY) * scale;
      clusters.style.height = Math.ceil(height + 24) + "px";
      var offX = 12 + (W - (maxX - minX) * scale) / 2;
      circles.forEach(function (c) {
        var v = c.v;
        var r = c.r * scale;
        var label = valueLabel(groupBy, v);
        var x = offX + (c.x - minX) * scale - r;
        var y = 12 + (c.y - minY) * scale - r;
        clusters.appendChild(el("button", {
          type: "button",
          class: "tx-cluster",
          title: label + " — " + fmt(v.count),
          style: {
            "--c": groupBy === "CLASSTYPE" ? palette(c.i) : AXIS_COLOR[groupBy],
            "--s": Math.round(r * 2) + "px",
            left: Math.round(x) + "px",
            top: Math.round(y) + "px",
            "--d": (reduced || !animate ? 0 : Math.min(c.i, 20) * 12) + "ms"
          },
          "data-anim": animate ? null : "0",
          onclick: function (e) {
            filters.push({ p: groupBy, v: v.value, label: label });
            var nx = nextAxis();
            if (nx) groupBy = nx;
            valueQ = "";
            valueInput.value = "";
            refresh(e.currentTarget);
          }
        }, nameSpan(label, r * 2), el("span", { class: "tx-cluster__n" }, fmt(v.count))));
      });
    }
    // The label's font shrinks until its longest word fits the bubble.
    function nameSpan(label, diameter) {
      var inner = diameter - 22;
      var longest = String(label).split(/\s+/).reduce(function (m, w) { return Math.max(m, w.length); }, 1);
      var size = Math.max(9.5, Math.min(15, diameter / 10, inner / (longest * 0.62)));
      return el("span", {
        class: "tx-cluster__name",
        style: { "font-size": size.toFixed(1) + "px" },
        "data-break": inner / (longest * 0.62) < 9.5 ? "1" : null
      }, label);
    }
    if (window.ResizeObserver) {
      var lastW = 0;
      var rt = null;
      new ResizeObserver(function () {
        var w = clusters.clientWidth;
        if (Math.abs(w - lastW) < 8 || !lastValues.length || !clusters.querySelector(".tx-cluster:not(.tx-cluster--skel)")) { lastW = w; return; }
        lastW = w;
        clearTimeout(rt);
        rt = setTimeout(function () { drawBubbles(lastValues, false); }, 120);
      }).observe(clusters);
    }

    var clusterToken = 0;
    function loadClusters(chosen) {
      var my = ++clusterToken;
      clusterCard.firstChild.firstChild.textContent = t("tx-explore-clusters") + " · " + axisLabel(groupBy);
      var leave = function () {
        if (chosen && !reduced) {
          chosen.classList.add("is-chosen");
          clusters.classList.add("is-leaving");
        }
      };
      leave();
      var started = Date.now();
      var slow = setTimeout(function () {
        if (my !== clusterToken) return;
        clusters.classList.remove("is-leaving");
        clear(clusters);
        clusters.style.height = "";
        for (var i = 0; i < 6; i++) {
          clusters.appendChild(el("span", { class: "tx-cluster tx-cluster--skel", "aria-hidden": "true", style: { "--s": (150 - i * 14) + "px", "--d": i * 60 + "ms" } }));
        }
        clusterCard._meta.textContent = t("tx-explore-loading");
      }, 260);
      api({ mode: "facets", property: groupBy, filter: filterParams(), count: 32, q: valueQ || undefined }).then(function (r) {
        var wait = chosen && !reduced ? Math.max(0, 220 - (Date.now() - started)) : 0;
        clearTimeout(slow);
        setTimeout(function () {
          if (my !== clusterToken) return;
          clusters.classList.remove("is-leaving");
          clear(clusters);
          var values = r.values || [];
          clusterCard._meta.textContent = countLabel(r.matched, r.capped) + " " + t("tx-explore-terms").toLowerCase();
          if (!values.length) {
            lastValues = [];
            clusterNote.textContent = "";
            clusters.style.height = "";
            clusters.appendChild(el("div", { class: "tx-empty" }, t("tx-explore-no-results")));
            return;
          }
          lastValues = values;
          drawBubbles(values, true);
          clusterNote.textContent = r.truncated ? t("tx-explore-values-truncated") : "";
        }, wait);
      }, function () {
        clearTimeout(slow);
        if (my !== clusterToken) return;
        clusters.classList.remove("is-leaving");
        clear(clusters);
        clusters.appendChild(errorBox(function () { loadClusters(); }));
      });
    }
    var termToken = 0;
    function loadTerms() {
      var my = ++termToken;
      if (!filters.length) {
        clear(terms);
        clear(pager);
        termsCard._meta.textContent = "";
        terms.appendChild(el("div", { class: "tx-empty" }, t("tx-explore-pick-cluster")));
        return;
      }
      terms.classList.add("tx-loading-dim");
      api({ mode: "members", filter: filterParams(), show: AXES.slice(1).join(","), count: PAGE, offset: termsOffset }).then(function (r) {
        if (my !== termToken) return;
        terms.classList.remove("tx-loading-dim");
        clear(terms);
        termsCard._meta.textContent = countLabel(r.total, r.capped);
        (r.items || []).forEach(function (it, i) {
          var chips = AXES.slice(2).map(function (a) {
            var pv = it.props && it.props[a];
            if (!pv) return null;
            return el("span", { style: { "--c": AXIS_COLOR[a] }, title: axisLabel(a) + ": " + (pv.display || pv.value) }, pv.display || pv.value);
          }).filter(Boolean);
          terms.appendChild(el("button", {
            type: "button",
            class: "tx-term",
            "data-code": it.code,
            style: { "--d": (reduced ? 0 : i * 18) + "ms" },
            onclick: function (e) {
              Array.prototype.forEach.call(terms.querySelectorAll("[aria-selected]"), function (x) { x.removeAttribute("aria-selected"); });
              e.currentTarget.setAttribute("aria-selected", "true");
              panel.show({ system: cfg.system, code: it.code, display: it.display });
            }
          }, el("span", { class: "tx-term__code" }, it.code), el("span", { class: "tx-term__name" }, it.display),
          chips.length ? el("span", { class: "tx-mini" }, chips) : null));
        });
        clear(pager);
        var total = r.total || 0;
        var from = total ? termsOffset + 1 : 0;
        var to = Math.min(total, termsOffset + PAGE);
        append(pager, [el("span", null, fmt(from) + "–" + fmt(to) + " / " + countLabel(total, r.capped)),
          el("span", { class: "tx-pager__btns" },
            el("button", { type: "button", class: "tx-iconbtn", "aria-label": t("tx-explore-back"), disabled: termsOffset === 0 ? true : null, onclick: function () { termsOffset = Math.max(0, termsOffset - PAGE); loadTerms(); } }, icon("back")),
            el("button", { type: "button", class: "tx-iconbtn", "aria-label": t("tx-explore-forward"), disabled: to >= total ? true : null, onclick: function () { termsOffset += PAGE; loadTerms(); } }, icon("fwd")))]);
      }, function () {
        if (my !== termToken) return;
        terms.classList.remove("tx-loading-dim");
        clear(terms);
        terms.appendChild(errorBox(loadTerms));
      });
    }
    function refresh(chosen) {
      termsOffset = 0;
      if (filtered(groupBy)) groupBy = nextAxis() || groupBy;
      saveState();
      renderAxes();
      renderFilters();
      loadClusters(chosen);
      loadTerms();
    }
    // Restore ?f=P=v|P=v&by=AXIS (labels resolve lazily to the value).
    var f0 = getParam("f");
    if (f0) {
      f0.split("|").forEach(function (pair) {
        var i = pair.indexOf("=");
        if (i > 0) filters.push({ p: pair.slice(0, i), v: pair.slice(i + 1), label: pair.slice(i + 1) });
      });
    }
    var by = getParam("by");
    if (by && AXES.indexOf(by) >= 0) groupBy = by;
    refresh();
    view(getParam("view") === "hierarchy" ? "hierarchy" : "clusters");
  }

  /* ====================================================================
   * NDC — product type → labeler → product → package
   * ================================================================== */

  function ndc() {
    var ROOT = "NDC";
    var bar = el("div", { class: "tx-ex-bar" });
    var crumbEl = el("nav", { class: "tx-crumbs", "aria-label": t("tx-explore-path") });
    var cards = el("div", { class: "tx-cards" });
    var stage = card("", "", cards);
    stage._body.style.padding = "0";
    var foot = el("div");
    stage._body.appendChild(foot);
    var shell = el("div", { class: "tx-shell" });
    shell.appendChild(el("div", { class: "tx-main" }, stage));
    var panel = Panel(shell, { onNavigate: function (code) { panel.show({ system: cfg.system, code: code }); } });
    var stack = [];
    append(bar, [SearchBox({
      search: defaultSearch(),
      onPick: function (it) { openPath(it.code, it.display); }
    })]);
    append(root, [bar, crumbEl, shell]);

    function setTitle(title, meta) {
      stage.firstChild.firstChild.textContent = title;
      stage._meta.textContent = meta || "";
    }
    function renderCrumbs() {
      crumbs(crumbEl, stack.map(function (s) { return { code: s.key, label: s.label }; }), function (p, i) {
        stack = stack.slice(0, i + 1);
        show(stack[i], true);
      });
    }
    function push(level, fromEl) {
      stack.push(level);
      show(level, false, fromEl);
    }
    function animate(back, fromEl) {
      if (reduced) return;
      cards.classList.remove("is-zooming", "is-zooming-out");
      if (fromEl) {
        var b = fromEl.getBoundingClientRect();
        var c = cards.getBoundingClientRect();
        cards.style.transformOrigin = (b.left - c.left + b.width / 2) + "px " + (b.top - c.top + b.height / 2) + "px";
      } else cards.style.transformOrigin = "50% 0";
      void cards.offsetWidth;
      cards.classList.add(back ? "is-zooming-out" : "is-zooming");
    }
    function loadingCards() {
      clear(cards);
      clear(foot);
      for (var i = 0; i < 6; i++) {
        cards.appendChild(el("div", { class: "tx-ccard", style: { cursor: "default" } },
          el("span", { class: "tx-ex-skel", style: { width: "40%" } }), el("span", { class: "tx-ex-skel", style: { width: "80%" } })));
      }
    }
    function moreButton(label, onClick) {
      clear(foot);
      foot.appendChild(el("div", { style: { padding: "0 12px 12px" } }, el("button", { type: "button", class: "tx-more", style: { margin: "0" }, onclick: onClick }, label)));
    }
    function show(level, back, fromEl) {
      renderCrumbs();
      loadingCards();
      setParam("at", stack.length > 1 ? stack.slice(1).map(function (s) { return s.key; }).join("|") : null);
      level.render(back, fromEl);
    }

    var rootLevel = {
      key: ROOT,
      label: "NDC",
      render: function (back, fromEl) {
        setTitle(t("tx-explore-ndc-types"));
        // Product types sort after the numeric product codes: read the
        // root's children backwards.
        api({ mode: "children", code: ROOT, count: 40, order: "code-desc" }).then(function (r) {
          return { total: r.total || 0, capped: r.capped, types: (r.items || []).filter(function (it) { return !/\d/.test(it.code); }) };
        }).then(function (r) {
          clear(cards);
          animate(back, fromEl);
          var max = 1;
          r.types.forEach(function (it) { max = Math.max(max, it.children || 0); });
          var other = r.total - r.types.length;
          var otherLabel = r.capped ? fmt(other) + "+" : fmt(other);
          r.types.sort(function (a, b) { return (b.children || 0) - (a.children || 0); });
          r.types.forEach(function (it, i) {
            cards.appendChild(typeCard(it.code, titleCase(it.display || it.code), it.children || 0, max, i, null, function (e) {
              push(labelersLevel(it.code, titleCase(it.display || it.code)), e.currentTarget);
            }));
          });
          if (other > 0) {
            cards.appendChild(typeCard("NDC", t("tx-explore-ndc-unclassified"), other, Math.max(max, other), r.types.length, otherLabel, function (e) {
              push(labelersLevel(ROOT, t("tx-explore-ndc-unclassified")), e.currentTarget);
            }));
          }
          setTitle(t("tx-explore-ndc-types"), fmt(cfg.concepts || r.total) + " " + t("tx-explore-concepts").toLowerCase());
        }, function () { clear(cards); cards.appendChild(errorBox(function () { show(rootLevel, false); })); });
      }
    };
    function typeCard(kicker, title, n, max, i, label, onclick) {
      return el("button", { type: "button", class: "tx-ccard", style: { "--c": palette(i), "--d": (reduced ? 0 : i * 40) + "ms" }, onclick: onclick },
        el("span", { class: "tx-ccard__kicker" }, kicker),
        el("span", { class: "tx-ccard__title" }, title),
        el("span", { class: "tx-ccard__bar" }, el("i", { style: { "--w": Math.max(3, Math.round(Math.log10(n + 1) / Math.log10(max + 1) * 100)) + "%", "--d": (reduced ? 0 : i * 40 + 120) + "ms" } })),
        el("span", { class: "tx-ccard__n" }, (label || fmt(n)) + " " + t("tx-explore-ndc-products").toLowerCase()));
    }
    function titleCase(s) {
      return /^[A-Z0-9 \-]+$/.test(s) ? s.toLowerCase().replace(/(^|\s)\S/g, function (c) { return c.toUpperCase(); }).replace(/\bOtc\b/, "OTC") : s;
    }
    function labelersLevel(parent, label) {
      return {
        key: parent,
        label: label,
        render: function (back, fromEl) {
          setTitle(t("tx-explore-ndc-labelers") + " · " + label);
          var offset = 0;
          var first = true;
          var max = 1;
          var page = function () {
            api({ mode: "buckets", code: parent, sep: "-", count: 48, offset: offset }).then(function (r) {
              if (first) { clear(cards); animate(back, fromEl); first = false; }
              var items = r.items || [];
              if (offset === 0) max = items.length ? items[0].count : 1;
              items.forEach(function (b, i) {
                cards.appendChild(el("button", {
                  type: "button",
                  class: "tx-ccard",
                  style: { "--c": palette(hashIndex(b.key)), "--d": (reduced ? 0 : Math.min(i, 16) * 14) + "ms" },
                  onclick: function (e) { push(productsLevel(parent, b.key, b.sample && b.sample.display), e.currentTarget); }
                },
                el("span", { class: "tx-ccard__kicker" }, t("tx-explore-ndc-labeler")),
                el("span", { class: "tx-ccard__title" }, b.key),
                b.sample ? el("span", { class: "tx-ccard__sub" }, t("tx-explore-ndc-sample") + " " + productName(b.sample.display) + " · " + b.sample.code) : null,
                el("span", { class: "tx-ccard__bar" }, el("i", { style: { "--w": Math.max(3, Math.round(b.count / max * 100)) + "%", "--d": (reduced ? 0 : Math.min(i, 16) * 14 + 100) + "ms" } })),
                el("span", { class: "tx-ccard__n" }, fmt(b.count) + " " + t("tx-explore-ndc-products").toLowerCase())));
              });
              offset += items.length;
              stage._meta.textContent = fmt(r.total) + " " + t("tx-explore-ndc-labelers").toLowerCase() + " · " + fmt(r.members) + " " + t("tx-explore-ndc-products").toLowerCase();
              if (offset < (r.total || 0)) moreButton(t("tx-explore-load-more") + " · " + t("tx-explore-more-remaining", r.total - offset), page);
              else clear(foot);
            }, function () { clear(cards); cards.appendChild(errorBox(page)); });
          };
          page();
        }
      };
    }
    function productName(d) {
      return (d || "").replace(/\s*\(.*$/, "") || d || "";
    }
    function productsLevel(parent, labeler, sample) {
      return {
        key: labeler,
        label: t("tx-explore-ndc-labeler") + " " + labeler,
        render: function (back, fromEl) {
          setTitle(t("tx-explore-ndc-products") + " · " + labeler);
          var offset = 0;
          var first = true;
          var page = function () {
            api({ mode: "children", code: parent, prefix: labeler + "-", order: "display", count: 48, offset: offset }).then(function (r) {
              if (first) { clear(cards); animate(back, fromEl); first = false; }
              (r.items || []).forEach(function (it, i) {
                cards.appendChild(el("button", {
                  type: "button",
                  class: "tx-ccard",
                  style: { "--c": palette(hashIndex(productName(it.display))), "--d": (reduced ? 0 : Math.min(i, 16) * 14) + "ms" },
                  onclick: function (e) {
                    panel.show({ system: cfg.system, code: it.code, display: it.display });
                    if (it.children) push(packagesLevel(it), e.currentTarget);
                  }
                },
                el("span", { class: "tx-ccard__kicker" }, it.code),
                el("span", { class: "tx-ccard__title" }, productName(it.display)),
                el("span", { class: "tx-ccard__sub" }, (it.display || "").replace(productName(it.display), "").trim()),
                el("span", { class: "tx-ccard__n" }, fmt(it.children || 0) + " " + (it.children === 1 ? t("tx-explore-ndc-package") : t("tx-explore-ndc-packages")).toLowerCase())));
              });
              offset += (r.items || []).length;
              stage._meta.textContent = fmt(r.total) + " " + t("tx-explore-ndc-products").toLowerCase();
              if (offset < (r.total || 0)) moreButton(t("tx-explore-load-more") + " · " + t("tx-explore-more-remaining", r.total - offset), page);
              else clear(foot);
            }, function () { clear(cards); cards.appendChild(errorBox(page)); });
          };
          page();
        }
      };
    }
    function packagesLevel(product) {
      return {
        key: product.code,
        label: productName(product.display) || product.code,
        render: function (back, fromEl) {
          setTitle(t("tx-explore-ndc-packages") + " · " + product.code);
          api({ mode: "children", code: product.code, count: 100 }).then(function (r) {
            clear(cards);
            clear(foot);
            animate(back, fromEl);
            (r.items || []).forEach(function (it, i) {
              var m = /,\s*(.*?)\s*\(([^)]+)\)\s*$/.exec(it.display || "");
              cards.appendChild(el("button", {
                type: "button",
                class: "tx-ccard",
                style: { "--c": palette(i + 3), "--d": (reduced ? 0 : i * 30) + "ms" },
                onclick: function (e) {
                  Array.prototype.forEach.call(cards.querySelectorAll("[aria-selected]"), function (x) { x.removeAttribute("aria-selected"); });
                  e.currentTarget.setAttribute("aria-selected", "true");
                  panel.show({ system: cfg.system, code: it.code, display: it.display });
                }
              },
              el("span", { class: "tx-ccard__kicker" }, it.code),
              el("span", { class: "tx-ccard__title" }, m ? m[1] : it.display),
              el("span", { class: "tx-ccard__sub" }, productName(it.display))));
            });
            stage._meta.textContent = fmt(r.total) + " " + t("tx-explore-ndc-packages").toLowerCase();
          }, function () { clear(cards); cards.appendChild(errorBox(function () { show(stack[stack.length - 1], false); })); });
        }
      };
    }
    function openPath(code, display) {
      api({ mode: "node", code: code, count: 1 }).then(function (n) {
        var path = n.path || [];
        // NDC > type > product > package > 11-digit
        stack = [rootLevel];
        var typeNode = path[1] && !/\d/.test(path[1].code) ? path[1] : null;
        var productIdx = typeNode ? 2 : 1;
        var product = path[productIdx];
        var parent = typeNode ? typeNode.code : ROOT;
        if (typeNode) stack.push(labelersLevel(typeNode.code, titleCase(typeNode.display || typeNode.code)));
        else stack.push(labelersLevel(ROOT, t("tx-explore-ndc-unclassified")));
        if (product) {
          var labeler = product.code.split("-")[0];
          stack.push(productsLevel(parent, labeler));
          if (path.length > productIdx + 1) {
            stack.push(packagesLevel({ code: product.code, display: product.display }));
          }
        }
        show(stack[stack.length - 1], false);
        panel.show({ system: cfg.system, code: code, display: display || n.display });
      }, function () { toast(t("tx-explore-no-results")); });
    }
    stack = [rootLevel];
    // ?at=TYPE|LABELER|PRODUCT restores the drill-down.
    var at = getParam("at");
    if (at) {
      var k = at.split("|");
      if (k[0]) stack.push(labelersLevel(k[0], k[0] === ROOT ? t("tx-explore-ndc-unclassified") : titleCase(k[0])));
      if (k[0] && k[1]) stack.push(productsLevel(k[0], k[1]));
      if (k[0] && k[1] && k[2]) stack.push(packagesLevel({ code: k[2], display: k[2] }));
      show(stack[stack.length - 1], false);
    } else show(rootLevel, false);
  }

  /* ====================================================================
   * UCUM — units by dimension
   * ================================================================== */

  var DIMS = {
    "1": ["Dimensionless", "無因次"], "": ["Special units (offset, logarithmic, arbitrary)", "特殊單位（偏移、對數、任意）"],
    "m": ["Length", "長度"], "g": ["Mass", "質量"], "s": ["Time", "時間"], "K": ["Temperature", "溫度"],
    "C": ["Electric charge", "電荷"], "cd": ["Luminous intensity", "發光強度"], "rad": ["Plane angle", "平面角"],
    "rad2": ["Solid angle", "立體角"], "m2": ["Area", "面積"], "m3": ["Volume", "體積"], "g.m-3": ["Mass concentration", "質量濃度"],
    "m.s-1": ["Velocity", "速度"], "m.s-2": ["Acceleration", "加速度"], "s-1": ["Frequency / rate / activity", "頻率／速率／活度"],
    "g.m-1.s-2": ["Pressure", "壓力"], "g.m2.s-2": ["Energy", "能量"], "g.m2.s-3": ["Power", "功率"], "g.m.s-2": ["Force", "力"],
    "s-1.C": ["Electric current", "電流"], "g.m2.s-2.C-1": ["Voltage", "電壓"], "g.m2.s-1.C-2": ["Resistance", "電阻"],
    "g-1.m-2.s.C2": ["Conductance", "電導"], "g-1.m-2.s2.C2": ["Capacitance", "電容"], "g.m2.C-2": ["Inductance", "電感"],
    "g.s-1.C-1": ["Magnetic flux density", "磁通密度"], "g.m2.s-1.C-1": ["Magnetic flux", "磁通量"], "m-1.s-1.C": ["Magnetic field strength", "磁場強度"],
    "m-1": ["Per length (wave number, refraction)", "每長度（波數、屈光度）"], "m2.s-2": ["Absorbed / equivalent dose", "吸收／等效劑量"],
    "m2.s-1": ["Kinematic viscosity", "運動黏度"], "g.m-1.s-1": ["Dynamic viscosity", "動力黏度"], "g.m-1": ["Linear density", "線密度"],
    "m-2.cd": ["Luminance", "亮度"], "m-2.rad2.cd": ["Illuminance", "照度"], "rad2.cd": ["Luminous flux", "光通量"],
    "g.m-4.s-1": ["Fluid resistance", "流體阻力"], "g-1.C": ["Ion dose (exposure)", "照射量"], "g.m2.s-1": ["Action", "作用量"],
    "_other": ["Other derived units", "其他導出單位"]
  };

  function ucum() {
    var bar = el("div", { class: "tx-ex-bar" });
    var filter = el("input", { class: "tx-search__input", type: "search", placeholder: t("tx-explore-search-placeholder"), "aria-label": t("tx-explore-search-placeholder") });
    var fwrap = el("div", { class: "tx-search" });
    var ic = icon("search");
    ic.setAttribute("class", "tx-search__icon");
    append(fwrap, [ic, filter]);
    var groupsEl = el("div", { class: "tx-groups" });
    var strip = el("div", { class: "tx-axisbar tx-axisbar--strip", "aria-label": t("tx-explore-ucum-dimension") });
    var shell = el("div", { class: "tx-shell" });
    shell.appendChild(el("div", { class: "tx-main" }, strip, groupsEl));
    var panel = Panel(shell, {});
    append(bar, [fwrap]);
    append(root, [bar, shell]);
    var all = [];
    groupsEl.appendChild(el("div", { class: "tx-ex__boot" }, el("span", { class: "tx-ex-skel", style: { width: "60%" } }), el("span", { class: "tx-ex-skel", style: { width: "30%" } })));
    var rootP = api({ mode: "children", count: 5 }).then(function (r) {
      var top = r.items || [];
      if (top.length === 1 && top[0].children) return api({ mode: "children", code: top[0].code, count: 500 });
      return api({ mode: "children", count: 500 });
    });
    rootP.then(function (r) {
      all = r.items || [];
      render("");
    }, function () { clear(groupsEl); groupsEl.appendChild(errorBox(function () { location.reload(); })); });
    var timer;
    filter.addEventListener("input", function () {
      clearTimeout(timer);
      timer = setTimeout(function () { render(filter.value.trim().toLowerCase()); }, 120);
    });
    function dimName(c) {
      var d = DIMS[c];
      if (!d) return c;
      if (c === "1") return t("tx-explore-ucum-dimensionless");
      return zhUi ? d[1] : d[0];
    }
    function render(q) {
      clear(groupsEl);
      var groups = {};
      var sizes = {};
      all.forEach(function (u) {
        var c = u.canonical == null ? "" : u.canonical;
        sizes[c] = (sizes[c] || 0) + 1;
      });
      all.forEach(function (u) {
        if (q && (u.code + " " + (u.display || "")).toLowerCase().indexOf(q) < 0) return;
        var c = u.canonical == null ? "" : u.canonical;
        // Dimensions with a single unit share one "other" card.
        var key = sizes[c] > 1 || DIMS[c] ? c : "_other";
        (groups[key] = groups[key] || []).push(u);
      });
      var keys = Object.keys(groups).sort(function (a, b) {
        if (a === "_other" || b === "_other") return a === "_other" ? 1 : -1;
        return groups[b].length - groups[a].length || (a < b ? -1 : 1);
      });
      clear(strip);
      strip.appendChild(el("span", { class: "tx-axisbar__label" }, t("tx-explore-ucum-dimension")));
      keys.forEach(function (k) {
        strip.appendChild(el("button", {
          type: "button",
          class: "tx-axis",
          style: { "--c": palette(hashIndex(k || "special")) },
          onclick: function () {
            var g = groupsEl.querySelector('[data-dim="' + cssEsc(k) + '"]');
            if (g) g.scrollIntoView({ behavior: reduced ? "auto" : "smooth", block: "start" });
          }
        }, dimName(k) + " · " + fmt(groups[k].length)));
      });
      if (!keys.length) {
        groupsEl.appendChild(el("div", { class: "tx-empty" }, t("tx-explore-no-results")));
        return;
      }
      keys.forEach(function (k, i) {
        var units = groups[k];
        var color = palette(hashIndex(k || "special"));
        groupsEl.appendChild(el("section", { class: "tx-group", "data-dim": k, style: { "--c": color, "--d": (reduced ? 0 : Math.min(i, 20) * 30) + "ms" } },
          el("div", { class: "tx-group__head" },
            el("span", { class: "tx-group__title" }, dimName(k)),
            k && k !== "1" && k !== "_other" ? el("span", { class: "tx-group__dim", title: t("tx-explore-ucum-dimension") }, "[" + k + "]") : null,
            el("span", { class: "tx-group__n" }, fmt(units.length) + " " + t("tx-explore-ucum-units"))),
          el("div", { class: "tx-units" }, units.map(function (u) {
            return el("button", {
              type: "button",
              class: "tx-unit",
              title: u.display || u.code,
              onclick: function (e) {
                Array.prototype.forEach.call(groupsEl.querySelectorAll(".tx-unit[aria-selected]"), function (x) { x.removeAttribute("aria-selected"); });
                e.currentTarget.setAttribute("aria-selected", "true");
                panel.show({ system: cfg.system, code: u.code, display: u.display });
              }
            }, el("code", null, u.code), el("span", null, u.display || ""),
              k === "_other" && u.canonical ? el("small", { class: "tx-unit__dim" }, "[" + u.canonical + "]") : null);
          }))));
      });
    }
  }

  /* ====================================================================
   * Generic — tree (hierarchical) or grouped list (flat)
   * ================================================================== */

  function hasZh() {
    return (cfg.designationLanguages || []).some(function (d) { return /^zh/i.test(d.language || ""); });
  }

  function genericTree() {
    var bar = el("div", { class: "tx-ex-bar" });
    var crumbEl = el("nav", { class: "tx-crumbs", "aria-label": t("tx-explore-path") });
    var shell = el("div", { class: "tx-shell" });
    var extra = hasZh() ? { lang: "zh" } : {};
    var tree = Tree({
      load: function (code, offset) {
        return api(Object.assign({ mode: "children", code: code, count: 200, offset: offset, order: "code" }, extra)).then(function (r) {
          if (r.roots && r.total > 500) r.note = t("tx-explore-roots-truncated");
          return r;
        });
      },
      row: function (it) {
        return { code: it.code, label: it.display, alt: it.designation, count: it.children };
      },
      bilingual: !!extra.lang,
      onSelect: function (it) { select(it); },
      autoExpand: true
    });
    var treeCard = card(t("tx-explore-tree"), cfg.concepts ? fmt(cfg.concepts) + " " + t("tx-explore-concepts").toLowerCase() : "", tree.el, "tx-card--tree");
    shell.appendChild(el("div", { class: "tx-main" }, treeCard));
    var panel = Panel(shell, { onNavigate: function (code) { reveal(code); } });
    append(bar, [SearchBox({ search: defaultSearch(extra), onPick: function (it) { reveal(it.code); } })]);
    append(root, [bar, crumbEl, shell]);
    function select(it) {
      setParam("code", it.code);
      panel.show({ system: cfg.system, code: it.code, display: it.display, alt: it.designation });
      api({ mode: "node", code: it.code, count: 1 }).then(function (n) {
        crumbs(crumbEl, n.path || [], function (p) { reveal(p.code); });
      });
    }
    function reveal(code) {
      api(Object.assign({ mode: "node", code: code, count: 1 }, extra)).then(function (n) {
        tree.reveal(n.path || []).then(function () {
          select({ code: n.code, display: n.display, designation: n.designation });
        });
      }, function () { toast(t("tx-explore-no-results")); });
    }
    tree.start().then(function () {
      var code = getParam("code");
      if (code) reveal(code);
    });
  }

  function genericFlat() {
    var bar = el("div", { class: "tx-ex-bar" });
    var shell = el("div", { class: "tx-shell" });
    var body = el("div");
    var listCard = card(t("tx-explore-list"), cfg.concepts ? fmt(cfg.concepts) + " " + t("tx-explore-concepts").toLowerCase() : "", body);
    listCard._body.style.padding = "0";
    shell.appendChild(el("div", { class: "tx-main" }, listCard));
    var panel = Panel(shell, {});
    var extra = hasZh() ? { lang: "zh" } : {};
    append(bar, [SearchBox({ search: defaultSearch(extra), onPick: function (it) { open(it); } })]);
    append(root, [bar, shell]);
    var selectedEl = null;
    function open(it, rowEl) {
      if (selectedEl) selectedEl.setAttribute("aria-selected", "false");
      selectedEl = rowEl || null;
      if (rowEl) rowEl.setAttribute("aria-selected", "true");
      setParam("code", it.code);
      panel.show({ system: cfg.system, code: it.code, display: it.display, alt: it.designation });
    }
    function row(it, i) {
      var r = el("div", {
        class: "tx-row",
        role: "button",
        tabindex: "0",
        "aria-selected": "false",
        style: { "--d": (reduced ? 0 : Math.min(i, 20) * 14) + "ms" },
        onclick: function () { open(it, r); },
        onkeydown: function (e) { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(it, r); } }
      },
      el("span", { class: "tx-row__code", style: { "padding-left": "6px" } }, it.code),
      el("span", { class: "tx-row__text" },
        el("span", { class: "tx-row__label" }, it.display || ""),
        it.designation ? el("span", { class: "tx-row__alt", lang: "zh-Hant" }, it.designation) : null));
      return r;
    }
    function pagedList(container, params) {
      var offset = 0;
      var page = function () {
        var sk = el("div", { style: { padding: "6px" } }, el("span", { class: "tx-ex-skel", style: { width: "50%" } }));
        container.appendChild(sk);
        api(Object.assign({ count: 100, offset: offset }, extra, params)).then(function (r) {
          sk.remove();
          (r.items || []).forEach(function (it, i) { container.appendChild(row(it, i)); });
          offset += (r.items || []).length;
          if (offset < (r.total || 0)) {
            var b = el("button", { type: "button", class: "tx-more", onclick: function () { b.remove(); page(); } },
              t("tx-explore-load-more") + " · " + t("tx-explore-more-remaining", r.total - offset));
            container.appendChild(b);
          }
          if (!r.items || !r.items.length) container.appendChild(el("div", { class: "tx-empty" }, t("tx-explore-no-results")));
        }, function () { sk.remove(); container.appendChild(errorBox(page)); });
      };
      page();
    }
    // Group by the property that splits the system best (2–60 values,
    // most of the concepts covered); otherwise one paged list.
    var props = (cfg.properties || []).filter(function (p) {
      return p.distinct >= 2 && p.distinct <= 60 && p.concepts >= 0.5 * (cfg.concepts || 0) && !/^(effectiveDate|status|USE|inactive)$/i.test(p.code);
    });
    props.sort(function (a, b) { return b.concepts - a.concepts || a.distinct - b.distinct; });
    var prop = props[0];
    if (!prop) {
      var list = el("div", { class: (cfg.concepts || 0) <= 400 ? "tx-flatgrid" : "", style: { padding: "6px" } });
      body.appendChild(list);
      pagedList(list, { mode: "children", order: "code" });
      return;
    }
    listCard.firstChild.firstChild.textContent = t("tx-explore-grouped-by") + " " + prop.code;
    api({ mode: "facets", property: prop.code, count: 60 }).then(function (r) {
      var values = r.values || [];
      var max = values.length ? values[0].count : 1;
      values.forEach(function (v, i) {
        var inner = el("div", { class: "tx-fold__body" });
        var det = el("details", { class: "tx-fold", style: { "--c": palette(i) } },
          el("summary", null,
            el("span", { class: "tx-row__chev", "aria-hidden": "true" }, icon("chev")),
            el("span", { class: "tx-row__dot", style: { "--c": palette(i) } }),
            el("span", null, v.display || v.value),
            el("span", { class: "tx-row__count" }, fmt(v.count)),
            el("span", { class: "tx-fold__bar" }, el("i", { style: { "--w": Math.round(v.count / max * 100) + "%", "--c": palette(i) } }))),
          inner);
        det.addEventListener("toggle", function () {
          if (det.open && !det._loaded) {
            det._loaded = true;
            pagedList(inner, { mode: "members", filter: prop.code + "=" + v.value });
          }
        });
        body.appendChild(det);
        if (values.length === 1 || (i === 0 && values.length <= 3)) det.open = true;
      });
    }, function () { body.appendChild(errorBox(function () { location.reload(); })); });
  }

  /* ---------- boot ---------- */

  clear(root);
  root.removeAttribute("aria-busy");
  var kinds = { snomed: snomed, icd: icd, loinc: loinc, ndc: ndc, ucum: ucum, tree: genericTree, flat: genericFlat };
  (kinds[cfg.kind] || (cfg.hierarchy ? genericTree : genericFlat))();
})();
