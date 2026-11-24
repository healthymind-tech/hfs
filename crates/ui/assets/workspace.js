/*
 * The list-and-editor workspace (hm/vd-layout): View Definitions, SQL
 * Queries, SQL Views and Compartments — every `.filter-layout--two[data-ws]`.
 *
 *  - The list rail collapses to a slim strip (and back) from its own toggle.
 *    On wide screens the choice is remembered (localStorage `hfs-ws-rail`);
 *    with no choice yet, a short list (`data-rail-few`, set by the server)
 *    starts collapsed. At 1250px and below the rail always starts collapsed
 *    and opens over the work area; on phones it opens as a sheet.
 *  - The quick switcher beside the selected item's title lists the rail's own
 *    items (filterable, keyboard driven) and activates one by clicking the
 *    matching rail link — the unsaved-changes guard and the rail's
 *    navigation apply exactly as for a click in the rail.
 *  - Every `.editor__grid--stretch[data-ws-pair]` gets a draggable,
 *    keyboard-resizable split between the JSON editor and the guided form
 *    (`hfs-ws-split`, the editor's share in %), and a segmented control
 *    shows JSON only / both / form only (`hfs-ws-view`).
 *  - `[data-ws-pair="fill"]` grows to the bottom of the viewport instead of
 *    a fixed 70vh.
 *
 * The first-paint boot script (`helios_ui_chrome::theme_boot`) stamps the
 * stored preferences on <html> (`data-ws-rail`, `data-ws-view`,
 * `--ws-split`) so CSS lays the page out right before this file runs; this
 * file only wires behaviour and keeps those stamps current. Motion goes
 * through `HM.motion` (shell.js), which does nothing under
 * prefers-reduced-motion.
 */
