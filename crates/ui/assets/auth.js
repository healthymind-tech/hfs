/*
 * Field affordances of the standalone pages (sign-in, break-glass sign-in,
 * forced password change; layouts/auth.html of both consoles). Everything
 * here is progressive: without script the forms post exactly the same
 * fields and the browser's own required-field checks apply.
 *
 *  - [data-pw-reveal] buttons show / hide the password of their field
 *    (aria-pressed + label swap; the type flips back before submit so a
 *    password manager still sees a password field).
 *  - Caps Lock: a field with data-caps shows that text in its message slot
 *    while Caps Lock is on (the slot has a fixed height: no layout shift).
 *  - Inline required-field messages instead of the browser bubble
 *    (data-msg-required), focus on the first invalid field.
 *  - Password policy (form[data-pw-policy]): the checklist ticks live; a
 *    too-short new password or a mismatching confirmation is reported on its
 *    field before the round trip (the server checks everything again).
 *  - One submit: the button goes busy (aria-busy, the shared spinner) and
 *    shows its data-busy-label; a second submit is ignored.
 */
(function () {
  "use strict";

  function msgFor(input) {
    var ids = (input.getAttribute("aria-describedby") || "").split(/\s+/);
    for (var i = 0; i < ids.length; i++) {
      var el = ids[i] && document.getElementById(ids[i]);
      if (el && el.classList.contains("auth-msg")) return el;
    }
    return null;
  }

  /* The message slot shows, in order: an error, else the caps-lock hint. */
  function paint(input) {
    var slot = msgFor(input);
    if (!slot) return;
    var error = input.getAttribute("data-error") || "";
    var caps = input.getAttribute("data-caps-on") === "true" ? input.getAttribute("data-caps") || "" : "";
    slot.textContent = error || caps;
    slot.classList.toggle("auth-msg--error", !!error);
    slot.classList.toggle("auth-msg--caps", !error && !!caps);
  }

  function setError(input, text) {
    if (text) {
      input.setAttribute("data-error", text);
      input.setAttribute("aria-invalid", "true");
    } else {
      input.removeAttribute("data-error");
      input.removeAttribute("aria-invalid");
    }
    paint(input);
  }

  /* ---- show password ------------------------------------------------ */
  document.addEventListener("click", function (event) {
    var button = event.target.closest && event.target.closest("[data-pw-reveal]");
    if (!button) return;
    var input = button.parentElement && button.parentElement.querySelector("input");
    if (!input) return;
    var show = input.type === "password";
    input.type = show ? "text" : "password";
    button.setAttribute("aria-pressed", show ? "true" : "false");
    var label = button.getAttribute(show ? "data-label-hide" : "data-label-show");
    if (label) {
      button.setAttribute("aria-label", label);
      button.setAttribute("title", label);
    }
    input.focus();
  });

  /* ---- caps lock ---------------------------------------------------- */
  function caps(event) {
    var input = event.target;
    if (!input || !input.hasAttribute || !input.hasAttribute("data-caps")) return;
    if (typeof event.getModifierState !== "function") return;
    var on = event.getModifierState("CapsLock");
    if ((input.getAttribute("data-caps-on") === "true") === on) return;
    input.setAttribute("data-caps-on", on ? "true" : "false");
    paint(input);
  }
  document.addEventListener("keydown", caps);
  document.addEventListener("keyup", caps);
  document.addEventListener("focusout", function (event) {
    var input = event.target;
    if (input && input.getAttribute && input.getAttribute("data-caps-on") === "true") {
      input.setAttribute("data-caps-on", "false");
      paint(input);
    }
  });

  /* ---- password policy ---------------------------------------------- */
  function policy(form) {
    var list = form.querySelector(".auth-policy");
    var pw = form.querySelector("[data-pw-new]");
    var confirm = form.querySelector("[data-pw-confirm]");
    if (!list || !pw) return;
    var value = pw.value;
    list.querySelectorAll("li[data-rule]").forEach(function (li) {
      var rule = li.getAttribute("data-rule");
      var state = null;
      if (rule === "length") {
        state = value ? value.length >= parseInt(li.getAttribute("data-min") || "0", 10) : null;
      } else if (rule === "user") {
        var user = (li.getAttribute("data-user") || "").toLowerCase();
        state = value ? !(user && value.toLowerCase().indexOf(user) >= 0) : null;
      } else if (rule === "match") {
        state = value && confirm && confirm.value ? confirm.value === value : null;
      }
      if (state === null) li.removeAttribute("data-state");
      else li.setAttribute("data-state", state ? "ok" : "bad");
    });
  }

  document.addEventListener("input", function (event) {
    var input = event.target;
    if (!input || !input.closest) return;
    if (input.hasAttribute("data-error")) setError(input, "");
    var form = input.closest("form[data-pw-policy]");
    if (form) {
      policy(form);
      var confirm = form.querySelector("[data-pw-confirm]");
      if (input.hasAttribute("data-pw-new") && confirm && confirm.hasAttribute("data-error")) setError(confirm, "");
    }
  });

  /* ---- validation + one submit -------------------------------------- */
  function check(form) {
    var first = null;
    form.querySelectorAll("input[required]").forEach(function (input) {
      var text = "";
      if (!input.value) {
        text = input.getAttribute("data-msg-required") || input.validationMessage;
      } else if (input.hasAttribute("data-pw-new")) {
        var min = parseInt(input.getAttribute("minlength") || "0", 10);
        if (input.value.length < min) text = input.getAttribute("data-msg-short") || input.validationMessage;
      } else if (input.hasAttribute("data-pw-confirm")) {
        var pw = form.querySelector("[data-pw-new]");
        if (pw && pw.value !== input.value) text = input.getAttribute("data-msg-mismatch") || "";
      }
      setError(input, text);
      if (text && !first) first = input;
    });
    return first;
  }

  function init(form) {
    // Our inline messages replace the browser bubble (still the fallback
    // without script).
    form.noValidate = true;
    form.addEventListener("submit", function (event) {
      if (form.getAttribute("data-submitting") === "true") {
        event.preventDefault();
        return;
      }
      var invalid = check(form);
      if (invalid) {
        event.preventDefault();
        invalid.focus();
        return;
      }
      // A revealed password goes back to a password field before it is sent.
      form.querySelectorAll("[data-pw-reveal][aria-pressed='true']").forEach(function (b) {
        b.click();
      });
      form.setAttribute("data-submitting", "true");
      var button = form.querySelector("button[type=submit]");
      if (button) {
        var label = button.getAttribute("data-busy-label");
        // A tick later: a disabled submitter would drop out of the entries.
        setTimeout(function () {
          button.setAttribute("aria-busy", "true");
          button.disabled = true;
          if (label) button.textContent = label;
        }, 0);
      }
    });
    if (form.hasAttribute("data-pw-policy")) policy(form);
  }

  function start() {
    document.querySelectorAll("form[data-auth-form]").forEach(init);
    // Server-side field errors: keep them when the user starts typing
    // elsewhere, clear them on the field itself (input handler above).
    document.querySelectorAll(".auth-msg--error").forEach(function (slot) {
      var input = document.querySelector('[aria-describedby~="' + slot.id + '"]');
      if (input && slot.textContent.trim()) input.setAttribute("data-error", slot.textContent.trim());
    });
    // The "show password" buttons are useless without this script.
    document.documentElement.classList.add("auth-js");
  }

  // Back from the next page (bfcache): the form is usable again.
  window.addEventListener("pageshow", function (event) {
    if (!event.persisted) return;
    document.querySelectorAll("form[data-submitting='true']").forEach(function (form) {
      form.removeAttribute("data-submitting");
      var button = form.querySelector("button[type=submit]");
      if (button) {
        button.disabled = false;
        button.removeAttribute("aria-busy");
      }
    });
  });

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
