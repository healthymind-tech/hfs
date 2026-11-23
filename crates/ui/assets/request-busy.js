/*
 * Loading feedback for user-initiated htmx requests, the same in both
 * consoles (loaded by crates/ui and crates/hts-ui layouts/base.html).
 *
 * While such a request is in flight its swap target carries
 * `aria-busy="true"` (assistive tech hears that the region is updating). If
 * the request is still running after DELAY ms the target also gets
 * `.is-loading`, and the element that issued it (and its form) gets
 * `.is-requesting`: app.css dims the target's content, draws a ring over it
 * and turns the filter field's magnifier into a ring. Everything is opacity
 * and absolutely positioned pseudo-elements, so nothing moves; a fast
 * response never shows anything, so nothing flickers.
 *
 * "User-initiated" means the request came from a real event (input, change,
 * submit, click, search). Polling (`every Ns`) and self-loading fragments
 * (`load`, `revealed`, `intersect`) keep their own pending states and stay
 * quiet here, as does anything inside `[data-busy="off"]` (the combobox,
 * which has its own loading footer). Inline targets (a one-line hint) get
 * `aria-busy` only: a ring over a word reads as noise.
 *
 * The state is cleared on the request's XHR `loadend`, which fires after
 * htmx's own `onload` swap and also on error, abort (`hx-sync` replace) and
 * timeout, so no path leaves a region dimmed. A per-element counter keeps an
 * element busy while a newer request for it is still running.
 */
(function () {
  "use strict";

  var DELAY = 300;
  var QUIET = /^(load|revealed|intersect|every|htmx:)/;
  var counts = new WeakMap();

  function userInitiated(detail) {
    var config = detail.requestConfig;
    var event = config && config.triggeringEvent;
    return !!(event && event.type && !QUIET.test(event.type));
  }

  function hold(el, attr, cls) {
    var n = (counts.get(el) || 0) + 1;
    counts.set(el, n);
    if (attr) el.setAttribute("aria-busy", "true");
    return function show() {
      if (counts.get(el) > 0 && cls) el.classList.add(cls);
    };
  }

  function release(el, attr, cls) {
    var n = (counts.get(el) || 1) - 1;
    counts.set(el, n);
    if (n > 0) return;
    if (attr) el.removeAttribute("aria-busy");
    if (cls) el.classList.remove(cls);
  }

  /* beforeSend, not beforeRequest: a listener may still cancel the latter,
     and a cancelled request would never reach loadend. */
  document.addEventListener("htmx:beforeSend", function (event) {
    var detail = event.detail || {};
    var elt = detail.elt;
    var xhr = detail.xhr;
    var target = detail.target || elt;
    if (!elt || !xhr || !target || !userInitiated(detail)) return;
    // In-place page navigations have their own progress (shell.js).
    if (detail.requestConfig && detail.requestConfig.boosted) return;
    if (elt.closest('[data-busy="off"]') || target.closest('[data-busy="off"]')) return;

    var block = getComputedStyle(target).display !== "inline";
    var sources = [elt];
    var form = elt.closest("form");
    if (form && form !== elt) sources.push(form);

    var shows = [hold(target, true, block ? "is-loading" : null)];
    sources.forEach(function (s) {
      shows.push(hold(s, false, "is-requesting"));
    });
    var timer = setTimeout(function () {
      shows.forEach(function (show) {
        show();
      });
    }, DELAY);

    var done = false;
    xhr.addEventListener("loadend", function () {
      if (done) return;
      done = true;
      clearTimeout(timer);
      release(target, true, block ? "is-loading" : null);
      sources.forEach(function (s) {
        release(s, false, "is-requesting");
      });
    });
  });
})();
