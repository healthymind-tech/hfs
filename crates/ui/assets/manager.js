/*
 * Manager console behaviours (progressive enhancement only — every page works
 * without this script):
 *
 *  - [data-inherit-target]   "Inherit from server" switches disable their fieldset
 *  - [data-copy]             copy-to-clipboard buttons (base URLs)
 *  - [data-confirm]          in-page confirmation (HfsConfirm) before submitting
 *  - [data-typed-confirm]    "type the name to delete" forms
 *  - [data-reveal]           radio cards that reveal their detail panel
 *  - [data-slug-from]        endpoint name suggested from the display name
 *  - [data-wizard]           the new-endpoint stepper
 *  - [data-autofocus]        error summaries take focus after a failed save
 */
(function () {
  "use strict";

  function each(sel, root, fn) {
    var nodes = (root || document).querySelectorAll(sel);
    for (var i = 0; i < nodes.length; i++) fn(nodes[i]);
  }

  // Inherit switches.
  function syncInherit(input) {
    var target = document.getElementById(input.getAttribute("data-inherit-target"));
    if (target) target.disabled = input.checked;
  }
  document.addEventListener("change", function (e) {
    var t = e.target;
    if (t && t.matches && t.matches("[data-inherit-target]")) syncInherit(t);
    if (t && t.matches && t.matches("input[type=radio][data-reveal]")) syncReveal(t.name);
  });

  // Copy buttons.
  var live = document.createElement("div");
  live.className = "visually-hidden";
  live.setAttribute("aria-live", "polite");
  document.addEventListener("DOMContentLoaded", function () {
    document.body.appendChild(live);
  });
  document.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest("[data-copy]");
    if (!btn) return;
    var text = btn.getAttribute("data-copy");
    var done = function () {
      var msg = document.body.getAttribute("data-msg-copied") || "Copied";
      live.textContent = msg;
      btn.classList.add("is-copied");
      setTimeout(function () {
        btn.classList.remove("is-copied");
        live.textContent = "";
      }, 1500);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () {});
    }
  });

  // Confirmations through the shared in-page dialog.
  document.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest("button[data-confirm]");
    if (!btn || btn.hasAttribute("data-confirmed")) return;
    if (!window.HfsConfirm) return;
    e.preventDefault();
    window.HfsConfirm.ask(btn.getAttribute("data-confirm"), { danger: true }).then(function (ok) {
      if (!ok) return;
      btn.setAttribute("data-confirmed", "");
      btn.click();
    });
  });

  // Typed-name confirmations.
  function wireTyped(form) {
    var expected = form.getAttribute("data-typed-confirm");
    var input = form.querySelector("input[name=confirm]");
    var submit = form.querySelector("[data-typed-confirm-submit]");
    if (!input || !submit) return;
    var sync = function () {
      submit.disabled = input.value.trim() !== expected;
    };
    input.addEventListener("input", sync);
    sync();
  }

  // Reveal panels of radio cards.
  function syncReveal(name) {
    each('input[type=radio][name="' + name + '"][data-reveal]', document, function (r) {
      each('[data-reveal-id="' + r.getAttribute("data-reveal") + '"]', document, function (panel) {
        panel.classList.toggle("is-shown", r.checked);
      });
    });
  }

  // Slug suggestion: lower-case, a-z0-9 and dashes, until the user edits it.
  function wireSlug(input) {
    var source = input.form && input.form.elements[input.getAttribute("data-slug-from")];
    if (!source) return;
    var touched = input.value !== "";
    input.addEventListener("input", function () {
      touched = true;
    });
    source.addEventListener("input", function () {
      if (touched) return;
      var slug = source.value
        .toLowerCase()
        .normalize("NFKD")
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/^-+|-+$/g, "")
        .replace(/^[^a-z]+/, "")
        .slice(0, 63)
        .replace(/-+$/, "");
      input.value = slug;
      input.dispatchEvent(new Event("change", { bubbles: true }));
    });
  }

  // The new-endpoint stepper.
  function wireWizard(form) {
    var steps = form.querySelectorAll("[data-wizard-step]");
    var nav = form.querySelector("[data-wizard-steps]");
    var back = form.querySelector("[data-wizard-back]");
    var next = form.querySelector("[data-wizard-next]");
    var submit = form.querySelector("[data-wizard-submit]");
    if (!steps.length || !nav) return;
    var current = 0;
    // A failed submit lands on the first step with an error.
    for (var i = 0; i < steps.length; i++) {
      if (steps[i].querySelector(".mfield--error, .mfield__error")) {
        current = i;
        break;
      }
    }
    nav.hidden = false;
    back.hidden = false;
    next.hidden = false;
    function show(n) {
      current = Math.max(0, Math.min(steps.length - 1, n));
      for (var i = 0; i < steps.length; i++) steps[i].hidden = i !== current;
      var buttons = nav.querySelectorAll("[data-wizard-go]");
      for (var j = 0; j < buttons.length; j++) {
        if (j === current) buttons[j].setAttribute("aria-current", "step");
        else buttons[j].removeAttribute("aria-current");
        buttons[j].classList.toggle("is-done", j < current);
      }
      back.disabled = current === 0;
      var last = current === steps.length - 1;
      next.hidden = last;
      submit.hidden = !last;
      if (last && window.htmx) {
        var preview = form.querySelector("[data-wizard-preview]");
        if (preview) window.htmx.trigger(preview, "click");
      }
      var heading = steps[current].querySelector("h2");
      if (heading) {
        heading.setAttribute("tabindex", "-1");
        heading.focus({ preventScroll: false });
      }
    }
    nav.addEventListener("click", function (e) {
      var b = e.target.closest("[data-wizard-go]");
      if (b) show(parseInt(b.getAttribute("data-wizard-go"), 10));
    });
    back.addEventListener("click", function () {
      show(current - 1);
    });
    next.addEventListener("click", function () {
      // Native constraint check of the visible step before moving on.
      var fields = steps[current].querySelectorAll("input, select, textarea");
      for (var i = 0; i < fields.length; i++) {
        if (!fields[i].disabled && fields[i].offsetParent !== null && !fields[i].checkValidity()) {
          fields[i].reportValidity();
          return;
        }
      }
      show(current + 1);
    });
    show(current);
  }

  function init() {
    each("[data-inherit-target]", document, syncInherit);
    each("form[data-typed-confirm]", document, wireTyped);
    var names = {};
    each("input[type=radio][data-reveal]", document, function (r) {
      names[r.name] = true;
    });
    Object.keys(names).forEach(syncReveal);
    each("[data-slug-from]", document, wireSlug);
    each("form[data-wizard]", document, wireWizard);
    var focus = document.querySelector("[data-autofocus]");
    if (focus) focus.focus();
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
