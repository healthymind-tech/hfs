/*
 * The console sidebar's icon-rail mode (hm/vd-layout), from 1025px up.
 *
 * <html data-sb="collapsed|expanded"> is stamped before first paint by the
 * boot script (`helios_ui_chrome::theme_boot`): the stored choice
 * (localStorage `hfs-sidebar`), else by width — collapsed below 1280px —
 * marked `data-sb-auto` so a resize across 1280px keeps following the width
 * until the user chooses. app.css lays the rail out from that attribute
 * alone; this script toggles it (the sidebar's own button, Ctrl/⌘+B),
 * remembers the choice, announces it, animates the change (body column
 * and sidebar width over --dur-3, nothing under prefers-reduced-motion), and
 * shows a tooltip with each entry's label on hover/focus while collapsed.
 *
 * Below 1025px the sidebar is nav.js's off-canvas drawer and none of this
 * applies. The sidebar persists across in-place navigations (shell.js), so
 * this runs once per document.
 */
(function () {
  "use strict";

  var html = document.documentElement;
  var KEY = "hfs-sidebar";
  var sidebar = document.getElementById("sidebar");
  var toggle = document.querySelector("[data-sb-toggle]");
  if (!sidebar || !toggle) return;

  var wideQuery = window.matchMedia("(min-width: 1025px)");
  var autoQuery = window.matchMedia("(min-width: 1280px)");
  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
  var animTimer = 0;

  function collapsed() {
    return html.getAttribute("data-sb") === "collapsed";
  }

  function sync() {
    var c = collapsed();
    var label = toggle.getAttribute(c ? "data-label-expand" : "data-label-collapse");
    toggle.setAttribute("aria-expanded", c ? "false" : "true");
    toggle.setAttribute("aria-label", label);
    var text = toggle.querySelector(".nav-item__label");
    if (text) text.textContent = label;
  }

  function announce(msg) {
    var live = document.getElementById("nav-announcer");
    if (!live || !msg) return;
    live.textContent = "";
    setTimeout(function () { live.textContent = msg; }, 30);
  }

  function apply(state, opts) {
    opts = opts || {};
    if (html.getAttribute("data-sb") === state) return;
    hideTip();
    if (opts.animate && !reduced.matches) {
      html.classList.add("sb-anim");
      clearTimeout(animTimer);
      animTimer = setTimeout(function () {
        html.classList.remove("sb-anim");
        window.dispatchEvent(new Event("resize"));
      }, 320);
    }
    html.setAttribute("data-sb", state);
    if (opts.remember) {
      html.removeAttribute("data-sb-auto");
      try {
        window.localStorage.setItem(KEY, state);
      } catch (e) {}
    }
    sync();
    // The current-entry indicator (shell.js) and anything sized from the
    // content width re-measure.
    window.dispatchEvent(new Event("resize"));
    if (opts.announce) announce(toggle.getAttribute(state === "collapsed" ? "data-msg-collapsed" : "data-msg-expanded"));
    var m = window.HM && window.HM.motion;
    if (m && opts.animate && state === "expanded") {
      var labels = sidebar.querySelectorAll(".nav-item__label, .nav__label > span, .brand__text");
      for (var i = 0; i < labels.length; i++) m.fade(labels[i], { duration: m.tokens.d3, delay: 60 });
    }
  }

  function flip(source) {
    if (!wideQuery.matches) return;
    apply(collapsed() ? "expanded" : "collapsed", { remember: true, animate: true, announce: true });
    if (source === "key") return;
  }

  toggle.addEventListener("click", function () { flip("click"); });

  document.addEventListener("keydown", function (event) {
    if (event.defaultPrevented || event.altKey || event.shiftKey) return;
    if (!(event.ctrlKey || event.metaKey) || (event.key !== "b" && event.key !== "B")) return;
    if (!wideQuery.matches) return;
    event.preventDefault();
    flip("key");
  });

  // No stored choice: keep following the width (collapsed below 1280px).
  autoQuery.addEventListener("change", function () {
    if (!html.hasAttribute("data-sb-auto")) return;
    apply(autoQuery.matches ? "expanded" : "collapsed", {});
  });

  // Other tabs.
  window.addEventListener("storage", function (event) {
    if (event.key !== KEY || (event.newValue !== "collapsed" && event.newValue !== "expanded")) return;
    html.removeAttribute("data-sb-auto");
    apply(event.newValue, {});
  });

  /* ---- tooltips while collapsed ------------------------------------------ */

  var tip = document.createElement("div");
  tip.className = "sb-tip";
  tip.setAttribute("role", "tooltip");
  tip.setAttribute("aria-hidden", "true");
  tip.hidden = true;
  document.body.appendChild(tip);
  var tipFor = null;

  function labelOf(el) {
    var l = el.querySelector(".nav-item__label");
    if (l && l.textContent.trim()) return l.textContent.trim();
    return (el.getAttribute("aria-label") || el.textContent || "").trim();
  }

  function showTip(el) {
    if (!collapsed() || !wideQuery.matches) return;
    if (sidebar.querySelector("details[open]")) return;
    var text = labelOf(el);
    if (!text) return;
    tipFor = el;
    tip.textContent = text;
    tip.hidden = false;
    var r = el.getBoundingClientRect();
    var s = sidebar.getBoundingClientRect();
    tip.style.left = Math.round(s.right + 8) + "px";
    tip.style.top = Math.round(r.top + r.height / 2) + "px";
    var m = window.HM && window.HM.motion;
    if (m) m.enter(tip, { dx: -4, duration: m.tokens.d1 });
  }

  function hideTip() {
    tipFor = null;
    tip.hidden = true;
  }

  var TARGETS = "a.nav-item, .sidebar__collapse, .ep-chip, .brand";
  sidebar.addEventListener("mouseover", function (event) {
    var el = event.target.closest && event.target.closest(TARGETS);
    if (!el) hideTip();
    else if (el !== tipFor && sidebar.contains(el)) showTip(el);
  });
  sidebar.addEventListener("mouseleave", hideTip);
  // A click opens a menu (the endpoint switcher) or navigates: the tooltip
  // would sit on top of the menu.
  sidebar.addEventListener("click", hideTip);
  sidebar.addEventListener("focusin", function (event) {
    var el = event.target.closest && event.target.closest(TARGETS);
    if (el) showTip(el);
  });
  sidebar.addEventListener("focusout", hideTip);
  sidebar.addEventListener("scroll", hideTip, true);
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") hideTip();
  });
  document.addEventListener("htmx:beforeSwap", hideTip);

  sync();
})();
