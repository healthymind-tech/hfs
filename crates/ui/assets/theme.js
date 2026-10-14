// Theme selection for the HFS and HTS web consoles (one file, served to both
// from the shared asset embed).
//
// Three preferences: "system" (follow `prefers-color-scheme`, live — the
// default for a first visit), "light" and "dark". The topbar carries ONE
// button (`[data-theme-cycle]`, markup from `helios-ui-chrome`) that cycles
// system -> light -> dark -> system; its icon follows `<html
// data-theme-pref>` in CSS and its aria-label/title are swapped in here.
//
// First paint is NOT this file's job any more: the inline boot script from
// `helios_ui_chrome::theme_boot()` stamps `data-theme-pref` and the effective
// `data-theme` on <html> before anything renders, so this script loads with
// `defer` and only wires behaviour.
//
// Where the choice lives (one source of truth per browser, shared by both
// consoles):
//
//   1. The `hfs_theme` cookie (`helios_ui_chrome::THEME_COOKIE`), `Path=/`:
//      HFS is mounted at /ui and HTS at /ui/hts on one origin, so both read
//      the same cookie, exactly like the shared `hfs_lang` language cookie.
//   2. Migration: the localStorage key `hfs-theme` (light | dark) both
//      consoles used before the cookie. Adopted once into the cookie when no
//      cookie exists yet, then removed.
//   3. HFS only: the per-user settings document (`/_user/settings`, #151/
//      #197) still roams the choice across devices. It seeds a browser that
//      has no choice of its own yet; once a browser has one, the browser's
//      choice wins and is merge-patched back to the document on change.
//      (HTS has no such endpoint and its layout names none.)
//
// Live sync between open tabs and windows — including an HFS tab next to an
// HTS tab — uses every channel a browser offers, each idempotent:
// BroadcastChannel("hfs-theme"), the `storage` event on `hfs-theme-sync`,
// the Cookie Store API's `change` event, and a cookie re-read whenever a tab
// regains focus or visibility (which also covers two consoles on different
// ports of one host, where cookies are shared but storage is not).
//
// Animation: a click that changes the EFFECTIVE appearance (light <-> dark)
// animates — a circular reveal from the button via the View Transitions API,
// else a short colour cross-fade. A click that keeps the appearance (system
// -> light on a light OS), a sync from another tab, and any change under
// `prefers-reduced-motion: reduce` do not animate. Every change dispatches
// `hfs:themechange` on document with {pref, theme, animation, source}.
(function () {
  "use strict";

  var root = document.documentElement;
  // The boot script already did this; kept for a layout that lacks it.
  root.classList.add("js");

  var COOKIE = "hfs_theme";
  var LEGACY_KEY = "hfs-theme";
  var SYNC_KEY = "hfs-theme-sync";
  var CHANNEL = "hfs-theme";
  var ORDER = ["system", "light", "dark"];
  var MAX_AGE = 60 * 60 * 24 * 365;

  var darkQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  var reduceQuery = window.matchMedia ? window.matchMedia("(prefers-reduced-motion: reduce)") : null;

  // Where the settings document lives is the server's call: HFS stamps
  // `<meta name="hfs-user-settings">` with the endpoint under its own API
  // prefix; a layout without the tag (HTS) means "no such endpoint".
  var settingsMeta = document.querySelector('meta[name="hfs-user-settings"]');
  var SETTINGS = settingsMeta ? settingsMeta.content : null;

  function valid(pref) {
    return ORDER.indexOf(pref) >= 0 ? pref : null;
  }

  function readCookie() {
    var match = document.cookie.match(new RegExp("(?:^|;\\s*)" + COOKIE + "=([^;]*)"));
    return match ? valid(decodeURIComponent(match[1])) : null;
  }

  function writeCookie(pref) {
    var secure = location.protocol === "https:" ? "; Secure" : "";
    document.cookie = COOKIE + "=" + pref + "; Path=/; Max-Age=" + MAX_AGE + "; SameSite=Lax" + secure;
  }

  function storage(fn) {
    try {
      return fn(window.localStorage);
    } catch (e) {
      return null; /* storage blocked: the cookie alone still works */
    }
  }

  function effective(pref) {
    if (pref === "system") return darkQuery && darkQuery.matches ? "dark" : "light";
    return pref;
  }

  function current() {
    return valid(root.getAttribute("data-theme-pref")) || "system";
  }

  function label(pref) {
    var buttons = document.querySelectorAll("[data-theme-cycle]");
    for (var i = 0; i < buttons.length; i++) {
      var text = buttons[i].getAttribute("data-label-" + pref);
      if (!text) continue;
      buttons[i].setAttribute("aria-label", text);
      buttons[i].setAttribute("title", text);
    }
  }

  function paint(pref) {
    root.setAttribute("data-theme-pref", pref);
    root.setAttribute("data-theme", effective(pref));
    label(pref);
  }

  var api = { animations: 0, last: null };

  function emit(pref, animation, source) {
    if (animation !== "none") api.animations++;
    api.last = { pref: pref, theme: effective(pref), animation: animation, source: source };
    root.setAttribute("data-theme-animation", animation);
    var event;
    try {
      event = new CustomEvent("hfs:themechange", { detail: api.last });
    } catch (e) {
      return;
    }
    document.dispatchEvent(event);
  }

  /* ---- persistence and fan-out ------------------------------------- */

  var channel = null;
  try {
    channel = window.BroadcastChannel ? new BroadcastChannel(CHANNEL) : null;
  } catch (e) {
    channel = null;
  }

  function persist(pref) {
    writeCookie(pref);
    storage(function (s) {
      s.removeItem(LEGACY_KEY);
      // A changing value is what fires `storage` in the other tabs.
      s.setItem(SYNC_KEY, pref + "|" + Date.now());
    });
    if (channel) {
      try {
        channel.postMessage({ pref: pref });
      } catch (e) {
        /* closed channel: the other lanes still deliver */
      }
    }
  }

  function patchServer(pref) {
    if (!window.fetch || !SETTINGS) return;
    fetch(SETTINGS, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      credentials: "same-origin",
      body: JSON.stringify({ theme: pref }),
    }).catch(function () {
      /* non-fatal: the cookie still holds the choice */
    });
  }

  /* ---- animated change --------------------------------------------- */

  function animateTo(pref, origin) {
    var rect = origin && origin.getBoundingClientRect ? origin.getBoundingClientRect() : null;
    var x = rect ? rect.left + rect.width / 2 : window.innerWidth;
    var y = rect ? rect.top + rect.height / 2 : 0;
    if (document.startViewTransition && !document.hidden) {
      var radius = Math.hypot(Math.max(x, window.innerWidth - x), Math.max(y, window.innerHeight - y));
      root.classList.add("theme-vt");
      inflight = pref;
      var transition;
      try {
        transition = document.startViewTransition(function () {
          paint(pref);
        });
      } catch (e) {
        transition = null;
      }
      if (transition) {
        transition.ready
          .then(function () {
            root.animate(
              {
                clipPath: [
                  "circle(0px at " + x + "px " + y + "px)",
                  "circle(" + Math.ceil(radius) + "px at " + x + "px " + y + "px)",
                ],
              },
              { duration: 450, easing: "ease-in-out", pseudoElement: "::view-transition-new(root)" }
            );
          })
          .catch(function () {});
        var done = function () {
          root.classList.remove("theme-vt");
          inflight = null;
        };
        transition.finished.then(done, done);
        emit(pref, "view-transition", "click");
        return;
      }
      root.classList.remove("theme-vt");
      inflight = null;
    }
    root.classList.add("theme-fade");
    paint(pref);
    setTimeout(function () {
      root.classList.remove("theme-fade");
    }, 320);
    emit(pref, "fade", "click");
  }

  // The preference a running view transition is about to paint: an echo of
  // our own write (the Cookie Store `change` event fires in this tab too)
  // must not paint it early, or the transition snapshots the new theme as
  // the old one and the reveal shows nothing.
  var inflight = null;

  function choose(pref, origin) {
    pref = valid(pref);
    if (!pref) return;
    var before = root.getAttribute("data-theme") || effective(current());
    var after = effective(pref);
    persist(pref);
    patchServer(pref);
    if (before === after || (reduceQuery && reduceQuery.matches)) {
      paint(pref);
      emit(pref, "none", "click");
      return;
    }
    animateTo(pref, origin);
  }

  /* A preference arriving from elsewhere (another tab, the other console,
     the roaming settings document): apply quietly, never re-broadcast. */
  function adopt(pref, source) {
    pref = valid(pref);
    if (!pref || pref === current() || pref === inflight) return;
    paint(pref);
    emit(pref, "none", source);
  }

  /* ---- startup ----------------------------------------------------- */

  var cookie = readCookie();
  var initial = cookie;
  if (!initial) {
    // Migration from the pre-cookie localStorage cache.
    var legacy = storage(function (s) {
      return s.getItem(LEGACY_KEY);
    });
    if (legacy === "light" || legacy === "dark") {
      initial = legacy;
      writeCookie(legacy);
    }
    storage(function (s) {
      s.removeItem(LEGACY_KEY);
    });
  }
  paint(initial || "system");
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () {
      label(current());
    });
  }

  // Roaming seed (HFS): only when this browser has no choice of its own.
  if (!initial && window.fetch && SETTINGS) {
    fetch(SETTINGS, { headers: { Accept: "application/json" }, credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (doc) {
        var roamed = doc && valid(doc.theme);
        if (!roamed || readCookie()) return;
        writeCookie(roamed);
        adopt(roamed, "settings");
      })
      .catch(function () {
        /* settings unavailable: the browser-local model is complete */
      });
  }

  /* ---- live updates ------------------------------------------------ */

  // The OS flipped light/dark: only the "system" preference follows it.
  if (darkQuery) {
    var onScheme = function () {
      if (current() !== "system") return;
      paint("system");
      emit("system", "none", "system");
    };
    if (darkQuery.addEventListener) darkQuery.addEventListener("change", onScheme);
    else if (darkQuery.addListener) darkQuery.addListener(onScheme);
  }

  if (channel) {
    channel.onmessage = function (event) {
      adopt(event.data && event.data.pref, "broadcast");
    };
  }

  window.addEventListener("storage", function (event) {
    if (event.key !== SYNC_KEY || !event.newValue) return;
    adopt(event.newValue.split("|")[0], "storage");
  });

  if (window.cookieStore && window.cookieStore.addEventListener) {
    window.cookieStore.addEventListener("change", function (event) {
      var changed = event.changed || [];
      for (var i = 0; i < changed.length; i++) {
        if (changed[i].name === COOKIE) adopt(changed[i].value, "cookie");
      }
    });
  }

  function recheck() {
    var pref = readCookie();
    if (pref) adopt(pref, "cookie");
  }
  window.addEventListener("focus", recheck);
  window.addEventListener("pageshow", recheck);
  document.addEventListener("visibilitychange", function () {
    if (!document.hidden) recheck();
  });

  /* ---- the button -------------------------------------------------- */

  document.addEventListener("click", function (event) {
    var button = event.target.closest && event.target.closest("[data-theme-cycle]");
    if (!button) return;
    var next = ORDER[(ORDER.indexOf(current()) + 1) % ORDER.length];
    choose(next, button);
  });

  api.get = current;
  api.set = function (pref) {
    choose(pref, document.querySelector("[data-theme-cycle]"));
  };
  api.effective = function () {
    return effective(current());
  };
  window.hfsTheme = api;
})();
