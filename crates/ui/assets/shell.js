/*
 * The persistent console shell (unified-server design §9.4).
 *
 * Every same-origin console link is boosted by htmx (`hx-boost`
 * on <body>). This script turns those boosted requests into an in-place
 * navigation that replaces only `#main`: the sidebar and topbar DOM — their
 * scroll position, open menus and listeners — stay exactly as they are, the
 * URL and title update, Back/Forward work (htmx history, snapshots of
 * `#main` only), and a slow page shows a 2 px progress bar after 150 ms and
 * dims the content after 400 ms instead of a blank frame.
 *
 * Pages whose own scripts are not lifecycle-clean yet (a `<script src>` in
 * their content, or a nav entry marked `data-full-load`) load as a full
 * document; `app.css` declares a cross-document view transition, so in
 * Chromium even those keep the shell on screen until the new page paints,
 * and the sidebar scroll position is carried over in sessionStorage.
 *
 * Forms are never boosted (they submit natively, a full page load): saves,
 * deletes, downloads and script-handled forms behave exactly as before.
 *
 * Page scripts can register with `HM.page(name, {init(root), destroy()})`;
 * `init` runs for every `[data-page=name]` element on load and after each
 * swap, `destroy` before that element leaves the page. Global scripts that
 * prefer an event listen for `hm:navigated` (detail: {main}).
 */
