/*
 * The Validate page (/ui/validate): paste or upload any FHIR resource or
 * Bundle, optionally name a profile canonical, and read the server's
 * `$validate` OperationOutcome — nothing is stored.
 *
 * The JSON syntax is checked as the user types (bundle-check.js, with the
 * error's line:column); the request and the rendering are fhir-validate.js,
 * shared with the Batch / Transaction page. The profile field's datalist is
 * filled on first use from the StructureDefinitions stored on this server
 * (`GET [base]/StructureDefinition?derivation=constraint`), so the TW Core
 * (or any other loaded IG's) profiles are one pick away.
 */
(function () {
  "use strict";

  var root = document.getElementById("validate");
  if (!root || !window.fetch || !window.hfsBusy || !window.HfsBundleCheck || !window.HfsValidate) return;
  var Check = window.HfsBundleCheck;
  var messages = root.dataset;
  var checkMessages = Check.messagesFrom(document.getElementById("bundle-check-messages"));
  var validateMessages = Check.messagesFrom(document.getElementById("validate-messages"));
  var MAX_BODY_SIZE = Number(root.dataset.maxBodySize) || 0;

  var TENANT = (document.querySelector('meta[name="hfs-tenant"]') || {}).content || "";
  var FHIR_BASE = ((document.querySelector('meta[name="hfs-fhir-base"]') || {}).content || "").replace(/\/+$/, "");

  var form = document.getElementById("validate-form");
  var textarea = document.getElementById("validate-text");
  var uploadBtn = document.getElementById("validate-upload");
  var fileInput = document.getElementById("validate-file");
  var fileName = document.getElementById("validate-file-name");
  var formatBtn = document.getElementById("validate-format");
  var checkPanel = document.getElementById("validate-check");
  var checkErrors = document.getElementById("validate-check-errors");
  var profileInput = document.getElementById("validate-profile");
  var profileList = document.getElementById("validate-profiles");
  var perEntryOption = document.getElementById("validate-per-entry-option");
  var perEntryHint = document.getElementById("validate-per-entry-hint");
  var perEntry = document.getElementById("validate-per-entry");
  var runBtn = document.getElementById("validate-run");
  var resultCard = document.getElementById("validate-result");
  var progress = document.getElementById("validate-progress");
  var results = document.getElementById("validate-results");

  var view = null;
  var report = null;

  /* ---- editor ---------------------------------------------------------- */

  if (window.HfsCodeEditor && window.HfsCodeMirror) {
    var CM = window.HfsCodeMirror;
    var highlight = window.HfsCodeEditor.jsonHighlight();
    view = window.HfsCodeEditor.mount(textarea, {
      language: CM.json(),
      highlight: highlight ? [CM.syntaxHighlighting(highlight)] : [],
      extensions: CM.lintGutter ? [CM.lintGutter()] : [],
      fold: true,
      wrapperClass: "batch-paste__editor",
      id: "validate-editor",
    });
  }

  function setText(text) {
    if (view) view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
    else textarea.value = text;
  }

  function offsetOf(line, col) {
    var doc = view.state.doc;
    if (line < 1 || line > doc.lines) return doc.length;
    var l = doc.line(line);
    return Math.min(l.from + Math.max(col - 1, 0), l.to);
  }

  function jumpTo(line, col) {
    if (view) {
      view.dispatch({ selection: { anchor: offsetOf(line, col) }, scrollIntoView: true });
      view.focus();
      return;
    }
    var text = textarea.value;
    var offset = 0;
    for (var l = 1; l < line && offset >= 0; l++) offset = text.indexOf("\n", offset) + 1;
    offset += col - 1;
    textarea.focus();
    textarea.setSelectionRange(offset, offset + 1);
  }

  /* ---- live syntax check ---------------------------------------------- */

  function check() {
    var text = textarea.value;
    report = Check.check(text, { maxBytes: MAX_BODY_SIZE, messages: checkMessages, requireTransaction: false });
    var empty = report.errors.length === 1 && report.errors[0].code === "empty";
    var list = checkErrors.querySelector("ul");
    list.textContent = "";
    report.errors.forEach(function (problem) {
      if (problem.code === "empty") return;
      var li = document.createElement("li");
      if (problem.line != null) {
        var jump = document.createElement("button");
        jump.type = "button";
        jump.className = "batch-check__at";
        jump.textContent = messages.msgCheckAt.replace("%line%", problem.line).replace("%col%", problem.col);
        jump.addEventListener("click", function () {
          jumpTo(problem.line, problem.col);
        });
        li.appendChild(jump);
      }
      li.appendChild(document.createTextNode(problem.message));
      list.appendChild(li);
    });
    var shown = report.errors.length && !empty;
    checkErrors.querySelector(".batch-check__count").textContent = shown ? "· " + report.errors.length : "";
    checkErrors.hidden = !shown;
    checkPanel.hidden = !shown;
    runBtn.disabled = !!report.errors.length;
    var isBundle = !report.errors.length && report.value && report.value.resourceType === "Bundle";
    perEntryOption.hidden = !isBundle;
    perEntryHint.hidden = !isBundle;
    var CMx = window.HfsCodeMirror;
    if (view && CMx && CMx.setDiagnostics) {
      var diagnostics = [];
      report.errors.forEach(function (problem) {
        if (problem.line == null) return;
        var from = offsetOf(problem.line, problem.col);
        diagnostics.push({ from: from, to: Math.min(from + 1, view.state.doc.length), severity: "error", message: problem.message });
      });
      try {
        view.dispatch(CMx.setDiagnostics(view.state, diagnostics));
      } catch (e) {
        /* the list above is authoritative */
      }
    }
    return report;
  }

  var debounce = null;
  textarea.addEventListener("input", function () {
    clearTimeout(debounce);
    debounce = setTimeout(check, 350);
  });

  formatBtn.addEventListener("click", function () {
    try {
      setText(Check.format(textarea.value));
    } catch (e) {
      /* the syntax error is shown, with its position */
    }
    clearTimeout(debounce);
    check();
  });

  uploadBtn.addEventListener("click", function () {
    fileInput.click();
  });
  fileInput.addEventListener("change", function () {
    var file = fileInput.files && fileInput.files[0];
    if (!file) return;
    var reader = new FileReader();
    reader.onerror = reader.onabort = function () {
      fileName.textContent = messages.msgReadFailed;
    };
    reader.onload = function () {
      fileName.textContent = file.name;
      setText(String(reader.result));
      clearTimeout(debounce);
      check();
      fileInput.value = "";
    };
    reader.readAsText(file);
  });

  /* ---- profile datalist ------------------------------------------------ */

  var profilesRequested = false;
  function loadProfiles() {
    if (profilesRequested) return;
    profilesRequested = true;
    var headers = { Accept: "application/fhir+json" };
    if (TENANT) headers["X-Tenant-ID"] = TENANT;
    var seen = {};
    var options = [];
    var pages = 0;
    function page(url) {
      return fetch(url, { headers: headers, credentials: "same-origin" })
        .then(function (r) {
          return r.ok ? r.json() : null;
        })
        .then(function (bundle) {
          if (!bundle) return;
          (bundle.entry || []).forEach(function (e) {
            var sd = e.resource;
            // `derivation` is not a search parameter every server indexes
            // (HFS ignores it), so base definitions are filtered out here.
            if (!sd || !sd.url || seen[sd.url] || sd.derivation === "specialization") return;
            seen[sd.url] = true;
            options.push({ url: sd.url, label: (sd.title || sd.name || sd.url) + (sd.type ? " (" + sd.type + ")" : ""), type: sd.type || "" });
          });
          var next = (bundle.link || []).filter(function (l) {
            return l.relation === "next";
          })[0];
          if (next && next.url && ++pages < 10) {
            var nextUrl = next.url;
            // Paging links carry the server's public base; keep the call
            // same-origin by reusing only path + query.
            try {
              var parsed = new URL(nextUrl, location.href);
              nextUrl = parsed.pathname + parsed.search;
            } catch (e) {
              /* use as given */
            }
            return page(nextUrl);
          }
        });
    }
    page(FHIR_BASE + "/StructureDefinition?derivation=constraint&_elements=url,name,title,type,derivation&_count=500")
      .then(function () {
        options.sort(function (a, b) {
          return a.type === b.type ? (a.label < b.label ? -1 : 1) : a.type < b.type ? -1 : 1;
        });
        var fragment = document.createDocumentFragment();
        options.forEach(function (o) {
          var opt = document.createElement("option");
          opt.value = o.url;
          opt.label = o.label;
          opt.textContent = o.label;
          fragment.appendChild(opt);
        });
        profileList.textContent = "";
        profileList.appendChild(fragment);
        profileList.setAttribute("data-count", String(options.length));
      })
      .catch(function () {
        profilesRequested = false;
      });
  }
  profileInput.addEventListener("focus", loadProfiles);
  profileInput.addEventListener("pointerdown", loadProfiles);

  /* ---- run ------------------------------------------------------------- */

  var abort = null;
  form.addEventListener("submit", function (event) {
    event.preventDefault();
    clearTimeout(debounce);
    var r = check();
    if (r.errors.length) return;
    if (document.getElementById("auth-bearer-only")) {
      resultCard.hidden = false;
      results.textContent = "";
      var note = document.createElement("p");
      note.className = "alert";
      note.textContent = messages.msgSignInRequired;
      results.appendChild(note);
      return;
    }
    var resource = r.value;
    if (abort) abort.abort();
    var controller = window.AbortController ? new AbortController() : null;
    abort = controller;
    resultCard.hidden = false;
    results.textContent = "";
    var label = progress.querySelector("[data-busy-label]");
    progress.hidden = false;
    label.textContent = validateMessages.running;
    window.hfsBusy.during([runBtn], function () {
      return window.HfsValidate.run(resource, {
        profile: profileInput.value.trim(),
        perEntry: !perEntryOption.hidden && perEntry.checked,
        signal: controller ? controller.signal : undefined,
        onProgress: function (done, total) {
          label.textContent = validateMessages.progress
            .replace("%done%", window.HfsNumber.format(done))
            .replace("%total%", window.HfsNumber.format(total));
        },
      })
        .then(function (result) {
          if (abort !== controller) return;
          progress.hidden = true;
          window.HfsValidate.render(results, result, { resource: resource, messages: validateMessages });
          resultCard.focus({ preventScroll: false });
        })
        .catch(function (e) {
          if (e && e.name === "AbortError") return;
          progress.hidden = true;
          var p = document.createElement("p");
          p.className = "alert";
          p.textContent = validateMessages.requestFailed.replace("%detail%", (e && e.message) || "");
          results.appendChild(p);
        });
    });
  });

  if (textarea.value.trim()) check();
})();
