/*
 * Authorization servers console (built-in IdP, /ui/idp/**).
 *
 *  - form[data-idp-track]        unsaved-changes guard (HfsUnsaved)
 *  - [data-idp-issuer-preview]   live issuer URL from the realm name; the
 *                                name follows the display name until edited
 *  - [data-idp-for-kind]         fields shown only for some app types
 *  - [data-idp-provider]         endpoint Security: External / Internal —
 *                                Internal hides the hand-typed issuer, JWKS
 *                                and SMART fields (generated from the realm)
 *  - #hash / [data-idp-goto]     "Go to field": focus + highlight the field
 *  - [data-idp-app-select]       Test page: fill the app's redirect URI
 *  - [data-idp-busy]             finish button: busy text while the checks run
 *  - a[data-smart-how]           opens "How SMART access works" (#smart-how)
 *                                in place and scrolls to it
 *
 * Idempotent: safe to load twice (included by several partials) and after
 * boosted swaps.
 */
(function () {
  "use strict";
  if (window.HfsIdp) { window.HfsIdp.init(document); return; }

  function each(sel, root, fn) { Array.prototype.forEach.call((root || document).querySelectorAll(sel), fn); }

  function slug(s) {
    return s.toLowerCase().normalize("NFKD").replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").replace(/^[^a-z]+/, "").slice(0, 63);
  }

  function goTo(id) {
    if (!id) return;
    var el = document.getElementById(id);
    if (!el) return;
    var target = el.matches("input,select,textarea,button,a") ? el : el.querySelector("input,select,textarea,button,a") || el;
    var box = el.closest(".mfield, .mcheck, .idp-ep, tr, fieldset, section") || el;
    if (box.closest("details") && !box.closest("details").open) box.closest("details").open = true;
    box.scrollIntoView({ block: "center" });
    box.classList.add("idp-highlight");
    setTimeout(function () { box.classList.remove("idp-highlight"); }, 2400);
    if (target.focus) target.focus({ preventScroll: true });
  }

  function syncKinds(root) {
    var picked = (root.querySelector('input[name="client_kind"]:checked') || {}).value || "public";
    each("[data-idp-for-kind]", root, function (box) {
      var on = box.getAttribute("data-idp-for-kind").split(" ").indexOf(picked) >= 0;
      box.hidden = !on;
      each("input,textarea,select", box, function (i) { i.disabled = !on; });
    });
  }

  function syncProvider(fs) {
    var internal = (fs.querySelector('input[name="auth_provider"]:checked') || {}).value === "internal";
    var form = fs.closest("form") || document;
    each("[data-idp-internal]", fs, function (b) { b.hidden = !internal; });
    ["auth_issuer", "auth_jwks_url", "auth_algorithms", "smart_authorization_endpoint", "smart_token_endpoint", "smart_jwks_uri", "smart_discover"].forEach(function (n) {
      var i = form.querySelector('[name="' + n + '"]');
      if (!i) return;
      var box = i.closest(".mfield, .mcheck") || i;
      box.hidden = internal;
    });
    var smart = form.querySelector('[name="smart_authorization_endpoint"]');
    var disc = smart && smart.closest("details");
    if (disc) disc.hidden = internal;
  }

  function init(root) {
    root = root || document;
    each("form[data-idp-track]", root, function (f) {
      if (f.__idpTracked || !window.HfsUnsaved) return;
      f.__idpTracked = true;
      try { window.HfsUnsaved.track({ root: f, form: f, cue: document.querySelector(".page-head__title") }); } catch (e) { /* guard optional */ }
    });
    each("[data-idp-issuer-preview]", root, function (p) {
      var form = p.closest("form");
      var name = form.querySelector('[name="name"]');
      var display = form.querySelector('[name="display_name"]');
      var out = p.querySelector("[data-idp-issuer]");
      var base = p.getAttribute("data-base");
      if (!name || name.__idp) return;
      name.__idp = true;
      var touched = name.value !== "";
      function show() { out.textContent = base + (name.value || "…"); }
      name.addEventListener("input", function () { touched = true; show(); });
      if (display) display.addEventListener("input", function () { if (!touched) { name.value = slug(display.value); show(); } });
      show();
    });
    each('input[name="client_kind"]', root, function (r) {
      var form = r.closest("form");
      if (!form.__idpKinds) {
        form.__idpKinds = true;
        form.addEventListener("change", function (e) { if (e.target.name === "client_kind") syncKinds(form); });
        syncKinds(form);
      }
    });
    each("[data-idp-provider]", root, function (fs) {
      if (fs.__idp) return;
      fs.__idp = true;
      fs.addEventListener("change", function () { syncProvider(fs); });
      syncProvider(fs);
    });
    each("[data-idp-app-select]", root, function (s) {
      if (s.__idp) return;
      s.__idp = true;
      s.addEventListener("change", function () {
        var ru = s.form.querySelector('[name="app_redirect"]');
        var opt = s.options[s.selectedIndex];
        if (ru && opt) ru.value = opt.getAttribute("data-redirect") || "";
      });
    });
    each("form[data-idp-finish]", root, function (f) {
      if (f.__idp) return;
      f.__idp = true;
      f.addEventListener("submit", function () {
        var b = f.querySelector("[data-idp-busy]");
        if (b) { b.setAttribute("aria-disabled", "true"); b.classList.add("is-busy"); b.lastChild.textContent = b.getAttribute("data-idp-busy"); }
      });
    });
  }

  function openHow() {
    var box = document.getElementById("smart-how");
    if (!box) return false;
    box.open = true;
    box.scrollIntoView({ block: "start", behavior: window.HM && window.HM.motion && window.HM.motion.reduced ? "auto" : "smooth" });
    var s = box.querySelector("summary");
    if (s && s.focus) s.focus({ preventScroll: true });
    if (window.HM && window.HM.motion) window.HM.motion.enter(box.querySelector(".smart-how__body"), { dy: 6 });
    return true;
  }

  document.addEventListener("click", function (e) {
    var how = e.target.closest && e.target.closest("a[data-smart-how]");
    if (how && openHow()) { e.preventDefault(); return; }
    var a = e.target.closest && e.target.closest("[data-idp-goto]");
    if (a) { e.preventDefault(); goTo(a.getAttribute("data-idp-goto")); return; }
    var h = e.target.closest && e.target.closest("a[data-idp-goto-href]");
    if (h) {
      var u = new URL(h.href, location.href);
      if (u.pathname === location.pathname && u.hash) { e.preventDefault(); goTo(decodeURIComponent(u.hash.slice(1))); }
    }
  });

  window.HfsIdp = { init: init, goTo: goTo };
  function boot() {
    init(document);
    if (location.hash === "#smart-how") openHow();
    else if (location.hash.length > 1) goTo(decodeURIComponent(location.hash.slice(1)));
    var summary = document.querySelector(".idp-errors[data-autofocus]");
    if (summary && location.hash.length <= 1) summary.focus();
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot); else boot();
  document.addEventListener("htmx:afterSwap", function (e) { init(e.target); });
})();
