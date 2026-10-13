/*
 * Responsive navigation drawer, shared by the HFS and HTS consoles.
 *
 * On a wide screen the sidebar is the hover-expanding icon rail `app.css`
 * has always drawn and this script does nothing. Below the drawer
 * breakpoint (`--nav-drawer-max`, 1024px, mirrored in the stylesheet's
 * media query) the rail has no resting state a finger can use — hovering
 * does not exist on a touch screen — so the sidebar becomes an off-canvas
 * drawer with its full labels, opened from the hamburger in the topbar.
 *
 * This script owns the drawer's behaviour and nothing else: the open/closed
 * state (`body.nav-open` + the toggle's `aria-expanded`), Escape to close,
 * closing on the scrim, a focus trap while open, and handing focus back to
 * the toggle when the drawer closes. Without JavaScript the stylesheet
 * (`html:not(.js)`) stacks the full sidebar above the content instead, so
 * every link stays reachable.
 */
(function () {
  "use strict";

  var sidebar = document.getElementById("sidebar");
  var toggles = document.querySelectorAll("[data-nav-toggle]");
  var scrim = document.querySelector("[data-nav-scrim]");
  if (!sidebar || !toggles.length) return;

  var drawerQuery = window.matchMedia("(max-width: 1024px)");
  var opener = null;

  function isOpen() {
    return document.body.classList.contains("nav-open");
  }

  function setExpanded(open) {
    for (var i = 0; i < toggles.length; i++) {
      toggles[i].setAttribute("aria-expanded", open ? "true" : "false");
    }
  }

  function focusables() {
    var nodes = sidebar.querySelectorAll(
      'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), summary, [tabindex]:not([tabindex="-1"])'
    );
    var out = [];
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      if (el.closest("details:not([open])") && el.tagName !== "SUMMARY") continue;
      if (el.getClientRects().length) out.push(el);
    }
    return out;
  }

  function open(from) {
    if (isOpen()) return;
    opener = from || document.activeElement;
    document.body.classList.add("nav-open");
    setExpanded(true);
    var first = sidebar.querySelector("[data-nav-close]") || focusables()[0];
    if (first) first.focus();
  }

  function close(returnFocus) {
    if (!isOpen()) return;
    document.body.classList.remove("nav-open");
    setExpanded(false);
    if (returnFocus !== false && opener && typeof opener.focus === "function") {
      opener.focus();
    }
    opener = null;
  }

  document.addEventListener("click", function (event) {
    var target = event.target.closest ? event.target.closest("[data-nav-toggle], [data-nav-close], [data-nav-scrim]") : null;
    if (!target) return;
    if (target.hasAttribute("data-nav-toggle")) {
      event.preventDefault();
      if (isOpen()) close();
      else open(target);
    } else {
      event.preventDefault();
      close();
    }
  });

  if (scrim) {
    scrim.addEventListener("click", function () {
      close();
    });
  }

  document.addEventListener("keydown", function (event) {
    if (!isOpen() || !drawerQuery.matches) return;
    if (event.key === "Escape" || event.key === "Esc") {
      event.preventDefault();
      close();
      return;
    }
    if (event.key !== "Tab") return;
    var items = focusables();
    if (!items.length) return;
    var first = items[0];
    var last = items[items.length - 1];
    var active = document.activeElement;
    if (event.shiftKey && (active === first || !sidebar.contains(active))) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && (active === last || !sidebar.contains(active))) {
      event.preventDefault();
      first.focus();
    }
  });

  /* A tap on a navigation link closes the drawer so the destination is not
     covered when the page is swapped in place (htmx boosts, same-page
     anchors); a full navigation discards the class with the document. */
  sidebar.addEventListener("click", function (event) {
    var link = event.target.closest ? event.target.closest("a[href]") : null;
    if (link && drawerQuery.matches) close(false);
  });

  /* Growing past the breakpoint turns the drawer back into the rail: drop
     the open state so the rail never starts out pinned. */
  function onViewportChange() {
    if (!drawerQuery.matches) close(false);
  }
  if (typeof drawerQuery.addEventListener === "function") {
    drawerQuery.addEventListener("change", onViewportChange);
  } else if (typeof drawerQuery.addListener === "function") {
    drawerQuery.addListener(onViewportChange);
  }
})();
