/*
 * The console's global search (crates/ui/src/global_search.rs).
 *
 * A combobox in the topbar (ARIA 1.2 pattern: role=combobox on the input,
 * a listbox of grouped options, aria-activedescendant). `/` or Ctrl/⌘+K
 * focuses it from anywhere. Typing fetches two HTML fragments from
 * /ui/find in parallel: `part=static` (pages, sections, fields — answers at
 * once) and `part=data` (endpoints, apps, users, sync jobs, code systems…,
 * debounced). Every option names where it lives as a path.
 *
 * Choosing an option navigates like a sidebar link (an in-place swap of
 * #main through shell.js where the page allows it, else a full load) and
 * then scrolls to the exact section or field, opens the disclosures around
 * it, pulses it and focuses the control — the target travels in
 * sessionStorage, so it survives either kind of navigation.
 *
 * Recent searches are kept per user in localStorage (best effort).
 */
(function () {
  "use strict";

  var PENDING_KEY = "hm-gs-target";
  var STATIC_DELAY = 60;
  var DATA_DELAY = 260;
  var PULSE_MS = 2600;
  var MAX_RECENT = 8;

  /* ---- go to a section / field --------------------------------------- */

  function visible(el) {
    return !!(el && el.getClientRects && el.getClientRects().length);
  }

  function openAround(el) {
    for (var node = el; node && node !== document.body; node = node.parentElement) {
      if (node.tagName === "DETAILS" && !node.open) node.open = true;
    }
  }

  function reducedMotion() {
    return window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  }

  function isControl(el) {
    return /^(INPUT|SELECT|TEXTAREA|BUTTON)$/.test(el.tagName);
  }

  function highlight(targetId, fallbackId) {
    var el = targetId ? document.getElementById(targetId) : null;
    if (!el || (!visible(el) && !visible(el.closest("details")))) {
      var fb = fallbackId ? document.getElementById(fallbackId) : null;
      if (fb && !el) el = fb;
      else if (fb && !visible(el)) {
        openAround(el);
        if (!visible(el)) el = fb;
      }
    }
    if (!el) return false;
    openAround(el);
    var box = el;
    if (isControl(el)) {
      box = el.closest(".mfield, .mcheck, .radio-card, .inherit") || el;
    } else if (/^H[1-4]$/.test(el.tagName)) {
      // A card's own title marks the card; a sub-heading inside a long
      // form marks just itself.
      var card = el.closest("section, details, .card");
      box = card && card.querySelector("h1, h2, h3, h4") === el ? card : el;
    } else if (el.matches(".radio-cards__grid, .radio-cards, .checks, fieldset") && !el.classList.contains("mfield--group")) {
      box = el.closest(".mfield, fieldset.mfield") || el;
    }
    // Hidden by a show-when rule: point at the nearest visible ancestor.
    var shown = box;
    while (shown && !visible(shown) && shown.parentElement && shown.parentElement !== document.body) {
      shown = shown.parentElement;
    }
    if (shown && visible(shown)) box = shown;
    box.scrollIntoView({ block: "center", behavior: reducedMotion() ? "auto" : "smooth" });
    box.classList.remove("gs-pulse");
    // Restart the animation when the same spot is chosen twice.
    void box.offsetWidth;
    box.classList.add("gs-pulse");
    box.setAttribute("data-gs-hit", "");
    setTimeout(function () {
      box.classList.remove("gs-pulse");
      box.removeAttribute("data-gs-hit");
    }, PULSE_MS);
    var control = isControl(el) ? el : null;
    if (!control && el.matches(".radio-cards__grid, .radio-cards, .checks, fieldset")) {
      control = el.querySelector("input:checked, input, select, textarea");
    }
    if (control && !control.matches(":disabled") && visible(control)) {
      try {
        control.focus({ preventScroll: true });
      } catch (e) {}
    }
    return true;
  }

  function readPending() {
    try {
      var raw = sessionStorage.getItem(PENDING_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch (e) {
      return null;
    }
  }

  function clearPending() {
    try {
      sessionStorage.removeItem(PENDING_KEY);
    } catch (e) {}
  }

  function applyPending() {
    var p = readPending();
    if (!p) return;
    if (Date.now() - (p.t || 0) > 30000) {
      clearPending();
      return;
    }
    if (p.path !== location.pathname + location.search) return;
    clearPending();
    highlight(p.target, p.fallback);
  }

  function navigate(href, target, fallback, full) {
    var url;
    try {
      url = new URL(href, location.href);
    } catch (e) {
      location.assign(href);
      return;
    }
    var path = url.pathname + url.search;
    if (target) {
      try {
        sessionStorage.setItem(PENDING_KEY, JSON.stringify({ path: path, target: target, fallback: fallback || "", t: Date.now() }));
      } catch (e) {}
    }
    if (path === location.pathname + location.search) {
      clearPending();
      if (target) highlight(target, fallback);
      return;
    }
    if (full || !window.htmx || !document.body.hasAttribute("hx-boost")) {
      location.assign(path);
      return;
    }
    // A boosted link, exactly as a sidebar entry: shell.js decides between
    // the in-place swap and a full load.
    // The link stays in the document until the navigation is over: shell.js
    // drops the history update of a request whose element has left the page.
    var a = document.createElement("a");
    a.href = path;
    a.hidden = true;
    a.setAttribute("data-gs-nav", "");
    document.body.appendChild(a);
    window.htmx.process(a);
    a.click();
    setTimeout(function () {
      if (a.isConnected) a.remove();
    }, 60000);
  }

  document.addEventListener("hm:navigated", function () {
    var links = document.querySelectorAll("a[data-gs-nav]");
    for (var i = 0; i < links.length; i++) links[i].remove();
    applyPending();
  });

  /* ---- the combobox -------------------------------------------------- */

  function init() {
    // Full page load: a target chosen on the previous page.
    setTimeout(applyPending, 80);

    var root = document.getElementById("gsearch");
    if (!root || root.__gs) return;
    root.__gs = true;

    var input = document.getElementById("gsearch-input");
    var form = root.querySelector("[data-gs-form]");
    var panel = document.getElementById("gsearch-panel");
    var list = document.getElementById("gsearch-list");
    var staticBox = root.querySelector("[data-gs-static]");
    var dataBox = root.querySelector("[data-gs-data]");
    var txBox = root.querySelector("[data-gs-tx]");
    var boxes = { static: staticBox, data: dataBox, tx: txBox };
    var emptyBox = root.querySelector("[data-gs-empty]");
    var noneBox = root.querySelector("[data-gs-none]");
    var noneTitle = root.querySelector("[data-gs-none-title]");
    var statusEl = document.getElementById("gsearch-status");
    var openBtn = root.querySelector("[data-gs-open]");
    var closeBtn = root.querySelector("[data-gs-close]");
    var recentBox = root.querySelector("[data-gs-recent]");
    var recentList = root.querySelector("[data-gs-recent-list]");
    var kbd = root.querySelector("[data-gs-kbd]");
    var endpoint = root.getAttribute("data-gs-url") || "/ui/find";
    var recentKey = "hm-gs-recent:" + (root.getAttribute("data-gs-user") || "local");

    if (kbd && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent || "")) kbd.textContent = "⌘K";

    var state = { q: "", seq: 0, pending: 0, staticTimer: 0, dataTimer: 0, announceTimer: 0, ctrl: { static: null, data: null, tx: null }, active: null };

    function msg(name) {
      return (root.getAttribute("data-msg-" + name) || "").replace(/[⁨⁩]/g, "");
    }

    function options() {
      return Array.prototype.slice.call(list.querySelectorAll('[role="option"]'));
    }

    function setActive(opt, scroll) {
      if (state.active && state.active !== opt) state.active.setAttribute("aria-selected", "false");
      state.active = opt || null;
      if (opt) {
        opt.setAttribute("aria-selected", "true");
        input.setAttribute("aria-activedescendant", opt.id);
        if (scroll) opt.scrollIntoView({ block: "nearest" });
      } else {
        input.removeAttribute("aria-activedescendant");
      }
    }

    function isOpen() {
      return !panel.hidden;
    }

    function open() {
      if (!isOpen()) {
        panel.hidden = false;
        input.setAttribute("aria-expanded", "true");
        root.classList.add("gsearch--active");
      }
      if (!input.value.trim()) showEmpty();
    }

    function close(collapse) {
      panel.hidden = true;
      input.setAttribute("aria-expanded", "false");
      root.classList.remove("gsearch--active");
      setActive(null);
      if (collapse !== false) {
        root.classList.remove("gsearch--expanded");
        if (openBtn) openBtn.setAttribute("aria-expanded", "false");
      }
    }

    function announce(text) {
      clearTimeout(state.announceTimer);
      state.announceTimer = setTimeout(function () {
        if (statusEl) statusEl.textContent = text;
      }, 350);
    }

    /* recent searches */

    function recent() {
      try {
        var v = JSON.parse(localStorage.getItem(recentKey) || "[]");
        return Array.isArray(v) ? v.filter(function (s) { return typeof s === "string"; }) : [];
      } catch (e) {
        return [];
      }
    }

    function remember(q) {
      q = (q || "").trim();
      if (q.length < 2) return;
      var list = recent().filter(function (s) { return s.toLowerCase() !== q.toLowerCase(); });
      list.unshift(q);
      try {
        localStorage.setItem(recentKey, JSON.stringify(list.slice(0, MAX_RECENT)));
      } catch (e) {}
    }

    function renderRecent() {
      var items = recent();
      recentList.textContent = "";
      items.forEach(function (q) {
        var b = document.createElement("button");
        b.type = "button";
        b.className = "gsearch__chip gsearch__chip--recent";
        b.setAttribute("data-gs-q", q);
        b.textContent = q;
        recentList.appendChild(b);
      });
      recentBox.hidden = items.length === 0;
    }

    function showEmpty() {
      staticBox.textContent = "";
      dataBox.textContent = "";
      txBox.textContent = "";
      list.hidden = true;
      noneBox.hidden = true;
      renderRecent();
      emptyBox.hidden = false;
      setActive(null);
    }

    /* fetching */

    function fetchPart(part, seq) {
      if (state.ctrl[part]) state.ctrl[part].abort();
      var ctrl = window.AbortController ? new AbortController() : null;
      state.ctrl[part] = ctrl;
      state.pending++;
      list.setAttribute("aria-busy", "true");
      var url = endpoint + "?part=" + part + "&q=" + encodeURIComponent(state.q);
      fetch(url, { credentials: "same-origin", headers: { Accept: "text/html" }, signal: ctrl ? ctrl.signal : undefined })
        .then(function (r) {
          if (!r.ok || r.redirected) return "";
          return r.text();
        })
        .then(function (html) {
          if (seq !== state.seq) return;
          boxes[part].innerHTML = html;
          refresh(part === "data");
        })
        .catch(function () {})
        .then(function () {
          state.pending = Math.max(0, state.pending - 1);
          if (seq === state.seq && state.pending === 0) {
            list.removeAttribute("aria-busy");
            refresh(true);
          }
        });
    }

    function refresh(final) {
      var opts = options();
      var groups = list.querySelectorAll(".gs-group");
      for (var g = 0; g < groups.length; g++) groups[g].classList.toggle("gs-group--first", g === 0);
      var hasAny = opts.length > 0;
      emptyBox.hidden = true;
      list.hidden = !hasAny;
      if (!hasAny && final && state.pending === 0 && state.q) {
        noneTitle.textContent = msg("no-results").replace("{query}", state.q);
        noneBox.hidden = false;
      } else {
        noneBox.hidden = true;
      }
      if (!state.active || opts.indexOf(state.active) < 0) setActive(opts[0] || null, false);
      if (final && state.pending === 0) {
        var n = opts.length;
        var text = n === 0 ? msg("count-zero") : n === 1 ? msg("count-one") : msg("count-other").replace("{count}", String(n));
        announce(text);
      }
    }

    function search() {
      var q = input.value.trim();
      if (q === state.q && (staticBox.childNodes.length || dataBox.childNodes.length)) return;
      state.q = q;
      state.seq++;
      clearTimeout(state.staticTimer);
      clearTimeout(state.dataTimer);
      open();
      if (!q) {
        state.pending = 0;
        ["static", "data", "tx"].forEach(function (p) {
          if (state.ctrl[p]) state.ctrl[p].abort();
        });
        list.removeAttribute("aria-busy");
        showEmpty();
        return;
      }
      var seq = state.seq;
      // The data half of the previous query no longer matches: drop it.
      dataBox.textContent = "";
      txBox.textContent = "";
      state.staticTimer = setTimeout(function () {
        fetchPart("static", seq);
      }, STATIC_DELAY);
      state.dataTimer = setTimeout(function () {
        fetchPart("data", seq);
        if (q.length >= 3) fetchPart("tx", seq);
      }, DATA_DELAY);
    }

    function choose(opt, event) {
      if (!opt) return;
      if (event && (event.ctrlKey || event.metaKey || event.shiftKey || event.button === 1)) return;
      if (event) event.preventDefault();
      remember(state.q || input.value);
      var href = opt.getAttribute("href");
      var target = opt.getAttribute("data-gs-target") || "";
      var fallback = opt.getAttribute("data-gs-fallback") || "";
      var full = opt.hasAttribute("data-gs-full");
      close();
      input.blur();
      navigate(href, target, fallback, full);
    }

    function move(delta) {
      var opts = options();
      if (!opts.length) return;
      var i = opts.indexOf(state.active);
      if (i < 0) i = delta > 0 ? -1 : 0;
      i = (i + delta + opts.length) % opts.length;
      setActive(opts[i], true);
    }

    input.addEventListener("input", search);
    input.addEventListener("focus", function () {
      open();
      if (input.value.trim() && !staticBox.childNodes.length) search();
    });

    input.addEventListener("keydown", function (e) {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          if (!isOpen()) open();
          move(1);
          break;
        case "ArrowUp":
          e.preventDefault();
          move(-1);
          break;
        case "Home":
        case "End":
          if (isOpen() && options().length && e.ctrlKey) {
            e.preventDefault();
            var o = options();
            setActive(e.key === "Home" ? o[0] : o[o.length - 1], true);
          }
          break;
        case "Enter":
          if (state.active && isOpen() && !list.hidden) {
            e.preventDefault();
            choose(state.active, null);
          } else {
            remember(input.value);
          }
          break;
        case "Escape":
          e.preventDefault();
          if (isOpen()) close();
          else if (input.value) {
            input.value = "";
            search();
          } else input.blur();
          break;
        case "Tab":
          close(false);
          break;
      }
    });

    form.addEventListener("submit", function () {
      remember(input.value);
    });

    list.addEventListener("mousemove", function (e) {
      var opt = e.target.closest && e.target.closest('[role="option"]');
      if (opt && opt !== state.active) setActive(opt, false);
    });
    // Keep focus in the input while clicking an option.
    list.addEventListener("mousedown", function (e) {
      if (e.target.closest && e.target.closest('[role="option"]')) e.preventDefault();
    });
    list.addEventListener("click", function (e) {
      var opt = e.target.closest && e.target.closest('[role="option"]');
      if (opt) choose(opt, e);
    });

    panel.addEventListener("mousedown", function (e) {
      if (e.target.closest && e.target.closest("[data-gs-q], [data-gs-clear-recent]")) e.preventDefault();
    });
    panel.addEventListener("click", function (e) {
      var chip = e.target.closest && e.target.closest("[data-gs-q]");
      if (chip) {
        input.value = chip.getAttribute("data-gs-q");
        input.focus();
        search();
        return;
      }
      if (e.target.closest && e.target.closest("[data-gs-clear-recent]")) {
        try {
          localStorage.removeItem(recentKey);
        } catch (err) {}
        renderRecent();
        input.focus();
      }
    });

    if (openBtn) {
      openBtn.addEventListener("click", function () {
        focusSearch();
      });
    }
    if (closeBtn) {
      closeBtn.addEventListener("click", function () {
        input.value = "";
        state.q = "";
        close();
        if (openBtn && root.offsetParent !== null && getComputedStyle(openBtn).display !== "none") openBtn.focus();
      });
    }

    document.addEventListener("mousedown", function (e) {
      if (!root.contains(e.target) && isOpen()) close();
    });
    root.addEventListener("focusout", function () {
      setTimeout(function () {
        if (!root.contains(document.activeElement) && isOpen()) close();
      }, 0);
    });

    function focusSearch() {
      root.classList.add("gsearch--expanded");
      if (openBtn) openBtn.setAttribute("aria-expanded", "true");
      input.focus();
      input.select();
      open();
    }

    function typing(el) {
      if (!el || el === document.body) return false;
      if (el.isContentEditable) return true;
      return /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName);
    }

    document.addEventListener("keydown", function (e) {
      if (e.defaultPrevented) return;
      var k = (e.key || "").toLowerCase();
      if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && k === "k") {
        e.preventDefault();
        focusSearch();
        return;
      }
      if (e.key === "/" && !e.ctrlKey && !e.metaKey && !e.altKey && !typing(e.target)) {
        e.preventDefault();
        focusSearch();
      }
    });

    // A page swapped in place: the topbar stays, the list closes.
    document.addEventListener("hm:navigated", function () {
      if (document.activeElement !== input) close();
    });
  }

  window.HfsSearch = { highlight: highlight, navigate: navigate };

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
