/*
 * Load-more focus management for the hts browser lists.
 *
 * "Load more" appends a page of rows to the table with htmx
 * (`hx-swap="beforeend"`). Left alone, the button is disabled and re-enabled
 * in place and focus falls back to <body>, so a keyboard or screen-reader
 * user has no idea where the new rows begin. This script remembers how many
 * rows the target had when a `[data-load-more]` request started and, once
 * the swap lands, moves focus to the first appended row (made focusable with
 * `tabindex="-1"` so it never joins the tab order). The count in the table
 * foot is an `aria-live` region of its own, so the new total is announced
 * as well.
 */
(function () {
  "use strict";

  document.addEventListener("htmx:beforeRequest", function (event) {
    var source = event.detail && event.detail.elt;
    var target = event.detail && event.detail.target;
    if (!source || !target || !source.hasAttribute("data-load-more")) return;
    target.dataset.rowsBefore = String(target.querySelectorAll(":scope > tr").length);
  });

  document.addEventListener("htmx:afterSwap", function (event) {
    var target = event.detail && event.detail.target;
    if (!target || target.dataset.rowsBefore === undefined) return;
    var before = Number(target.dataset.rowsBefore);
    delete target.dataset.rowsBefore;
    var rows = target.querySelectorAll(":scope > tr");
    var first = rows[before];
    if (!first) return;
    if (!first.hasAttribute("tabindex")) first.setAttribute("tabindex", "-1");
    first.focus({ preventScroll: false });
  });
})();