(function () {
  "use strict";

  var html = document.documentElement;
  var KEY_RAIL = "hfs-ws-rail";
  var KEY_VIEW = "hfs-ws-view";
  var KEY_SPLIT = "hfs-ws-split";
  var SPLIT_MIN = 20;
  var SPLIT_MAX = 80;
  var PANE_MIN_PX = 300;
  var narrowQuery = window.matchMedia ? window.matchMedia("(max-width: 1250px)") : null;
  var phoneQuery = window.matchMedia ? window.matchMedia("(max-width: 900px)") : null;

  function store(key, value) {
    try {
      if (value == null) window.localStorage.removeItem(key);
      else window.localStorage.setItem(key, String(value));
    } catch (e) {}
  }

  function motion() {
    return window.HM && window.HM.motion ? window.HM.motion : null;
  }

  function narrow() {
    return !!(narrowQuery && narrowQuery.matches);
  }

  function phone() {
    return !!(phoneQuery && phoneQuery.matches);
  }

  /* ---- the rail --------------------------------------------------------- */

  // The state CSS would show without a `data-rail` override: the stored
  // choice, else collapsed for a short list. Narrow screens always start
  // collapsed (the open rail covers the work area there).
  function defaultRail(layout) {
    if (narrow()) return "collapsed";
    var pref = html.getAttribute("data-ws-rail");
    if (pref === "collapsed" || pref === "open") return pref;
    return layout.hasAttribute("data-rail-few") ? "collapsed" : "open";
  }

  function railState(layout) {
    return layout.getAttribute("data-rail") || defaultRail(layout);
  }

  function syncToggles(layout) {
    var open = railState(layout) === "open";
    var toggles = layout.querySelectorAll("[data-rail-toggle]");
    for (var i = 0; i < toggles.length; i++) {
      var t = toggles[i];
      t.setAttribute("aria-expanded", open ? "true" : "false");
      var label = t.getAttribute(open ? "data-label-collapse" : "data-label-expand");
      if (label) {
        t.setAttribute("aria-label", label);
        t.setAttribute("title", label);
      }
    }
  }

  function setRail(layout, state, opts) {
    opts = opts || {};
    if (railState(layout) === state && layout.hasAttribute("data-rail")) return;
    layout.setAttribute("data-rail", state);
    if (opts.remember && !narrow()) {
      html.setAttribute("data-ws-rail", state);
      store(KEY_RAIL, state);
    }
    syncToggles(layout);
    var rail = layout.querySelector(".filter-rail");
    var m = motion();
    if (state === "open" && rail && m) {
      // The column (or the overlay) grows through its CSS transition; the
      // contents fade in from the left behind it.
      var kids = rail.children;
      for (var i = 0; i < kids.length; i++) {
        if (!kids[i].classList.contains("filter-rail__top")) m.enter(kids[i], { dx: -8, duration: m.tokens.d3, delay: 40 });
      }
      if (narrow()) m.enter(rail, { dx: phone() ? 0 : -12, dy: phone() ? 16 : 0, duration: m.tokens.d3 });
    }
    if (opts.focus && state === "open" && rail) {
      var input = rail.querySelector(".filter-rail__search input[type=search]");
      var current = rail.querySelector(".filter-rail__item[aria-current='true']");
      var target = opts.focus === "search" ? input : current || input;
      if (target) target.focus({ preventScroll: false });
    }
    if (state === "collapsed" && opts.returnFocus) opts.returnFocus.focus();
    layout.dispatchEvent(new CustomEvent("ws:rail", { bubbles: true, detail: { state: state } }));
  }

  function initRail(layout) {
    syncToggles(layout);
    // An active rail filter keeps the rail open: its search box is where the
    // filter can be seen and cleared.
    var search = layout.querySelector(".filter-rail__search input[type=search]");
    if (search && search.value && !narrow()) layout.setAttribute("data-rail", "open");
    syncToggles(layout);

    layout.addEventListener("click", function (event) {
      var toggle = event.target.closest && event.target.closest("[data-rail-toggle]");
      if (!toggle || !layout.contains(toggle)) return;
      event.preventDefault();
      var next = railState(layout) === "open" ? "collapsed" : "open";
      setRail(layout, next, { remember: true, focus: next === "open" && narrow() ? "item" : null, returnFocus: narrow() ? toggle : null });
    });

    // Narrow screens: the open rail floats over the work area. Escape, a
    // click outside it, or following one of its links closes it.
    document.addEventListener("keydown", function (event) {
      if (event.key !== "Escape" || !narrow() || railState(layout) !== "open") return;
      var toggle = layout.querySelector(".filter-center [data-rail-toggle]") || layout.querySelector("[data-rail-toggle]");
      if (phone()) toggle = layout.querySelector(".ws-title__rail") || toggle;
      setRail(layout, "collapsed", { returnFocus: toggle });
    });
    document.addEventListener("pointerdown", function (event) {
      if (!narrow() || railState(layout) !== "open") return;
      var rail = layout.querySelector(".filter-rail");
      if (!rail || rail.contains(event.target)) return;
      if (event.target.closest && event.target.closest("[data-rail-toggle]")) return;
      setRail(layout, "collapsed");
    });

    function onResize() {
      // Leaving or entering the narrow range drops a transient open/closed
      // state so the stored preference (or the narrow default) applies.
      if (layout.hasAttribute("data-rail") && !(search && search.value && !narrow())) layout.removeAttribute("data-rail");
      syncToggles(layout);
    }
    if (narrowQuery && narrowQuery.addEventListener) narrowQuery.addEventListener("change", onResize);
    if (phoneQuery && phoneQuery.addEventListener) phoneQuery.addEventListener("change", onResize);
  }

  /* ---- the quick switcher ------------------------------------------------ */

  function initSwitcher(layout) {
    var host = layout.querySelector(".ws-switch");
    if (!host) return;
    var btn = host.querySelector("[data-ws-switch]");
    var pop = host.querySelector(".ws-switch__pop");
    var input = host.querySelector(".ws-switch__input");
    var list = host.querySelector(".ws-switch__list");
    var empty = host.querySelector(".ws-switch__empty");
    var more = host.querySelector("[data-ws-switch-more]");
    var rail = document.getElementById(host.getAttribute("data-ws-switch-for"));
    if (!btn || !pop || !input || !list || !rail) return;
    var options = [];
    var active = -1;

    // The rail's items, recently used first, one entry per link.
    function collect() {
      var seen = {};
      var out = [];
      var links = rail.querySelectorAll("a.filter-rail__item");
      for (var i = 0; i < links.length; i++) {
        var a = links[i];
        var href = a.getAttribute("href");
        if (!href || seen[href]) continue;
        if (a.closest(".filter-rail__group[hidden]")) continue;
        seen[href] = true;
        var label = a.querySelector(".filter-rail__label");
        var meta = a.querySelector(".filter-rail__meta, .count");
        out.push({
          link: a,
          name: (a.getAttribute("data-full-name") || (label ? label.textContent : a.textContent) || "").trim(),
          meta: meta ? meta.textContent.trim() : "",
          current: a.getAttribute("aria-current") === "true",
        });
      }
      return out;
    }

    function render() {
      var needle = input.value.trim().toLowerCase();
      var items = collect().filter(function (o) {
        return !needle || o.name.toLowerCase().indexOf(needle) >= 0;
      });
      list.textContent = "";
      options = [];
      items.forEach(function (o, i) {
        var li = document.createElement("li");
        li.id = list.id + "-" + i;
        li.className = "ws-switch__opt";
        li.setAttribute("role", "option");
        li.setAttribute("aria-selected", o.current ? "true" : "false");
        if (o.current) li.classList.add("is-current");
        var name = document.createElement("span");
        name.className = "ws-switch__name";
        name.textContent = o.name;
        name.title = o.name;
        li.appendChild(name);
        if (o.meta) {
          var meta = document.createElement("span");
          meta.className = "ws-switch__meta";
          meta.textContent = o.meta;
          li.appendChild(meta);
        }
        li.addEventListener("mousedown", function (e) { e.preventDefault(); });
        li.addEventListener("click", function () { choose(i); });
        li.addEventListener("mousemove", function () { if (active !== i) setActive(i); });
        list.appendChild(li);
        options.push({ el: li, item: o });
      });
      if (empty) empty.hidden = options.length > 0;
      list.hidden = options.length === 0;
      var cur = -1;
      for (var k = 0; k < options.length; k++) if (options[k].item.current) cur = k;
      setActive(needle ? 0 : Math.max(cur, 0));
      var m = motion();
      if (m && !needle) m.stagger(list.children, { max: 12, total: 200 });
    }

    function setActive(i) {
      if (active >= 0 && options[active]) options[active].el.classList.remove("is-active");
      active = options.length ? Math.max(0, Math.min(i, options.length - 1)) : -1;
      if (active >= 0) {
        var el = options[active].el;
        el.classList.add("is-active");
        input.setAttribute("aria-activedescendant", el.id);
        if (el.scrollIntoView) el.scrollIntoView({ block: "nearest" });
      } else {
        input.removeAttribute("aria-activedescendant");
      }
    }

    function isOpen() {
      return !pop.hidden;
    }

    function open() {
      if (isOpen()) return;
      input.value = "";
      pop.hidden = false;
      btn.setAttribute("aria-expanded", "true");
      render();
      var m = motion();
      if (m) m.animate(pop, [
        { opacity: 0, transform: "translateY(-4px) scale(.98)" },
        { opacity: 1, transform: "none" },
      ], { duration: m.tokens.d2, easing: m.tokens.emph });
      input.focus();
    }

    function close(returnFocus) {
      if (!isOpen()) return;
      pop.hidden = true;
      btn.setAttribute("aria-expanded", "false");
      if (returnFocus) btn.focus();
    }

    function choose(i) {
      var o = options[i];
      if (!o) return;
      close(false);
      if (o.item.current) {
        btn.focus();
        return;
      }
      o.item.link.click();
    }

    btn.addEventListener("click", function () {
      if (isOpen()) close(true);
      else open();
    });
    input.addEventListener("input", render);
    input.addEventListener("keydown", function (event) {
      switch (event.key) {
        case "ArrowDown": event.preventDefault(); setActive(active + 1); break;
        case "ArrowUp": event.preventDefault(); setActive(active - 1); break;
        case "Home": if (!input.value) { event.preventDefault(); setActive(0); } break;
        case "End": if (!input.value) { event.preventDefault(); setActive(options.length - 1); } break;
        case "PageDown": event.preventDefault(); setActive(active + 8); break;
        case "PageUp": event.preventDefault(); setActive(active - 8); break;
        case "Enter": event.preventDefault(); choose(active); break;
        case "Escape": event.preventDefault(); event.stopPropagation(); close(true); break;
        case "Tab": close(false); break;
      }
    });
    if (more) {
      more.addEventListener("click", function () {
        var needle = input.value.trim();
        close(false);
        setRail(layout, "open", { remember: !narrow(), focus: "search" });
        var search = rail.querySelector(".filter-rail__search input[type=search]");
        if (search && needle) {
          search.value = needle;
          search.dispatchEvent(new Event("input", { bubbles: true }));
        }
      });
    }
    document.addEventListener("pointerdown", function (event) {
      if (isOpen() && !host.contains(event.target)) close(false);
    });
    // Ctrl/Cmd+Shift+O-free: "/" belongs to global search, so the switcher
    // opens from its own button only (and Alt+ArrowDown on it).
    btn.addEventListener("keydown", function (event) {
      if (event.altKey && event.key === "ArrowDown") {
        event.preventDefault();
        open();
      }
    });
  }

  /* ---- the editor / guided-form pairing ---------------------------------- */

  function currentSplit() {
    var v = parseFloat(getComputedStyle(html).getPropertyValue("--ws-split"));
    return v >= SPLIT_MIN && v <= SPLIT_MAX ? v : 50;
  }

  function setSplit(v, remember) {
    v = Math.round(Math.max(SPLIT_MIN, Math.min(SPLIT_MAX, v)) * 10) / 10;
    html.style.setProperty("--ws-split", String(v));
    var handles = document.querySelectorAll(".ws-split");
    for (var i = 0; i < handles.length; i++) {
      handles[i].setAttribute("aria-valuenow", String(Math.round(v)));
      handles[i].setAttribute("aria-valuetext", Math.round(v) + "% / " + Math.round(100 - v) + "%");
    }
    if (remember) store(KEY_SPLIT, v === 50 ? null : v);
    return v;
  }

  function currentView() {
    var v = html.getAttribute("data-ws-view");
    return v === "json" || v === "form" ? v : "both";
  }

  function setView(view, from) {
    var before = currentView();
    if (view === "both") html.removeAttribute("data-ws-view");
    else html.setAttribute("data-ws-view", view);
    store(KEY_VIEW, view === "both" ? null : view);
    syncSegs(from, before);
    // The editor may have been hidden: let CodeMirror re-measure.
    window.dispatchEvent(new Event("resize"));
    var pairs = document.querySelectorAll("[data-ws-pair]");
    var m = motion();
    for (var i = 0; i < pairs.length && m; i++) {
      var shown = pairs[i].querySelectorAll(":scope > .card");
      for (var k = 0; k < shown.length; k++) {
        if (shown[k].offsetParent !== null) m.fade(shown[k], { duration: m.tokens.d2 });
      }
    }
  }

  function syncSegs(from, before) {
    var view = currentView();
    // On phones "both" stacks nothing side by side: it shows the editor.
    var shown = phone() && view === "both" ? "json" : view;
    var segs = document.querySelectorAll(".ws-seg");
    for (var i = 0; i < segs.length; i++) {
      var prev = null;
      var next = null;
      var btns = segs[i].querySelectorAll("[data-ws-view]");
      for (var k = 0; k < btns.length; k++) {
        var on = btns[k].getAttribute("data-ws-view") === shown;
        if (btns[k].getAttribute("aria-pressed") === "true" && !on) prev = btns[k];
        if (on) next = btns[k];
        btns[k].setAttribute("aria-pressed", on ? "true" : "false");
      }
      var m = motion();
      if (m && from && before !== undefined && prev && next && segs[i].contains(from)) m.glide(prev, next, "ws-seg__ink");
    }
  }

  function initSegs() {
    var segs = document.querySelectorAll(".ws-seg");
    for (var i = 0; i < segs.length; i++) {
      if (segs[i].__ws) continue;
      segs[i].__ws = true;
      segs[i].addEventListener("click", function (event) {
        var b = event.target.closest && event.target.closest("[data-ws-view]");
        if (!b) return;
        setView(b.getAttribute("data-ws-view"), b);
      });
      // Arrow keys move between the segments, like a radio group.
      segs[i].addEventListener("keydown", function (event) {
        if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
        var btns = Array.prototype.filter.call(this.querySelectorAll("[data-ws-view]"), function (b) {
          return b.offsetParent !== null;
        });
        var at = btns.indexOf(document.activeElement);
        if (at < 0) return;
        event.preventDefault();
        var n = btns[(at + (event.key === "ArrowRight" ? 1 : btns.length - 1)) % btns.length];
        n.focus();
        setView(n.getAttribute("data-ws-view"), n);
      });
    }
    syncSegs();
    if (phoneQuery && phoneQuery.addEventListener) phoneQuery.addEventListener("change", function () { syncSegs(); });
  }

  function initPair(grid) {
    if (grid.__ws) return;
    grid.__ws = true;
    var json = grid.querySelector(":scope > .card:not(.editor-form)");
    if (!json) return;
    var handle = document.createElement("div");
    handle.className = "ws-split";
    handle.setAttribute("role", "separator");
    handle.setAttribute("tabindex", "0");
    handle.setAttribute("aria-orientation", "vertical");
    handle.setAttribute("aria-valuemin", String(SPLIT_MIN));
    handle.setAttribute("aria-valuemax", String(SPLIT_MAX));
    if (json.id) handle.setAttribute("aria-controls", json.id);
    var label = grid.getAttribute("data-ws-split-label");
    if (label) {
      handle.setAttribute("aria-label", label);
      handle.setAttribute("title", label);
    }
    json.after(handle);
    setSplit(currentSplit(), false);

    // Each pane keeps at least PANE_MIN_PX (or the 20-80% range, whichever
    // is tighter) while dragging.
    function bounds() {
      var w = grid.getBoundingClientRect().width;
      var minPct = w > 0 ? Math.max(SPLIT_MIN, (PANE_MIN_PX / w) * 100) : SPLIT_MIN;
      return { lo: Math.min(minPct, 50), hi: Math.max(100 - minPct, 50) };
    }
    function clampPct(v) {
      var b = bounds();
      return Math.max(b.lo, Math.min(b.hi, v));
    }

    // The drag follows the pointer on the window (and, where supported,
    // captures it), so a fast move off the 16px handle never drops it.
    var dragging = null;
    function onMove(event) {
      if (dragging === null) return;
      var r = grid.getBoundingClientRect();
      setSplit(clampPct(((event.clientX - r.left) / r.width) * 100), false);
    }
    function stop(event) {
      if (dragging === null) return;
      try { handle.releasePointerCapture(dragging); } catch (e) {}
      dragging = null;
      grid.classList.remove("is-resizing");
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      setSplit(currentSplit(), true);
      window.dispatchEvent(new Event("resize"));
    }
    handle.addEventListener("pointerdown", function (event) {
      if (event.button !== 0) return;
      event.preventDefault();
      dragging = event.pointerId;
      try { handle.setPointerCapture(event.pointerId); } catch (e) {}
      grid.classList.add("is-resizing");
      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", stop);
      window.addEventListener("pointercancel", stop);
    });
    handle.addEventListener("dblclick", function () { setSplit(50, true); });
    handle.addEventListener("keydown", function (event) {
      var v = currentSplit();
      var step = event.shiftKey ? 10 : 2;
      var b = bounds();
      var next = null;
      if (event.key === "ArrowLeft") next = v - step;
      else if (event.key === "ArrowRight") next = v + step;
      else if (event.key === "Home") next = b.lo;
      else if (event.key === "End") next = b.hi;
      else if (event.key === "Enter") next = 50;
      if (next == null) return;
      event.preventDefault();
      setSplit(clampPct(next), true);
    });

    if (grid.getAttribute("data-ws-pair") === "fill") initFill(grid);
  }

  /* ---- fill the viewport ------------------------------------------------- */

  // The pairing reaches the bottom of the visible work area (less a small
  // margin), never below `min` px. On wide screens the work column scrolls
  // itself (`.content--app .filter-center`); below 1101px the page scrolls,
  // under the sticky title row.
  function initFill(grid) {
    var center = grid.closest(".filter-center");
    var MIN = 420;
    var raf = 0;
    function measure() {
      raf = 0;
      var h;
      var scroller = center && getComputedStyle(center).overflowY !== "visible" && center.scrollHeight >= center.clientHeight ? center : null;
      if (scroller && !phone() && window.innerWidth > 1100) {
        var cTop = scroller.getBoundingClientRect().top;
        var top = grid.getBoundingClientRect().top - cTop + scroller.scrollTop;
        h = scroller.clientHeight - top - 16;
      } else {
        var head = center ? center.querySelector(".ws-head") : null;
        var topbar = parseFloat(getComputedStyle(html).getPropertyValue("--topbar-h")) || 52;
        h = window.innerHeight - topbar - (head ? head.getBoundingClientRect().height : 0) - 32;
      }
      grid.style.setProperty("--ws-fill", Math.max(MIN, Math.round(h)) + "px");
    }
    function schedule() {
      if (!raf) raf = requestAnimationFrame(measure);
    }
    measure();
    window.addEventListener("resize", schedule);
    if (window.ResizeObserver && center) new ResizeObserver(schedule).observe(center);
    // Notices above the editor (a save error, "selection filtered") come and
    // go with htmx swaps.
    document.addEventListener("htmx:afterSettle", schedule);
  }

  /* ---- boot -------------------------------------------------------------- */

  function init(root) {
    root = root || document;
    var layouts = root.querySelectorAll(".filter-layout--two[data-ws]");
    for (var i = 0; i < layouts.length; i++) {
      var layout = layouts[i];
      if (layout.__ws) continue;
      layout.__ws = true;
      initRail(layout);
      initSwitcher(layout);
      // The title truncates and narrow work areas show Duplicate/Delete as
      // icons: their full text becomes the tooltip.
      var titled = layout.querySelectorAll(".ws-title__name, .ws-head > .page-head__actions > .btn");
      for (var t = 0; t < titled.length; t++) {
        if (!titled[t].getAttribute("title")) titled[t].setAttribute("title", titled[t].textContent.replace(/\s+/g, " ").trim());
      }
      layout.classList.add("ws-ready");
    }
    var pairs = root.querySelectorAll(".editor__grid--stretch[data-ws-pair]");
    for (var k = 0; k < pairs.length; k++) initPair(pairs[k]);
    initSegs();
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", function () { init(); });
  else init();
  // In-place navigations (Compartments' boosted rail) bring a new layout.
  document.addEventListener("hm:navigated", function () { init(); });
  document.addEventListener("htmx:afterSettle", function (event) {
    if (event.target && event.target.querySelector && event.target.querySelector("[data-ws], [data-ws-pair]")) init(event.target);
  });
})();