(function () {
  "use strict";

  var PROGRESS_DELAY = 150;
  var SKELETON_DELAY = 400;
  var FULL_LOAD_KEY = "hm-full-load";
  var SIDEBAR_SCROLL_KEY = "hm-sidebar-scroll";

  var buildMeta = document.querySelector('meta[name="hm-build"]');
  var build = buildMeta ? buildMeta.getAttribute("content") : "";

  /* ---- page-script registry ------------------------------------------- */

  var pages = {};
  var live = [];

  function initPages(root) {
    if (!root || !root.querySelectorAll) return;
    var nodes = [];
    if (root.matches && root.matches("[data-page]")) nodes.push(root);
    var found = root.querySelectorAll("[data-page]");
    for (var i = 0; i < found.length; i++) nodes.push(found[i]);
    nodes.forEach(function (el) {
      var def = pages[el.getAttribute("data-page")];
      if (!def || el.__hmPage) return;
      el.__hmPage = true;
      try {
        if (def.init) def.init(el);
      } catch (e) {
        if (window.console) console.error(e);
      }
      live.push({ el: el, def: def });
    });
  }

  function destroyPages(scope) {
    live = live.filter(function (entry) {
      if (scope && !scope.contains(entry.el)) return true;
      try {
        if (entry.def.destroy) entry.def.destroy(entry.el);
      } catch (e) {
        if (window.console) console.error(e);
      }
      return false;
    });
  }

  window.HM = window.HM || {};
  window.HM.page = function (name, def) {
    pages[name] = def;
    if (document.readyState !== "loading") initPages(document.getElementById("main"));
  };

  /* ---- what may be swapped in place ----------------------------------- */

  function learned() {
    try {
      return JSON.parse(sessionStorage.getItem(FULL_LOAD_KEY) || "[]");
    } catch (e) {
      return [];
    }
  }

  function pattern(path) {
    return path.replace(/^\/ui\/e\/[^/]+/, "/ui/e/*").replace(/\/$/, "");
  }

  function learn(path) {
    var key = pattern(path);
    var list = learned();
    if (list.indexOf(key) >= 0) return;
    list.push(key);
    try {
      sessionStorage.setItem(FULL_LOAD_KEY, JSON.stringify(list.slice(-200)));
    } catch (e) {}
  }

  /* Never in place: other origins, other apps on this origin (/fhir, /tx,
     /admin), sign-in and sign-out, language switches (cookies and <html
     lang>), downloads. */
  var NEVER = /^\/ui\/(login|logout|callback|admin\/login|admin\/logout|console\/)/;

  function fullLoad(url, elt) {
    if (url.origin !== location.origin) return true;
    if (url.pathname !== "/ui" && url.pathname.indexOf("/ui/") !== 0) return true;
    if (url.pathname.indexOf("/ui/assets/") === 0) return true;
    if (NEVER.test(url.pathname)) return true;
    if (url.searchParams.has("lang")) return true;
    if (/\/download(\/|$)|\.(ndjson|json|csv|zip|parquet|tgz)$/i.test(url.pathname)) return true;
    if (elt && elt.closest && elt.closest("[data-full-load]")) return true;
    // A link to a fragment of another page ("Go to field" on the IdP and
    // Verify reports): the in-place swap scrolls to the top and the
    // response URL has no fragment, so the target field would be lost.
    if (url.hash.length > 1 && (url.pathname !== location.pathname || url.search !== location.search)) return true;
    if (elt && elt.hasAttribute && (elt.hasAttribute("download") || elt.getAttribute("target") === "_blank")) return true;
    return learned().indexOf(pattern(url.pathname)) >= 0;
  }

  function explicitHx(elt) {
    var verbs = ["hx-get", "hx-post", "hx-put", "hx-patch", "hx-delete"];
    for (var i = 0; i < verbs.length; i++) {
      if (elt.hasAttribute(verbs[i]) || elt.hasAttribute("data-" + verbs[i])) return true;
    }
    return false;
  }

  function isBoosted(detail) {
    var cfg = detail && detail.requestConfig;
    if (cfg && typeof cfg.boosted === "boolean") return cfg.boosted;
    var elt = detail && detail.elt;
    return !!(elt && (elt.tagName === "A" || elt.tagName === "FORM") && !explicitHx(elt));
  }

  function nativeSubmit(form, submitter) {
    if (submitter && submitter.name) {
      var carry = document.createElement("input");
      carry.type = "hidden";
      carry.name = submitter.name;
      carry.value = submitter.value;
      form.appendChild(carry);
    }
    if (window.HfsUnsaved) window.HfsUnsaved.suspend();
    saveSidebarScroll();
    HTMLFormElement.prototype.submit.call(form);
  }

  function go(url) {
    if (window.HfsUnsaved) window.HfsUnsaved.suspend();
    saveSidebarScroll();
    location.assign(url);
  }

  /* ---- progress --------------------------------------------------------- */

  var pending = null;

  function startProgress() {
    stopProgress();
    var bar = document.getElementById("nav-progress");
    var main = document.getElementById("main");
    var state = { timers: [] };
    state.timers.push(
      setTimeout(function () {
        if (bar) bar.classList.add("is-active");
      }, PROGRESS_DELAY)
    );
    state.timers.push(
      setTimeout(function () {
        var m = document.getElementById("main");
        if (m) m.classList.add("is-navigating");
      }, SKELETON_DELAY)
    );
    if (main) main.setAttribute("aria-busy", "true");
    pending = state;
  }

  function stopProgress() {
    if (!pending) return;
    pending.timers.forEach(clearTimeout);
    pending = null;
    var bar = document.getElementById("nav-progress");
    if (bar) bar.classList.remove("is-active");
    var main = document.getElementById("main");
    if (main) {
      main.classList.remove("is-navigating");
      main.removeAttribute("aria-busy");
    }
  }

  /* ---- active entry ----------------------------------------------------- */

  var ALIASES = { editor: "resources", history: "resources", queries: "resources", search: "resources", home: "ep-home" };

  // Sidebar entries may stand for several pages (a group whose pages are
  // route tabs): `data-nav-ids` lists their active-page ids and
  // `data-match` their path prefixes, space-separated.
  function markActive() {
    var nav = document.getElementById("sidebar-nav");
    if (!nav) return;
    var links = nav.querySelectorAll("a.nav-item");
    var main = document.getElementById("main");
    var active = main ? main.getAttribute("data-active-page") || "" : "";
    var chosen = null;
    if (active) {
      var want = ALIASES[active] || active;
      for (var a = 0; a < links.length && !chosen; a++) {
        var ids = (links[a].getAttribute("data-nav-ids") || links[a].getAttribute("data-nav-id") || "").split(" ");
        if (ids.indexOf(want) >= 0) chosen = links[a];
      }
    }
    if (!chosen) {
      var path = location.pathname.replace(/\/$/, "") || "/ui";
      var best = -1;
      for (var i = 0; i < links.length; i++) {
        var ms = (links[i].getAttribute("data-match") || "").split(" ");
        for (var k = 0; k < ms.length; k++) {
          var m = ms[k];
          if (!m) continue;
          var hit = m === "/ui" ? path === "/ui" : path === m || path.indexOf(m + "/") === 0;
          if (hit && m.length > best) {
            best = m.length;
            chosen = links[i];
          }
        }
      }
    }
    for (var j = 0; j < links.length; j++) {
      if (links[j] === chosen) links[j].setAttribute("aria-current", "page");
      else if (links[j].getAttribute("aria-current") === "page") links[j].removeAttribute("aria-current");
    }
    placeIndicator(true);
  }

  /* ---- the sliding current-entry fill (REDESIGN.md §5.2, §7) ----------- */

  var REDUCED = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  // One element under the entries of the sidebar's <nav>, moved to the
  // current entry: it slides (240 ms) on in-place navigation instead of the
  // highlight jumping. Entries outside the <nav> (the sidebar foot) and
  // pages without JavaScript fill the entry itself (app.css).
  function placeIndicator(animate) {
    var nav = document.querySelector("#sidebar-nav .nav");
    if (!nav) return;
    var ind = nav.querySelector(":scope > .nav__indicator");
    if (!ind) {
      ind = document.createElement("div");
      ind.className = "nav__indicator";
      ind.setAttribute("aria-hidden", "true");
      nav.insertBefore(ind, nav.firstChild);
      animate = false;
    }
    var cur = nav.querySelector('a.nav-item[aria-current="page"]');
    if (!cur || !cur.offsetHeight) {
      ind.classList.remove("is-placed");
      return;
    }
    var move = animate && !REDUCED && ind.classList.contains("is-placed");
    if (!move) ind.style.transition = "none";
    ind.style.height = cur.offsetHeight + "px";
    ind.style.transform = "translateY(" + cur.offsetTop + "px)";
    ind.classList.add("is-placed");
    if (!move) {
      void ind.offsetWidth;
      ind.style.transition = "";
    }
  }
  window.addEventListener("resize", function () {
    placeIndicator(false);
  });

  /* ---- sidebar scroll across full loads --------------------------------- */

  function saveSidebarScroll() {
    var side = document.getElementById("sidebar");
    if (!side) return;
    try {
      sessionStorage.setItem(SIDEBAR_SCROLL_KEY, String(side.scrollTop));
    } catch (e) {}
  }

  function restoreSidebarScroll() {
    var side = document.getElementById("sidebar");
    if (!side) return;
    try {
      var v = sessionStorage.getItem(SIDEBAR_SCROLL_KEY);
      if (v !== null) side.scrollTop = parseInt(v, 10) || 0;
    } catch (e) {}
  }

  /* ---- swap handling ---------------------------------------------------- */

  var parser = new DOMParser();
  var viewTransition = false;
  var nextNav = null;
  var navigated = false;

  function panel(message, retryUrl) {
    var main = document.getElementById("main");
    if (!main) return;
    var data = document.body.dataset;
    main.innerHTML = "";
    var box = document.createElement("div");
    box.className = "error-page";
    box.id = "nav-error";
    var h = document.createElement("h1");
    h.className = "page-head__title";
    h.tabIndex = -1;
    h.textContent = data.msgNavError || "This page could not be loaded.";
    var p = document.createElement("p");
    p.className = "error-page__text";
    p.textContent = message;
    var actions = document.createElement("div");
    actions.className = "error-page__actions";
    var retry = document.createElement("a");
    retry.className = "btn btn--primary";
    retry.href = retryUrl;
    retry.textContent = data.msgNavRetry || "Retry";
    actions.appendChild(retry);
    box.appendChild(h);
    box.appendChild(p);
    box.appendChild(actions);
    main.appendChild(box);
    h.focus();
  }

  // Forms are never boosted: their pages' scripts handle many of them
  // (preventDefault + fetch), and a boosted submit would race those
  // handlers. A form without its own hx-* submits natively (a full page,
  // kept on screen by the view transition); links are what navigate in
  // place.
  document.addEventListener("htmx:beforeProcessNode", function (event) {
    var elt = event.detail && event.detail.elt;
    if (!elt || !elt.querySelectorAll) return;
    var forms = elt.tagName === "FORM" ? [elt] : elt.querySelectorAll("form");
    for (var i = 0; i < forms.length; i++) {
      if (!forms[i].hasAttribute("hx-boost") && !explicitHx(forms[i])) forms[i].setAttribute("hx-boost", "false");
    }
  });

  document.addEventListener("htmx:configRequest", function (event) {
    var headers = event.detail && event.detail.headers;
    if (!headers) return;
    if (build) headers["X-HM-Build"] = build;
    var key = document.body.getAttribute("data-nav-key");
    if (key) headers["X-HM-Nav"] = key;
  });

  document.addEventListener("htmx:beforeRequest", function (event) {
    var detail = event.detail || {};
    if (!isBoosted(detail)) return;
    var elt = detail.elt;
    var cfg = detail.requestConfig || {};
    var verb = String(cfg.verb || "get").toLowerCase();
    if (elt && elt.tagName === "FORM" && verb !== "get") {
      event.preventDefault();
      var submitter = cfg.triggeringEvent && cfg.triggeringEvent.submitter;
      nativeSubmit(elt, submitter);
      return;
    }
    var href = elt && elt.tagName === "A" ? elt.href : cfg.path;
    var url;
    try {
      url = new URL(href, location.href);
    } catch (e) {
      return;
    }
    if (elt && elt.tagName === "FORM") {
      try {
        url.search = new URLSearchParams(new FormData(elt, cfg.triggeringEvent && cfg.triggeringEvent.submitter)).toString();
      } catch (e) {}
    }
    if (fullLoad(url, elt)) {
      event.preventDefault();
      go(url.href);
      return;
    }
    // A request the old page still has in flight (a filter that fired on
    // blur when the link was clicked) is dropped when it answers after the
    // new page is in (see the beforeSwap guard below).
    startProgress();
    var xhr = detail.xhr;
    if (xhr) xhr.addEventListener("loadend", stopProgress);
  });

  // The answer to a request whose element has left the page (a filter that
  // fired on blur as the user clicked a link) must not touch the history:
  // its own events no longer reach this document, but this one is fired on
  // <body>, and htmx reads `history.type` back after it.
  document.body.addEventListener("htmx:beforeHistoryUpdate", function (event) {
    var detail = event.detail || {};
    var cfg = detail.requestConfig || {};
    var gone = function (el) { return el && el.isConnected === false; };
    if (detail.history && (gone(detail.elt) || gone(cfg.elt) || gone(detail.target))) {
      // htmx then pushes or replaces `history.path`: make it a no-op
      // replace of the current entry.
      detail.history.type = "replace";
      detail.history.path = location.pathname + location.search + location.hash;
    }
  });

  document.addEventListener("htmx:beforeSwap", function (event) {
    var detail = event.detail || {};
    if (!isBoosted(detail)) {
      // A fragment for a page that is gone: drop it (no swap, no history).
      var src = detail.elt;
      if ((src && !src.isConnected) || (detail.target && !detail.target.isConnected)) {
        detail.shouldSwap = false;
      }
      return;
    }
    var xhr = detail.xhr;
    var url = (xhr && xhr.responseURL) || (detail.pathInfo && detail.pathInfo.responsePath) || location.href;
    var type = xhr ? xhr.getResponseHeader("Content-Type") || "" : "";
    if (type && type.indexOf("text/html") < 0) {
      detail.shouldSwap = false;
      go(url);
      return;
    }
    var doc = parser.parseFromString(detail.serverResponse || "", "text/html");
    var next = doc.getElementById("main");
    if (!next) {
      // A sign-in page, a stand-alone message, or no page at all.
      detail.shouldSwap = false;
      if (xhr && xhr.status >= 400 && !/<html/i.test(detail.serverResponse || "")) {
        panel(xhr.status + " " + (xhr.statusText || ""), url);
      } else {
        go(url);
      }
      return;
    }
    if (next.querySelector("script[src]")) {
      // Page scripts expect a fresh document: load it as one, and go there
      // directly next time.
      detail.shouldSwap = false;
      learn(new URL(url, location.href).pathname);
      go(url);
      return;
    }
    var key = doc.body ? doc.body.getAttribute("data-nav-key") : null;
    nextNav = null;
    if (key && key !== document.body.getAttribute("data-nav-key")) {
      nextNav = { key: key, nav: doc.getElementById("sidebar-nav") };
    }
    detail.shouldSwap = true;
    detail.isError = false;
    detail.target = document.getElementById("main");
    detail.selectOverride = "#main";
    // The content column rises in (REDESIGN.md §7): a view transition
    // around the swap where the browser has one (app.css animates the
    // `hm-main` snapshot only — the sidebar and topbar hold still), else a
    // CSS entrance on the new #main after the swap.
    viewTransition = !REDUCED && typeof document.startViewTransition === "function";
    detail.swapOverride = "outerHTML show:window:top" + (viewTransition ? " transition:true" : "");
    navigated = {
      title: doc.title,
      bodyClass: doc.body ? doc.body.getAttribute("class") || "" : "",
      method: String((detail.requestConfig || {}).verb || "get"),
    };
    destroyPages(document.getElementById("main"));
  });

  document.addEventListener("htmx:afterSettle", function (event) {
    if (!navigated) return;
    var info = navigated;
    navigated = false;
    var main = document.getElementById("main");
    if (nextNav) {
      var current = document.getElementById("sidebar-nav");
      if (current && nextNav.nav) current.replaceWith(document.importNode(nextNav.nav, true));
      document.body.setAttribute("data-nav-key", nextNav.key);
      if (window.htmx) window.htmx.process(document.getElementById("sidebar-nav"));
      nextNav = null;
    }
    if (info.title) document.title = info.title;
    // Menus opened to navigate (endpoint switcher, account) close, as they
    // would on a fresh page.
    var menus = document.querySelectorAll("#sidebar details.menu[open], .topbar details[open]");
    for (var k = 0; k < menus.length; k++) menus[k].open = false;
    // Page-level body classes (`app-shell` on full-height workspaces).
    var keep = document.body.classList.contains("nav-open") ? " nav-open" : "";
    document.body.setAttribute("class", (info.bodyClass + keep).trim());
    markActive();
    if (main && !viewTransition && !REDUCED) {
      main.classList.remove("page-enter");
      void main.offsetWidth;
      main.classList.add("page-enter");
      main.addEventListener("animationend", function done(e) {
        if (e.target !== main) return;
        main.classList.remove("page-enter");
        main.removeEventListener("animationend", done);
      });
    }
    initPages(main);
    var heading = main && main.querySelector("h1");
    if (heading) {
      if (!heading.hasAttribute("tabindex")) heading.setAttribute("tabindex", "-1");
      heading.focus({ preventScroll: true });
    } else if (main) {
      main.focus({ preventScroll: true });
    }
    var announcer = document.getElementById("nav-announcer");
    if (announcer) announcer.textContent = document.title;
    rememberPage();
    document.dispatchEvent(new CustomEvent("hm:navigated", { detail: { main: main } }));
  });

  // A list filter that pushes its URL is the same page under a new address.
  document.addEventListener("htmx:pushedIntoHistory", function () {
    if (!navigated) rememberPage();
  });
  document.addEventListener("htmx:replacedInHistory", function () {
    if (!navigated) rememberPage();
  });

  document.addEventListener("htmx:responseError", function (event) {
    var detail = event.detail || {};
    if (!isBoosted(detail) || navigated) return;
    stopProgress();
  });

  document.addEventListener("htmx:sendError", function (event) {
    var detail = event.detail || {};
    if (!isBoosted(detail)) return;
    stopProgress();
    var cfg = detail.requestConfig || {};
    var data = document.body.dataset;
    panel(data.msgNavOffline || "The server could not be reached.", cfg.path || location.href);
  });

  // What belongs to a page but is not inside the #main snapshot htmx keeps:
  // body classes, the active entry, the sidebar context. And form values,
  // which innerHTML does not carry: written into attributes first, so Back
  // shows the filter the list was filtered by.
  var pageState = {};
  function rememberPage() {
    var main = document.getElementById("main");
    pageState[location.href] = {
      bodyClass: document.body.getAttribute("class") || "",
      active: main ? main.getAttribute("data-active-page") || "" : "",
      navKey: document.body.getAttribute("data-nav-key") || "",
    };
  }
  document.addEventListener("htmx:beforeHistorySave", function () {
    var main = document.getElementById("main");
    if (main) {
      var fields = main.querySelectorAll("input, select, textarea");
      for (var i = 0; i < fields.length; i++) {
        var el = fields[i];
        if (el.type === "password" || el.type === "file") continue;
        if (el.tagName === "SELECT") {
          for (var o = 0; o < el.options.length; o++) {
            if (el.options[o].selected) el.options[o].setAttribute("selected", "");
            else el.options[o].removeAttribute("selected");
          }
        } else if (el.type === "checkbox" || el.type === "radio") {
          if (el.checked) el.setAttribute("checked", "");
          else el.removeAttribute("checked");
        } else if (el.tagName === "TEXTAREA") {
          el.textContent = el.value;
        } else {
          el.setAttribute("value", el.value);
        }
      }
    }
    // Not `rememberPage()` here: htmx also saves on popstate, when
    // `location` already names the page being restored.
  });

  document.addEventListener("htmx:historyRestore", function () {
    var state = pageState[location.href];
    var main = document.getElementById("main");
    if (state) {
      document.body.setAttribute("class", state.bodyClass);
      if (main) main.setAttribute("data-active-page", state.active);
      if (state.navKey && state.navKey !== document.body.getAttribute("data-nav-key")) {
        // The page belongs to another sidebar (another endpoint or role).
        location.reload();
        return;
      }
    } else if (main) {
      // Restored from the server: its own active entry is unknown here.
      main.removeAttribute("data-active-page");
    }
    markActive();
    initPages(main);
    document.dispatchEvent(new CustomEvent("hm:navigated", { detail: { main: main } }));
  });

  window.addEventListener("pagehide", saveSidebarScroll);

  /* ---- shared page components (console redesign §6, §7) ---------------- */

  // In-page tabs ([data-page-tabs]): one panel visible at a time; without
  // JavaScript every panel shows. The active tab survives in the hash.
  function initPageTabs(root) {
    var lists = (root || document).querySelectorAll("[data-page-tabs]");
    for (var i = 0; i < lists.length; i++) {
      var list = lists[i];
      if (list.__hmTabs) continue;
      list.__hmTabs = true;
      var tabs = list.querySelectorAll("[role=tab]");
      var want = location.hash.slice(1);
      var pick = null;
      for (var t = 0; t < tabs.length; t++) {
        if (want && tabs[t].getAttribute("aria-controls") === want) pick = tabs[t];
      }
      selectTab(list, pick || list.querySelector('[role=tab][aria-selected="true"]') || tabs[0], false);
    }
  }
  function selectTab(list, tab, animate) {
    var tabs = list.querySelectorAll("[role=tab]");
    for (var k = 0; k < tabs.length; k++) {
      var on = tabs[k] === tab;
      tabs[k].setAttribute("aria-selected", on ? "true" : "false");
      tabs[k].tabIndex = on ? 0 : -1;
      var panel = document.getElementById(tabs[k].getAttribute("aria-controls"));
      if (!panel) continue;
      panel.hidden = !on;
      if (on && animate && !REDUCED) {
        panel.classList.remove("page-enter");
        void panel.offsetWidth;
        panel.classList.add("page-enter");
      }
    }
  }
  document.addEventListener("click", function (e) {
    var tab = e.target.closest && e.target.closest("[data-page-tabs] [role=tab]");
    if (tab) {
      selectTab(tab.closest("[data-page-tabs]"), tab, true);
      try {
        history.replaceState(history.state, "", "#" + tab.getAttribute("aria-controls"));
      } catch (err) {}
      return;
    }
    // Type rails: types without data stay folded until asked for.
    var empty = e.target.closest && e.target.closest("[data-rail-empty-toggle]");
    if (empty) {
      var rail = empty.closest(".filter-rail");
      var on = empty.getAttribute("aria-pressed") !== "true";
      empty.setAttribute("aria-pressed", on ? "true" : "false");
      if (rail) rail.classList.toggle("show-empty", on);
      return;
    }
    // Grouped versions: "+N older" folds the older rows of one system.
    var tog = e.target.closest && e.target.closest("[data-versions-toggle]");
    if (tog) {
      var row = tog.closest("tr");
      var open = tog.getAttribute("aria-expanded") !== "true";
      tog.setAttribute("aria-expanded", open ? "true" : "false");
      var n = 0;
      for (var r = row.nextElementSibling; r && r.hasAttribute("data-older"); r = r.nextElementSibling) {
        r.classList.toggle("is-open", open);
        if (open && !REDUCED) {
          r.style.animationDelay = Math.min(n, 14) * 30 + "ms";
          r.classList.remove("row-in");
          void r.offsetWidth;
          r.classList.add("row-in");
        }
        n++;
      }
    }
  });
  document.addEventListener("keydown", function (e) {
    var tab = e.target.closest && e.target.closest("[data-page-tabs] [role=tab]");
    if (!tab || (e.key !== "ArrowRight" && e.key !== "ArrowLeft")) return;
    var list = tab.closest("[data-page-tabs]");
    var tabs = Array.prototype.slice.call(list.querySelectorAll("[role=tab]"));
    var i = tabs.indexOf(tab) + (e.key === "ArrowRight" ? 1 : -1);
    var next = tabs[(i + tabs.length) % tabs.length];
    selectTab(list, next, true);
    next.focus();
    e.preventDefault();
  });
  // Table rows rise in, staggered, on first render and after a filter swap.
  function staggerRows(root) {
    if (REDUCED) return;
    var bodies = (root || document).querySelectorAll("table.data-table > tbody");
    for (var b = 0; b < bodies.length; b++) {
      var rows = bodies[b].rows;
      if (rows.length > 400) continue;
      for (var i = 0; i < rows.length && i < 40; i++) {
        rows[i].style.animationDelay = Math.min(i, 14) * 18 + "ms";
        rows[i].classList.add("row-in");
      }
    }
  }
  document.addEventListener("hm:navigated", function (e) {
    var main = e.detail && e.detail.main;
    initPageTabs(main);
    staggerRows(main);
  });
  document.addEventListener("htmx:afterSwap", function (e) {
    var t = e.detail && e.detail.target;
    if (t && t.id !== "main" && t.querySelector) {
      // An outerHTML swap leaves `target` detached: use its replacement.
      var live = (t.id && document.getElementById(t.id)) || t;
      initPageTabs(live);
      staggerRows(live);
    }
  });

  function start() {
    rememberPage();
    restoreSidebarScroll();
    markActive();
    initPages(document.getElementById("main"));
    initPageTabs(document.getElementById("main"));
    staggerRows(document.getElementById("main"));
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
