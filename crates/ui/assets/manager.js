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
    (input.getAttribute("data-inherit-target") || "").split(/\s+/).forEach(function (id) {
      var target = id && document.getElementById(id);
      if (target) target.disabled = input.checked;
    });
    // The read-only "what inheriting means" box (UX-11).
    each('[data-inherit-shows="' + input.name + '"]', input.form || document, function (box) {
      box.hidden = !input.checked;
    });
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
  function attachLive() {
    if (!live.isConnected) document.body.appendChild(live);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", attachLive);
  else attachLive();
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
    function show(n, focus) {
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
      if (heading && focus) {
        heading.setAttribute("tabindex", "-1");
        heading.focus({ preventScroll: false });
      }
    }
    nav.addEventListener("click", function (e) {
      var b = e.target.closest("[data-wizard-go]");
      if (b) show(parseInt(b.getAttribute("data-wizard-go"), 10), true);
    });
    back.addEventListener("click", function () {
      show(current - 1, true);
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
      show(current + 1, true);
    });
    show(current, false);
  }

  // `data-show-when="field=value1|value2"`: show the block only while the
  // form's control `field` has one of the values (the sync job form's
  // per-kind / per-auth fields). Without script every block stays visible.
  function syncShowWhen(root) {
    each("[data-show-when]", root || document, function (el) {
      var rule = el.getAttribute("data-show-when").split("=");
      var form = el.closest("form");
      var control = form && form.elements[rule[0]];
      if (!control) return;
      var value = control.value;
      el.hidden = rule[1].split("|").indexOf(value) < 0;
    });
  }

  document.addEventListener("change", function (event) {
    var form = event.target && event.target.form;
    if (form && form.querySelector("[data-show-when]")) syncShowWhen(form);
  });

  // Unsaved-changes guard (UX-04) for every configuration form and the
  // wizard: any navigation away from an edited form (sidebar, endpoint
  // switcher, Back, in-page links, tab close) asks first; Save and Cancel
  // do not.
  function wireUnsaved(form) {
    if (!window.HfsUnsaved || form.__hmUnsaved) return;
    form.__hmUnsaved = true;
    var cue = form.querySelector(".save-bar__actions, .save-bar") || form;
    window.HfsUnsaved.track({ root: form, form: form, cue: cue });
  }

  // Verify panels (UX-02/07/08): stale marking when a checked field
  // changes, "Go to field", "Use suggested value", automatic run.
  function verifyPanels(form) {
    var out = [];
    if (!form) return out;
    each("[data-verify]", form, function (p) {
      out.push(p);
    });
    return out;
  }
  function watched(panel, name) {
    var list = (panel.getAttribute("data-verify-watch") || "").split(/\s+/);
    return list.indexOf(name) >= 0;
  }
  document.addEventListener("input", onVerifyEdit);
  document.addEventListener("change", onVerifyEdit);
  function onVerifyEdit(e) {
    var t = e.target;
    if (!t || !t.name || !t.form) return;
    verifyPanels(t.form).forEach(function (panel) {
      if (!watched(panel, t.name)) return;
      if (panel.querySelector("[data-verify-summary]")) {
        panel.classList.add("is-stale");
        each("[data-verify-stale]", panel, function (s) {
          s.hidden = false;
        });
      }
      if (panel.hasAttribute("data-verify-auto") && e.type === "input") {
        clearTimeout(panel.__hmAuto);
        panel.__hmAuto = setTimeout(function () {
          var run = panel.querySelector("[data-verify-run]");
          if (run && !run.disabled && run.offsetParent !== null) run.click();
        }, 600);
      }
    });
  }
  document.addEventListener("click", function (e) {
    var go = e.target.closest && e.target.closest("[data-verify-goto]");
    if (go) {
      var input = document.getElementById(go.getAttribute("data-verify-goto"));
      if (!input) return;
      e.preventDefault();
      // Reveal it: a wizard step, a closed disclosure, an inherit switch.
      var step = input.closest("[data-wizard-step]");
      if (step && step.hidden) {
        var form = input.closest("form");
        var steps = form ? form.querySelectorAll("[data-wizard-step]") : [];
        for (var i = 0; i < steps.length; i++) {
          if (steps[i] === step) {
            var goBtn = form.querySelector('[data-wizard-go="' + i + '"]');
            if (goBtn) goBtn.click();
          }
        }
      }
      var details = input.closest("details");
      if (details) details.open = true;
      input.scrollIntoView({ block: "center" });
      input.focus({ preventScroll: true });
      var field = input.closest(".mfield");
      if (field) {
        field.classList.add("is-verify-target");
        setTimeout(function () {
          field.classList.remove("is-verify-target");
        }, 2500);
      }
      return;
    }
    var fill = e.target.closest && e.target.closest("[data-verify-fill]");
    if (fill) {
      var target = document.getElementById(fill.getAttribute("data-verify-fill"));
      if (!target) return;
      target.value = fill.getAttribute("data-verify-value") || "";
      target.dispatchEvent(new Event("input", { bubbles: true }));
      target.dispatchEvent(new Event("change", { bubbles: true }));
      target.focus();
    }
  });
  // A fresh result takes focus so screen readers hear the outcome.
  document.addEventListener("htmx:afterSwap", function (e) {
    var t = e.detail && e.detail.target;
    if (!t || !t.matches || !t.matches("[data-verify-result]")) return;
    var panel = t.closest("[data-verify]");
    if (panel) panel.classList.remove("is-stale");
    var summary = t.querySelector("[data-verify-summary]");
    if (summary && !(panel && panel.hasAttribute("data-verify-auto"))) summary.focus({ preventScroll: true });
  });

  // Elements wired once each (pages swapped in place bring new elements).
  function once(el, key) {
    if (el["__hm" + key]) return false;
    el["__hm" + key] = true;
    return true;
  }

  function init() {
    syncShowWhen(document);
    each("[data-inherit-target]", document, syncInherit);
    each("form[data-typed-confirm]", document, function (f) {
      if (once(f, "Typed")) wireTyped(f);
    });
    each("form[data-unsaved-form], form[data-unsaved-form-auto]", document, wireUnsaved);
    var names = {};
    each("input[type=radio][data-reveal]", document, function (r) {
      names[r.name] = true;
    });
    Object.keys(names).forEach(syncReveal);
    each("[data-slug-from]", document, function (i) {
      if (once(i, "Slug")) wireSlug(i);
    });
    each("form[data-wizard]", document, function (f) {
      if (once(f, "Wizard")) wireWizard(f);
    });
    var focus = document.querySelector("[data-autofocus]");
    if (focus && once(focus, "Focus")) focus.focus();
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
  // Pages swapped in place by the shell (shell.js).
  document.addEventListener("hm:navigated", init);
})();
