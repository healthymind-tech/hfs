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

  /* ---- motion: the one helper every console animation goes through ------
     Durations and easings are the app.css tokens (120 / 180 / 240 ms;
     standard and emphasized). Transform and opacity only, through the Web
     Animations API; nothing runs under prefers-reduced-motion (each call
     resolves at once, the element in its end state). */
  var MOTION = {
    d1: 120,
    d2: 180,
    d3: 240,
    std: "cubic-bezier(0.4, 0, 0.2, 1)",
    emph: "cubic-bezier(0.2, 0.7, 0.2, 1)",
  };
  function animate(el, frames, opts) {
    if (!el || REDUCED || typeof el.animate !== "function") return Promise.resolve();
    var a = el.animate(frames, {
      duration: (opts && opts.duration) || MOTION.d2,
      easing: (opts && opts.easing) || MOTION.std,
      delay: (opts && opts.delay) || 0,
      fill: "backwards",
    });
    return a.finished.catch(function () {});
  }
  var motion = {
    tokens: MOTION,
    reduced: REDUCED,
    animate: animate,
    // In from `dx`/`dy` px with a fade (emphasized easing).
    enter: function (el, o) {
      o = o || {};
      return animate(el, [
        { opacity: 0, transform: "translate(" + (o.dx || 0) + "px," + (o.dy || 0) + "px)" },
        { opacity: 1, transform: "none" },
      ], { duration: o.duration || MOTION.d2, easing: MOTION.emph, delay: o.delay });
    },
    // Out to `dx`/`dy` px with a fade (standard easing), the end state kept.
    leave: function (el, o) {
      o = o || {};
      if (!el || REDUCED || typeof el.animate !== "function") return Promise.resolve();
      return el.animate([
        { opacity: 1, transform: "none" },
        { opacity: 0, transform: "translate(" + (o.dx || 0) + "px," + (o.dy || 0) + "px)" },
      ], { duration: o.duration || MOTION.d1, easing: MOTION.std, fill: "forwards" }).finished.catch(function () {});
    },
    fade: function (el, o) {
      o = o || {};
      return animate(el, [{ opacity: 0 }, { opacity: 1 }], { duration: o.duration || MOTION.d2, delay: o.delay });
    },
    // Rows fade up one after another: at most `max` rows (20), the whole
    // run within `total` ms (240).
    stagger: function (rows, o) {
      o = o || {};
      var max = Math.min(rows.length, o.max || 20);
      var total = o.total || MOTION.d3;
      var dur = MOTION.d1;
      var step = max > 1 ? Math.max(0, Math.floor((total - dur) / (max - 1))) : 0;
      for (var i = 0; i < max; i++) {
        animate(rows[i], [{ opacity: 0, transform: "translateY(4px)" }, { opacity: 1, transform: "none" }], {
          duration: dur,
          easing: MOTION.emph,
          delay: i * step,
        });
      }
    },
    // Grid tracks from one px list to another (a master-detail column
    // opening or closing while its neighbour gives up or takes back the
    // room). `from`/`to` are arrays of px widths with the same length. The
    // end state is held until the caller's cleanup; `null` under reduced
    // motion, else the Animation.
    tracks: function (el, from, to, o) {
      o = o || {};
      if (!el || REDUCED || typeof el.animate !== "function") return null;
      var px = function (a) { return a.map(function (v) { return Math.max(0, v) + "px"; }).join(" "); };
      return el.animate([{ gridTemplateColumns: px(from) }, { gridTemplateColumns: px(to) }], {
        duration: o.duration || 200,
        easing: o.easing || MOTION.std,
        fill: "forwards",
      });
    },
    // Slide a stand-in of `from`'s box to `to`'s (a selection highlight
    // moving between rail items).
    glide: function (from, to, cls) {
      if (!from || !to || from === to || REDUCED) return;
      var host = to.offsetParent;
      if (!host) return;
      var a = from.getBoundingClientRect(), b = to.getBoundingClientRect(), h = host.getBoundingClientRect();
      var ink = document.createElement("div");
      ink.className = cls || "md-ink";
      ink.setAttribute("aria-hidden", "true");
      ink.style.cssText = "position:absolute;pointer-events:none;left:" + (b.left - h.left + host.scrollLeft) + "px;top:" + (b.top - h.top + host.scrollTop) + "px;width:" + b.width + "px;height:" + b.height + "px;";
      var cs = getComputedStyle(to);
      ink.style.background = cs.backgroundColor;
      ink.style.borderRadius = cs.borderRadius;
      ink.style.zIndex = "0";
      host.appendChild(ink);
      to.classList.add("is-gliding");
      animate(ink, [
        { transform: "translate(" + (a.left - b.left) + "px," + (a.top - b.top) + "px)", opacity: 1 },
        { transform: "none", opacity: 1 },
      ], { duration: MOTION.d2, easing: MOTION.emph }).then(function () {
        to.classList.remove("is-gliding");
        ink.remove();
      });
    },
  };
  window.HM.motion = motion;

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

  /* ---- master-detail: swap the regions, not the page -------------------- */

  var mdPending = null;

  function mdStrip(el) {
    // A region's markup without its selection marks: equal means only the
    // selection moved.
    var c = el.cloneNode(true);
    var marked = c.querySelectorAll("[aria-current],[aria-selected]");
    for (var i = 0; i < marked.length; i++) {
      marked[i].removeAttribute("aria-current");
      marked[i].removeAttribute("aria-selected");
    }
    if (c.removeAttribute) {
      c.removeAttribute("aria-current");
      c.removeAttribute("aria-selected");
    }
    // Links that only carry the selection (`sel=` on a pager) are the same
    // region too: their hrefs move with the marks (mdCopySelection).
    return c.innerHTML.replace(/\s+/g, " ").replace(/[?&](?:amp;)?sel=[^&"]*/g, "");
  }

  function mdSelected(root) {
    return root.querySelector('[aria-current="true"], [aria-current="page"], [aria-selected="true"]');
  }

  function mdCopySelection(cur, nxt) {
    // Move the selection marks in place, element for element.
    var a = cur.querySelectorAll("*"), b = nxt.querySelectorAll("*");
    if (a.length !== b.length) return false;
    for (var i = 0; i < a.length; i++) {
      ["aria-current", "aria-selected", "href"].forEach(function (attr) {
        var v = b[i].getAttribute(attr);
        if (v === null) a[i].removeAttribute(attr);
        else if (a[i].getAttribute(attr) !== v) a[i].setAttribute(attr, v);
      });
    }
    return true;
  }

  function mdIsEmpty(region) {
    return !region || region.hasAttribute("data-md-empty") || !!region.querySelector("[data-md-empty]");
  }

  function mdReplace(cur, nxt) {
    var fresh = document.importNode(nxt, true);
    cur.replaceWith(fresh);
    if (window.htmx) window.htmx.process(fresh);
    initPages(fresh);
    return fresh;
  }

  /* The detail column exists only while something is selected
     (`data-md-open` on the container): it slides in from the right as the
     list gives up the room, and back out as the list takes it again. The
     grid tracks animate from the list's current width, so nothing jumps;
     the panel keeps its final width while its track grows (the container
     clips it), so it slides rather than reflows. */
  var mdResizing = null;

  function mdTracks(md) {
    var v = getComputedStyle(md).gridTemplateColumns || "";
    if (v === "none") return [];
    return v.split(/\s+/).map(parseFloat).filter(function (n) { return !isNaN(n); });
  }

  // While the tracks move, the list's content is laid out once, at the
  // width it ends at (`--md-list-w`, app.css): only the column's edge moves
  // per frame, not 50 rows of wrapping text.
  function mdHoldList(md, width) {
    var list = md.querySelector("[data-md-list]");
    if (!list) return function () {};
    var cs = getComputedStyle(list);
    var inner = width - (parseFloat(cs.paddingLeft) || 0) - (parseFloat(cs.paddingRight) || 0);
    list.style.setProperty("--md-list-w", Math.max(0, inner) + "px");
    return function () { list.style.removeProperty("--md-list-w"); };
  }

  function mdSettle() {
    // Finish a running open/close at once (a new selection arrived).
    if (mdResizing) mdResizing();
    mdResizing = null;
  }

  function mdOpen(md, ndet) {
    var t = mdTracks(md);
    var gap = parseFloat(getComputedStyle(md).columnGap) || 0;
    var fresh = document.importNode(ndet, true);
    md.appendChild(fresh);
    md.setAttribute("data-md-open", "");
    if (window.htmx) window.htmx.process(fresh);
    initPages(fresh);
    var side = t.length === 3;
    if (side) {
      // Closed, the list spans the detail track and the gap before it.
      var from = [t[0], t[1] + gap + t[2], 0];
      fresh.style.width = t[2] + "px";
      var release = mdHoldList(md, t[1]);
      md.classList.add("md-resizing");
      var anim = motion.tracks(md, from, t, { duration: 200, easing: MOTION.emph });
      var end = function () {
        if (anim) anim.cancel();
        fresh.style.width = "";
        release();
        md.classList.remove("md-resizing");
      };
      if (anim) {
        mdResizing = end;
        anim.finished.then(function () { if (mdResizing === end) mdSettle(); }, function () {});
      } else {
        end();
      }
      motion.enter(fresh, { dx: 16, duration: 200 });
    } else {
      // Stacked (narrow): the panel rises under the list; bring it into view.
      motion.enter(fresh, { dy: 8, duration: 200 });
      // A long way down (under a full page of rows) jumps; a short one glides.
      var far = Math.abs(fresh.getBoundingClientRect().top) > window.innerHeight * 1.5;
      if (fresh.scrollIntoView) fresh.scrollIntoView({ block: "start", behavior: REDUCED || far ? "auto" : "smooth" });
    }
    return fresh;
  }

  function mdClose(md, det) {
    var t = mdTracks(md);
    var gap = parseFloat(getComputedStyle(md).columnGap) || 0;
    var release = function () {};
    var end = function () {
      if (anim) anim.cancel();
      if (det.isConnected) det.remove();
      release();
      md.removeAttribute("data-md-open");
      md.classList.remove("md-resizing");
    };
    var anim = null;
    det.setAttribute("inert", "");
    det.removeAttribute("data-md-detail");
    if (t.length === 3 && !REDUCED) {
      det.style.width = t[2] + "px";
      release = mdHoldList(md, t[1] + gap + t[2]);
      md.classList.add("md-resizing");
      anim = motion.tracks(md, t, [t[0], t[1] + gap + t[2], 0], { duration: 200, easing: MOTION.std });
      motion.leave(det, { dx: 16, duration: 200 });
    }
    if (anim) {
      mdResizing = end;
      anim.finished.then(function () { if (mdResizing === end) mdSettle(); }, function () {});
    } else {
      end();
    }
  }

  // The selected row a close (×, Esc, a second click on it) started from:
  // focus goes back to it once the detail has gone.
  var mdCloseRow = null;

  function mdRows(list) {
    return list ? list.querySelectorAll("tbody > tr, [data-md-item]") : [];
  }

  function mdSwap(curMd, nextMd, nextMain) {
    if (!curMd.isConnected) return;
    var done = mdPending;
    mdPending = null;
    if (done) clearTimeout(done.timer);
    mdSettle();
    var main = document.getElementById("main");
    // Regions outside the master-detail container that follow the
    // selection (a page head's counts, a version switch).
    var syncs = nextMain.querySelectorAll("[data-md-sync]");
    for (var k = 0; k < syncs.length; k++) {
      var id = syncs[k].id;
      var here = id && document.getElementById(id);
      if (here) mdReplace(here, syncs[k]);
    }
    // Rail: only the selection moved → move the marks and glide the
    // highlight; otherwise replace it, keeping its scroll position.
    var rail = curMd.querySelector("[data-md-rail]"), nrail = nextMd.querySelector("[data-md-rail]");
    if (rail && nrail) {
      var before = mdSelected(rail);
      if (mdStrip(rail) === mdStrip(nrail) && mdCopySelection(rail, nrail)) {
        motion.glide(before, mdSelected(rail), "md-ink");
      } else {
        var lists = rail.querySelectorAll("[data-md-scroll], .filter-rail__list");
        var tops = [];
        for (var li = 0; li < lists.length; li++) tops.push(lists[li].scrollTop);
        rail = mdReplace(rail, nrail);
        var nlists = rail.querySelectorAll("[data-md-scroll], .filter-rail__list");
        for (var lj = 0; lj < nlists.length && lj < tops.length; lj++) nlists[lj].scrollTop = tops[lj];
      }
    }
    // List: a new list re-renders with a short stagger; a moved selection
    // only moves the marks.
    var list = curMd.querySelector("[data-md-list]"), nlist = nextMd.querySelector("[data-md-list]");
    var closing = !!(curMd.querySelector("[data-md-detail]") && !nextMd.querySelector("[data-md-detail]"));
    var backTo = -1;
    if (closing && list) {
      var from = mdCloseRow && mdCloseRow.isConnected ? mdCloseRow : list.querySelector('[aria-selected="true"]');
      backTo = Array.prototype.indexOf.call(mdRows(list), from);
    }
    mdCloseRow = null;
    if (list && nlist) {
      if (!(mdStrip(list) === mdStrip(nlist) && mdCopySelection(list, nlist))) {
        var wraps = list.querySelectorAll(".table-wrap");
        var wtops = [];
        for (var wi = 0; wi < wraps.length; wi++) wtops.push(wraps[wi].scrollTop);
        list = mdReplace(list, nlist);
        var nwraps = list.querySelectorAll(".table-wrap");
        for (var wj = 0; wj < nwraps.length && wj < wtops.length; wj++) nwraps[wj].scrollTop = wtops[wj];
        var rows = list.querySelectorAll("tbody > tr, [data-md-item]");
        motion.stagger(rows, { max: 20, total: 240 });
      }
    }
    // Detail: slides in from the right when it opens, cross-fades when the
    // item changes, slides out when it closes.
    var det = curMd.querySelector("[data-md-detail]"), ndet = nextMd.querySelector("[data-md-detail]");
    if (!det && ndet) {
      det = mdOpen(curMd, ndet);
      mdFocusHead(det);
    } else if (det && !ndet) {
      mdClose(curMd, det);
      var back = mdRows(list)[backTo];
      var link = back && back.querySelector("a[href]");
      if (link) link.focus();
    } else if (det && ndet) {
      det.classList.remove("md-pending");
      if (det.innerHTML !== ndet.innerHTML) {
        var wasEmpty = mdIsEmpty(det), nowEmpty = mdIsEmpty(ndet);
        var ghost = null;
        if (!REDUCED && !wasEmpty) {
          // The old content, frozen above the new while it leaves.
          var r = det.getBoundingClientRect(), host = det.offsetParent || document.body, h = host.getBoundingClientRect();
          ghost = det.cloneNode(true);
          ghost.removeAttribute("id");
          ghost.removeAttribute("data-md-detail");
          ghost.setAttribute("aria-hidden", "true");
          ghost.setAttribute("inert", "");
          ghost.classList.add("md-ghost");
          ghost.style.cssText += ";position:absolute;margin:0;left:" + (r.left - h.left + host.scrollLeft) + "px;top:" + (r.top - h.top + host.scrollTop) + "px;width:" + r.width + "px;height:" + r.height + "px;overflow:hidden;pointer-events:none;z-index:2;";
          host.appendChild(ghost);
        }
        det = mdReplace(det, ndet);
        if (ghost) {
          var g = ghost;
          motion.leave(g, { dx: nowEmpty ? 14 : 0, duration: nowEmpty ? MOTION.d2 : MOTION.d1 }).then(function () { g.remove(); });
        }
        if (wasEmpty && !nowEmpty) motion.enter(det, { dx: 14, duration: 200 });
        else motion.fade(det, { duration: MOTION.d2 });
        if (!nowEmpty) mdFocusHead(det);
      }
    }
    var announcer = document.getElementById("nav-announcer");
    if (announcer) announcer.textContent = document.title;
    rememberPage();
    document.dispatchEvent(new CustomEvent("hm:navigated", { detail: { main: main, md: true } }));
  }

  function mdFocusHead(det) {
    var head = det.querySelector("h2, h3, [data-md-heading]");
    if (head) {
      if (!head.hasAttribute("tabindex")) head.setAttribute("tabindex", "-1");
      head.focus({ preventScroll: true });
    }
  }

  // Close the open detail (Esc, a second click on the selected row): its ×
  // link, so the URL drops the selection the same way.
  function mdCloseOpen(md) {
    var x = md && md.querySelector("[data-md-detail] [data-md-close]");
    if (!x) return false;
    var list = md.querySelector("[data-md-list]");
    mdCloseRow = list && list.querySelector('[aria-selected="true"]');
    x.click();
    return true;
  }

  document.addEventListener("keydown", function (e) {
    if (e.key !== "Escape" || e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey) return;
    var md = document.querySelector("#main [data-md][data-md-open]");
    if (!md) return;
    var t = e.target;
    var inDetail = t && t.closest && t.closest("[data-md-detail]");
    // Escape in a field, a menu or a dialog is theirs.
    if (!inDetail && t && t.closest && t.closest("input, textarea, select, [contenteditable], [role=combobox], [role=listbox], [role=menu]")) return;
    if (document.querySelector("dialog[open], [aria-modal=true]:not([hidden]), body.nav-open")) return;
    if (mdCloseOpen(md)) e.preventDefault();
  });

  // Immediate feedback (< 100 ms): the clicked entry is selected at once;
  // a response slower than 120 ms dims the detail behind a skeleton.
  document.addEventListener("click", function (e) {
    if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    var a = e.target.closest && e.target.closest("a[href]");
    if (!a) return;
    var md = a.closest("#main [data-md]");
    if (!md || a.hasAttribute("data-md-full")) return;
    if (a.hasAttribute("data-md-close")) {
      var sel = md.querySelector('[data-md-list] [aria-selected="true"]');
      if (sel) mdCloseRow = sel;
      return;
    }
    if (a.closest("[data-md-detail]")) return;
    var region = a.closest("[data-md-rail], [data-md-list]");
    if (!region) return;
    // A second click on the selected row closes the detail.
    var again = region.hasAttribute("data-md-list") && a.closest('[aria-selected="true"]');
    if (again && md.hasAttribute("data-md-open") && md.querySelector("[data-md-detail] [data-md-close]")) {
      e.preventDefault();
      e.stopPropagation();
      mdCloseOpen(md);
      return;
    }
    if (region.hasAttribute("data-md-rail")) {
      var was = mdSelected(region);
      if (was && was !== a && was.getAttribute("aria-current")) {
        was.removeAttribute("aria-current");
        a.setAttribute("aria-current", "true");
        motion.glide(was, a, "md-ink");
      }
    } else {
      var row = a.closest("tr, [data-md-item]");
      if (row) {
        var olds = region.querySelectorAll('[aria-selected="true"]');
        for (var i = 0; i < olds.length; i++) olds[i].removeAttribute("aria-selected");
        row.setAttribute("aria-selected", "true");
      }
    }
    var det = md.querySelector("[data-md-detail]");
    if (det) {
      if (mdPending) clearTimeout(mdPending.timer);
      mdPending = {
        timer: setTimeout(function () {
          if (det.isConnected) det.classList.add("md-pending");
        }, 120),
      };
    }
  }, true);

  /* ---- swap handling ---------------------------------------------------- */

  var parser = new DOMParser();
  var viewTransition = false;
  // List → detail ("forward") and back: the content column enters from the
  // right or the left instead of rising (REDESIGN.md §7, master-detail).
  var navDir = null;
  document.addEventListener("click", function (e) {
    var a = e.target.closest && e.target.closest("a[href]");
    if (!a || !a.closest("#main")) {
      navDir = null;
      return;
    }
    var row = a.closest("table.data-table > tbody > tr");
    if (a.closest(".back-link, [data-drill=back]")) navDir = "back";
    else if (row || a.closest("[data-drill]")) navDir = "forward";
    else navDir = null;
    // The row answers at once (well under 100 ms), before the page does.
    if (row && !a.closest("[data-md]")) row.setAttribute("data-pressed", "");
  }, true);
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
    var scripts = next.querySelectorAll("script[src]");
    var unsafe = false;
    for (var si = 0; si < scripts.length; si++) {
      if (!scripts[si].hasAttribute("data-swap-safe")) unsafe = true;
    }
    if (unsafe) {
      // Page scripts expect a fresh document: load it as one, and go there
      // directly next time.
      detail.shouldSwap = false;
      learn(new URL(url, location.href).pathname);
      go(url);
      return;
    }
    if (scripts.length) {
      // `data-swap-safe` scripts only delegate from `document` (and redo
      // their per-page work on `hm:navigated`): load each once, never twice.
      var stripped = false;
      for (var sj = 0; sj < scripts.length; sj++) {
        var src = scripts[sj].getAttribute("src");
        if (document.querySelector('script[src="' + src + '"]')) {
          scripts[sj].remove();
          stripped = true;
        }
      }
      if (stripped) detail.serverResponse = "<!doctype html>" + doc.documentElement.outerHTML;
    }
    // Master-detail pages: the same view under a new selection swaps only
    // the regions that changed (rail, list, detail), with their own motion;
    // the rest of #main, the scroll positions and the focus stay.
    var curMd = document.querySelector("#main [data-md]");
    var nextMd = next.querySelector("[data-md]");
    if (curMd && nextMd && curMd.getAttribute("data-md") === nextMd.getAttribute("data-md") && !(detail.requestConfig && detail.requestConfig.elt && detail.requestConfig.elt.hasAttribute && detail.requestConfig.elt.hasAttribute("data-md-full"))) {
      detail.swapOverride = "none";
      detail.selectOverride = "#main";
      detail.target = document.getElementById("main");
      detail.shouldSwap = true;
      detail.isError = false;
      var title = doc.title;
      setTimeout(function () {
        mdSwap(curMd, nextMd, next);
        if (title) document.title = title;
      }, 0);
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
    if (navDir) document.documentElement.setAttribute("data-nav-dir", navDir);
    else document.documentElement.removeAttribute("data-nav-dir");
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
    var dir = navDir;
    navDir = null;
    setTimeout(function () {
      if (!navDir) document.documentElement.removeAttribute("data-nav-dir");
    }, 400);
    if (main && !viewTransition && !REDUCED) {
      main.classList.remove("page-enter");
      void main.offsetWidth;
      if (dir) main.setAttribute("data-enter", dir);
      else main.removeAttribute("data-enter");
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

  // A short confirmation that rises in at the bottom right and leaves.
  function toast(message) {
    var t = document.createElement("div");
    t.className = "toast";
    t.setAttribute("role", "status");
    t.textContent = message;
    document.body.appendChild(t);
    setTimeout(function () {
      t.classList.add("is-leaving");
      setTimeout(function () {
        t.remove();
      }, 200);
    }, 2000);
  }
  window.HM.toast = toast;

  // The endpoint switcher keeps the page: each option asks the server for
  // the same sub-page on its endpoint (`manager::switch`), from wherever the
  // operator is now — the persistent sidebar's own hrefs cannot know that.
  // The hrefs are set when the menu opens (htmx re-reads a boosted link's
  // href when its attributes change and it is processed again).
  document.addEventListener("toggle", function (e) {
    var menu = e.target;
    if (!menu.open || !menu.querySelector || !menu.querySelector("a[data-switch-href]")) return;
    var from = encodeURIComponent(location.pathname + location.search);
    var links = menu.querySelectorAll("a[data-switch-href]");
    for (var i = 0; i < links.length; i++) {
      links[i].setAttribute("href", links[i].getAttribute("data-switch-href") + "?from=" + from);
    }
    if (window.htmx) window.htmx.process(menu);
  }, true);

  // A server-side notice for the next page (`hm_toast` cookie, e.g. the
  // switcher's "not on this endpoint" fallback): shown once.
  function cookieToast() {
    var m = document.cookie.match(/(?:^|;\s*)hm_toast=([^;]*)/);
    if (!m) return;
    document.cookie = "hm_toast=; Path=/ui; Max-Age=0; SameSite=Lax";
    var text = "";
    try {
      text = decodeURIComponent(m[1].replace(/\+/g, " "));
    } catch (err) {}
    if (text) toast(text);
  }
  document.addEventListener("hm:navigated", cookieToast);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", cookieToast);
  else cookieToast();

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
      // The ink under the chosen tab grows only when the operator picks it.
      tabs[k].classList.toggle("ink-in", on && !!animate);
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
    // Log viewer: wrap toggle and copy.
    var wrap = e.target.closest && e.target.closest("[data-log-wrap]");
    if (wrap) {
      var log = document.getElementById(wrap.getAttribute("aria-controls"));
      var wrapped = wrap.getAttribute("aria-pressed") !== "true";
      wrap.setAttribute("aria-pressed", wrapped ? "true" : "false");
      if (log) log.classList.toggle("log--nowrap", !wrapped);
      return;
    }
    var copyLog = e.target.closest && e.target.closest("[data-log-copy]");
    if (copyLog) {
      var src = document.getElementById(copyLog.getAttribute("data-log-copy"));
      if (src && navigator.clipboard) {
        navigator.clipboard.writeText(src.textContent || "").then(function () {
          toast(document.body.getAttribute("data-msg-copied") || "Copied");
        }, function () {});
      }
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
      // Browsers list one row per canonical; its other versions are
      // fetched once, on the first expand, and inserted under it.
      var src = tog.getAttribute("data-versions-src");
      if (open && src && !tog.hasAttribute("data-loaded")) {
        if (tog.getAttribute("aria-busy") === "true") return;
        tog.setAttribute("aria-busy", "true");
        fetch(src, { headers: { "HX-Request": "true" }, credentials: "same-origin" })
          .then(function (r) { return r.text(); })
          .then(function (html) {
            var doc = new DOMParser().parseFromString(html, "text/html");
            var rows = doc.querySelectorAll("tr[data-older]");
            var at = row;
            for (var i = 0; i < rows.length; i++) {
              var tr = document.importNode(rows[i], true);
              at.after(tr);
              at = tr;
            }
            if (window.htmx) for (var j = row.nextElementSibling; j && j.hasAttribute("data-older"); j = j.nextElementSibling) window.htmx.process(j);
            tog.setAttribute("data-loaded", "");
            tog.removeAttribute("aria-busy");
            tog.click();
          })
          .catch(function () { tog.removeAttribute("aria-busy"); });
        return;
      }
      tog.setAttribute("aria-expanded", open ? "true" : "false");
      var opened = [];
      for (var r = row.nextElementSibling; r && r.hasAttribute("data-older"); r = r.nextElementSibling) {
        r.classList.toggle("is-open", open);
        if (open) opened.push(r);
      }
      motion.stagger(opened, { max: 20, total: 240 });
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
  // Table rows fade up, staggered (<= 20 rows, <= 240 ms), when a filter
  // or a pager swaps them in — never on first paint, where the content
  // column's own rise is the only motion.
  function staggerRows(root) {
    if (REDUCED) return;
    var bodies = (root || document).querySelectorAll("table.data-table > tbody");
    for (var b = 0; b < bodies.length; b++) {
      if (bodies[b].rows.length > 400) continue;
      motion.stagger(bodies[b].rows, { max: 20, total: 240 });
    }
  }
  document.addEventListener("hm:navigated", function (e) {
    var main = e.detail && e.detail.main;
    initPageTabs(main);
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

  // A filter that matches only an older version keeps its group: that
  // version is shown, marked, under the listed one.
  function autoExpand(root) {
    var t = (root || document).querySelectorAll("[data-versions-toggle][data-autoexpand]:not([data-loaded])");
    for (var i = 0; i < t.length && i < 25; i++) t[i].click();
  }
  document.addEventListener("hm:navigated", function (e) {
    autoExpand(e.detail && e.detail.main);
  });
  document.addEventListener("htmx:afterSettle", function (e) {
    var t = e.detail && e.detail.target;
    if (t && t.querySelector) autoExpand(document.getElementById("main"));
  });

  function start() {
    autoExpand(document.getElementById("main"));
    rememberPage();
    restoreSidebarScroll();
    markActive();
    initPages(document.getElementById("main"));
    initPageTabs(document.getElementById("main"));
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
