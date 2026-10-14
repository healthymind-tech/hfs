/*
 * Batch / Transaction workspace (#476, Brett's frames): pick a Bundle JSON,
 * review the execution plan (one row per entry, method chip, collapsible
 * body), execute against the FHIR root, and read the per-action outcomes plus
 * the aggregate result. The bundle's own `type` decides the semantics copy.
 *
 * Two ways in: upload a file, or paste JSON into the editor (CodeMirror via
 * code-editor.js when available, else the plain monospace textarea). Both
 * feed the same client-side checks (bundle-check.js): live with a debounce
 * while typing, again on submit, and on a file once it is read. Errors —
 * each with its line:column — keep the plan out of reach; warnings show but
 * never block.
 */
(function () {
  "use strict";

  var root = document.getElementById("batch");
  if (!root || !window.fetch || !window.hfsBusy) return;
  var messages = root.dataset;
  var hfsBusy = window.hfsBusy;

  /* The effective tenant, stamped by the server (#344); FHIR calls carry it. */
  var TENANT = (document.querySelector('meta[name="hfs-tenant"]') || {}).content || "";
  /* The FHIR API's path prefix, stamped by the server from HFS_BASE_URL
     (`<meta name="hfs-fhir-base">`): "" at the origin root, "/fhir" behind a
     reverse proxy that mounts the server under a prefix. Root-relative FHIR
     paths are built on it; absolute URLs (a Bundle's paging links) and paths
     already carrying the prefix pass through untouched. */
  var FHIR_BASE = ((document.querySelector('meta[name="hfs-fhir-base"]') || {}).content || "")
    .replace(/\/+$/, "");
  function fhirUrl(path) {
    if (!FHIR_BASE || !/^\/(?!\/)/.test(path)) return path;
    if (path === FHIR_BASE || path.indexOf(FHIR_BASE + "/") === 0 || path.indexOf(FHIR_BASE + "?") === 0) {
      return path;
    }
    return FHIR_BASE + path;
  }
  function fhirHeaders(extra) {
    var h = { Accept: "application/fhir+json" };
    if (TENANT) h["X-Tenant-ID"] = TENANT;
    if (extra) for (var k in extra) h[k] = extra[k];
    return h;
  }

  var stages = {
    upload: document.getElementById("batch-upload"),
    preflight: document.getElementById("batch-preflight"),
    response: document.getElementById("batch-response"),
  };
  function show(stage) {
    for (var k in stages) stages[k].hidden = k !== stage;
  }

  var drop = document.getElementById("batch-drop");
  var fileInput = document.getElementById("batch-file");
  var uploadError = document.getElementById("batch-upload-error");
  var requestLine = document.getElementById("batch-request-line");
  var semantics = document.getElementById("batch-semantics");
  var rows = document.getElementById("batch-rows");
  var rawJson = document.getElementById("batch-json");
  var tabActions = document.getElementById("batch-tab-actions");
  var tabJson = document.getElementById("batch-tab-json");
  var executeError = document.getElementById("batch-execute-error");
  var outcomes = document.getElementById("batch-outcomes");
  var overall = document.getElementById("batch-overall");
  var summary = document.getElementById("batch-summary");
  var executeTopBtn = document.getElementById("batch-execute-top");
  var cancelTopBtn = document.getElementById("batch-cancel-top");
  var createdBadge = document.getElementById("batch-created");
  var doneBtn = document.getElementById("batch-done");
  var busyRegion = document.getElementById("batch-busy");
  var modeFile = document.getElementById("batch-mode-file");
  var modePaste = document.getElementById("batch-mode-paste");
  var paneFile = document.getElementById("batch-pane-file");
  var panePaste = document.getElementById("batch-pane-paste");
  var pasteArea = document.getElementById("batch-paste");
  var formatBtn = document.getElementById("batch-format");
  var pasteActions = document.getElementById("batch-paste-actions");
  var continueBtn = document.getElementById("batch-continue");
  var continueBlocked = document.getElementById("batch-continue-blocked");
  var checkPanel = document.getElementById("batch-check");
  var checkOk = document.getElementById("batch-check-ok");
  var checkErrors = document.getElementById("batch-check-errors");
  var checkWarnings = document.getElementById("batch-check-warnings");
  var checkSummary = document.getElementById("batch-check-summary");
  var checkMethods = document.getElementById("batch-check-methods");
  var checkTypes = document.getElementById("batch-check-types");
  var preflightWarnings = document.getElementById("batch-preflight-warnings");

  var Check = window.HfsBundleCheck;
  var checkMessages = Check ? Check.messagesFrom(document.getElementById("bundle-check-messages")) : {};
  /* The last check report: Execute and Continue refuse while it has errors. */
  var lastReport = null;
  var editorView = null;
  var mode = "file";

  var bundle = null;
  var bundleJsonRenderer = null;
  var renderGeneration = 0;
  var renderControllers = [];

  function resetJsonRendering() {
    renderGeneration++;
    renderControllers.forEach(function (controller) { controller.abort(); });
    renderControllers = [];
    return renderGeneration;
  }

  function fallbackJson(target, value) {
    target.textContent = "";
    var pre = document.createElement("pre");
    pre.className = "json-view__fallback";
    pre.textContent = JSON.stringify(value, null, 2);
    target.appendChild(pre);
  }

  function lazyJson(target, value, generation) {
    var state = "idle";
    return function () {
      if (state === "pending" || state === "ready" || state === "fallback" || generation !== renderGeneration) return;
      state = "pending";
      var controller = new AbortController();
      renderControllers.push(controller);
      fetch("/ui/json-view/render", {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "text/html" },
        credentials: "same-origin",
        body: JSON.stringify(value),
        signal: controller.signal,
      })
        .then(function (response) {
          if (!response.ok) throw new Error(String(response.status));
          return response.text();
        })
        .then(function (html) {
          if (generation !== renderGeneration || !target.isConnected) return;
          target.innerHTML = html;
          state = "ready";
        })
        .catch(function (error) {
          if (generation !== renderGeneration || error.name === "AbortError") return;
          fallbackJson(target, value);
          state = "fallback";
        })
        .finally(function () {
          var index = renderControllers.indexOf(controller);
          if (index !== -1) renderControllers.splice(index, 1);
        });
    };
  }

  /* ---- stage 1: pick the file ---------------------------------------- */

  drop.addEventListener("click", function () {
    fileInput.click();
  });
  drop.addEventListener("dragover", function (e) {
    e.preventDefault();
    drop.classList.add("batch-drop--over");
  });
  drop.addEventListener("dragleave", function () {
    drop.classList.remove("batch-drop--over");
  });
  drop.addEventListener("drop", function (e) {
    e.preventDefault();
    drop.classList.remove("batch-drop--over");
    if (e.dataTransfer.files && e.dataTransfer.files[0]) readFile(e.dataTransfer.files[0]);
  });
  fileInput.addEventListener("change", function () {
    if (fileInput.files && fileInput.files[0]) readFile(fileInput.files[0]);
  });

  function fail(message) {
    uploadError.textContent = message;
    uploadError.hidden = false;
  }

  function clearPreflight() {
    rows.textContent = "";
    rawJson.textContent = "";
    bundleJsonRenderer = null;
  }

  /* Yield so the busy region paints before a heavy parse blocks the main
     thread; rAF never fires in a hidden tab, hence the timeout lane. */
  function afterPaint(fn) {
    if (document.hidden) {
      setTimeout(fn, 0);
      return;
    }
    requestAnimationFrame(function () {
      setTimeout(fn, 0);
    });
  }

  function readFile(file) {
    var generation = resetJsonRendering();
    bundle = null;
    clearPreflight();
    show("upload");
    uploadError.hidden = true;
    /* Busy is up before the file is even read (#679): the region reveal is
       synchronous, FileReader delivery is already async. */
    var busy = hfsBusy.region(busyRegion, messages.msgReading);
    var reader = new FileReader();
    /* A folder drop or a file that vanished between pick and read fires
       error/abort, never load — the region must clear and say something. */
    reader.onerror = reader.onabort = function () {
      if (generation !== renderGeneration) return;
      try {
        fail(messages.msgReadFailed);
      } finally {
        busy.done();
      }
    };
    reader.onload = function () {
      if (generation !== renderGeneration) return;
      afterPaint(function () {
        /* Re-checked here, not just above: a second pick during this
           deferred window owns the stage now. */
        if (generation !== renderGeneration) return;
        try {
          var report = runCheck(String(reader.result));
          if (report.errors.length) return;
          bundle = report.value;
          renderPreflight(generation);
          show("preflight");
          /* The drop zone was hidden with its stage, stranding focus.
             Checked by containment, not activeElement === body: the focus
             fixup for a hidden element runs at the next render update, so
             the hidden trigger can still read as active here. Land on the
             revealed stage, not its primary action. */
          var active = document.activeElement;
          if (active === document.body || stages.upload.contains(active)) {
            /* preventScroll (#732): on a plan taller than the viewport the
               default scroll-into-view pinned the Execute row to the top. */
            stages.preflight.focus({ preventScroll: true });
          }
        } finally {
          busy.done();
        }
      });
    };
    reader.readAsText(file);
  }

  /* ---- client-side checks (bundle-check.js) ---------------------------- */

  function positionText(problem) {
    if (problem.line == null) return "";
    return messages.msgCheckAt.replace("%line%", problem.line).replace("%col%", problem.col);
  }

  function fillList(group, problems, clickable) {
    var list = group.querySelector("ul");
    list.textContent = "";
    group.querySelector(".batch-check__count").textContent = problems.length ? "· " + window.HfsNumber.format(problems.length) : "";
    problems.forEach(function (problem) {
      var li = document.createElement("li");
      var where = positionText(problem);
      if (where && clickable && mode === "paste") {
        var jump = document.createElement("button");
        jump.type = "button";
        jump.className = "batch-check__at";
        jump.textContent = where;
        jump.addEventListener("click", function () {
          jumpTo(problem.line, problem.col);
        });
        li.appendChild(jump);
      } else if (where) {
        var at = document.createElement("span");
        at.className = "batch-check__at";
        at.textContent = where;
        li.appendChild(at);
      }
      li.appendChild(document.createTextNode(problem.message));
      list.appendChild(li);
    });
    group.hidden = !problems.length;
  }

  function chips(target, counts) {
    target.textContent = "";
    Object.keys(counts)
      .sort(function (a, b) {
        return counts[b] - counts[a] || (a < b ? -1 : 1);
      })
      .forEach(function (key) {
        var li = document.createElement("li");
        var chip = document.createElement("span");
        var isMethod = target === checkMethods;
        chip.className = isMethod ? "batch-chip batch-chip--" + key.toLowerCase() : "tag";
        chip.textContent = key;
        li.appendChild(chip);
        li.appendChild(document.createTextNode(" " + window.HfsNumber.format(counts[key])));
        target.appendChild(li);
      });
  }

  function renderCheck(report) {
    var empty = report.errors.length === 1 && report.errors[0].code === "empty";
    checkPanel.hidden = empty && mode === "paste";
    if (empty && mode !== "paste") checkPanel.hidden = false;
    fillList(checkErrors, report.errors, true);
    fillList(checkWarnings, report.warnings, true);
    checkOk.hidden = !!(report.errors.length || report.warnings.length);
    if (report.summary && !report.errors.some(function (e) { return e.code === "jsonSyntax" || e.code === "notBundle" || e.code === "notObject"; })) {
      chips(checkMethods, report.summary.byMethod);
      chips(checkTypes, report.summary.byType);
      checkSummary.hidden = !report.summary.entries;
      var total = checkSummary.querySelector(".batch-check__total");
      if (!total) {
        total = document.createElement("p");
        total.className = "batch-check__total";
        checkSummary.insertBefore(total, checkSummary.firstChild);
      }
      total.textContent = messages.msgCheckEntries.replace("%count%", window.HfsNumber.format(report.summary.entries));
    } else {
      checkSummary.hidden = true;
    }
    continueBtn.disabled = !!report.errors.length;
    continueBlocked.hidden = !report.errors.length || empty;
    pushDiagnostics(report);
  }

  function runCheck(text) {
    var report = Check
      ? Check.check(text, { maxBytes: MAX_BODY_SIZE, messages: checkMessages })
      : legacyCheck(text);
    lastReport = report;
    renderCheck(report);
    return report;
  }

  /* bundle-check.js failed to load: the old three checks, unlocated. */
  function legacyCheck(text) {
    var report = { errors: [], warnings: [], summary: null, value: undefined };
    function err(message) {
      report.errors.push({ code: "legacy", message: message, line: null, col: null });
      return report;
    }
    try {
      report.value = JSON.parse(text);
    } catch (e) {
      return err(messages.msgInvalidJson + " (" + e.message + ")");
    }
    if (!report.value || report.value.resourceType !== "Bundle") return err(messages.msgNotABundle);
    if (report.value.type !== "batch" && report.value.type !== "transaction") return err(messages.msgBadType);
    return report;
  }

  function offsetOf(line, col) {
    if (!editorView) return null;
    var doc = editorView.state.doc;
    if (line < 1 || line > doc.lines) return doc.length;
    var l = doc.line(line);
    return Math.min(l.from + Math.max(col - 1, 0), l.to);
  }

  function pushDiagnostics(report) {
    var CM = window.HfsCodeMirror;
    if (!editorView || !CM || !CM.setDiagnostics || mode !== "paste") return;
    var diagnostics = [];
    function add(list, severity) {
      list.forEach(function (problem) {
        if (problem.line == null) return;
        var from = offsetOf(problem.line, problem.col);
        if (from == null) return;
        var to = Math.min(from + 1, editorView.state.doc.length);
        diagnostics.push({ from: from, to: Math.max(to, from), severity: severity, message: problem.message });
      });
    }
    add(report.errors, "error");
    add(report.warnings, "warning");
    try {
      editorView.dispatch(CM.setDiagnostics(editorView.state, diagnostics));
    } catch (e) {
      /* diagnostics are a nicety; the list above is authoritative */
    }
  }

  function jumpTo(line, col) {
    if (editorView) {
      var at = offsetOf(line, col);
      editorView.dispatch({ selection: { anchor: at }, scrollIntoView: true });
      editorView.focus();
      return;
    }
    var text = pasteArea.value;
    var offset = 0;
    for (var l = 1; l < line && offset >= 0; l++) offset = text.indexOf("\n", offset) + 1;
    offset += col - 1;
    pasteArea.focus();
    pasteArea.setSelectionRange(offset, offset + 1);
  }

  /* ---- paste mode --------------------------------------------------------- */

  function mountEditor() {
    if (editorView || !window.HfsCodeEditor || !window.HfsCodeMirror) return;
    var CM = window.HfsCodeMirror;
    var highlight = window.HfsCodeEditor.jsonHighlight();
    editorView = window.HfsCodeEditor.mount(pasteArea, {
      language: CM.json(),
      highlight: highlight ? [CM.syntaxHighlighting(highlight)] : [],
      extensions: CM.lintGutter ? [CM.lintGutter()] : [],
      fold: true,
      wrapperClass: "batch-paste__editor",
      id: "batch-paste-editor",
    });
  }

  function setPasteText(text) {
    if (editorView) {
      editorView.dispatch({ changes: { from: 0, to: editorView.state.doc.length, insert: text } });
    } else {
      pasteArea.value = text;
    }
  }

  function selectMode(which, focus) {
    mode = which;
    var paste = which === "paste";
    modeFile.setAttribute("aria-selected", paste ? "false" : "true");
    modePaste.setAttribute("aria-selected", paste ? "true" : "false");
    modeFile.tabIndex = paste ? -1 : 0;
    modePaste.tabIndex = paste ? 0 : -1;
    paneFile.hidden = paste;
    panePaste.hidden = !paste;
    pasteActions.hidden = !paste;
    uploadError.hidden = true;
    if (paste) {
      mountEditor();
      if (pasteArea.value.trim()) runCheck(pasteArea.value);
      else checkPanel.hidden = true;
    } else {
      checkPanel.hidden = true;
    }
    if (focus) (paste ? modePaste : modeFile).focus();
  }
  modeFile.addEventListener("click", function () { selectMode("file"); });
  modePaste.addEventListener("click", function () { selectMode("paste"); });
  [modeFile, modePaste].forEach(function (tab) {
    tab.addEventListener("keydown", function (e) {
      if (e.key === "ArrowRight" || e.key === "ArrowLeft" || e.key === "Home" || e.key === "End") {
        e.preventDefault();
        var next = e.key === "Home" ? "file" : e.key === "End" ? "paste" : mode === "file" ? "paste" : "file";
        selectMode(next, true);
      }
    });
  });

  var debounce = null;
  pasteArea.addEventListener("input", function () {
    clearTimeout(debounce);
    debounce = setTimeout(function () {
      runCheck(pasteArea.value);
    }, 350);
  });

  formatBtn.addEventListener("click", function () {
    var text = pasteArea.value;
    var formatted;
    try {
      formatted = Check ? Check.format(text) : JSON.stringify(JSON.parse(text), null, 2) + "\n";
    } catch (e) {
      runCheck(text);
      return;
    }
    setPasteText(formatted);
    clearTimeout(debounce);
    runCheck(formatted);
  });

  continueBtn.addEventListener("click", function () {
    clearTimeout(debounce);
    var report = runCheck(pasteArea.value);
    if (report.errors.length) return;
    var generation = resetJsonRendering();
    bundle = report.value;
    clearPreflight();
    renderPreflight(generation);
    show("preflight");
    stages.preflight.focus({ preventScroll: true });
  });

  /* ---- stage 2: the execution plan ------------------------------------ */

  function entries() {
    return Array.isArray(bundle.entry) ? bundle.entry : [];
  }

  function methodOf(entry) {
    return ((entry.request && entry.request.method) || "?").toUpperCase();
  }

  function renderPreflight(generation) {
    var n = entries().length;
    requestLine.textContent =
      "POST [base] · Bundle · " + bundle.type + " · " + window.HfsNumber.format(n) + " " + messages.msgEntries;
    semantics.textContent =
      bundle.type === "transaction" ? messages.msgSemanticsTransaction : messages.msgSemanticsBatch;

    rows.textContent = "";
    entries().forEach(function (entry, i) {
      var li = document.createElement("li");
      li.className = "batch-row";

      var head = document.createElement("button");
      head.type = "button";
      head.className = "batch-row__head";
      head.setAttribute("aria-expanded", "false");

      var num = document.createElement("span");
      num.className = "batch-row__num";
      num.textContent = window.HfsNumber.format(i + 1);

      var method = methodOf(entry);
      var chip = document.createElement("span");
      chip.className = "batch-chip batch-chip--" + method.toLowerCase();
      chip.textContent = method;

      var url = document.createElement("code");
      url.className = "batch-row__url";
      url.textContent = (entry.request && entry.request.url) || "";

      var arrow = document.createElement("span");
      arrow.className = "batch-row__arrow";
      /* The app's chevron icon, template-rendered so the SVG stays vendored
         in templates/icons rather than inlined here (#674). */
      var chevron = document.getElementById("batch-chevron");
      if (chevron) arrow.appendChild(chevron.content.cloneNode(true));

      head.appendChild(num);
      head.appendChild(chip);
      head.appendChild(url);
      head.appendChild(arrow);

      var body = document.createElement("div");
      body.className = "batch-row__body";
      body.hidden = true;
      var showJson = null;
      if (entry.resource) {
        showJson = lazyJson(body, entry.resource, generation);
      } else {
        var noBody = document.createElement("pre");
        noBody.className = "json-view__fallback";
        noBody.textContent = messages.msgNoBody;
        body.appendChild(noBody);
      }

      head.addEventListener("click", function () {
        body.hidden = !body.hidden;
        head.setAttribute("aria-expanded", body.hidden ? "false" : "true");
        if (!body.hidden && showJson) showJson();
      });

      li.appendChild(head);
      li.appendChild(body);
      rows.appendChild(li);
    });

    rawJson.textContent = "";
    bundleJsonRenderer = lazyJson(rawJson, bundle, generation);
    selectTab("actions");

    var warnings = (lastReport && lastReport.warnings) || [];
    var list = preflightWarnings.querySelector("ul");
    list.textContent = "";
    warnings.forEach(function (w) {
      var li = document.createElement("li");
      var where = positionText(w);
      li.textContent = (where ? where + " — " : "") + w.message;
      list.appendChild(li);
    });
    preflightWarnings.hidden = !warnings.length;
    executeTopBtn.disabled = !!(lastReport && lastReport.errors.length);
  }

  function selectTab(which) {
    var actions = which === "actions";
    tabActions.setAttribute("aria-selected", actions ? "true" : "false");
    tabJson.setAttribute("aria-selected", actions ? "false" : "true");
    rows.hidden = !actions;
    rawJson.hidden = actions;
    if (!actions && bundleJsonRenderer) bundleJsonRenderer();
  }
  tabActions.addEventListener("click", function () { selectTab("actions"); });
  tabJson.addEventListener("click", function () { selectTab("json"); });

  function reset() {
    resetJsonRendering();
    bundle = null;
    fileInput.value = "";
    clearPreflight();
    /* Errors are page-level elements, not stage content: hiding a stage
       leaves them set, so an abandoned attempt's error would greet the next
       one (#731). */
    uploadError.hidden = true;
    executeError.hidden = true;
    show("upload");
    if (mode === "paste" && pasteArea.value.trim()) runCheck(pasteArea.value);
    else checkPanel.hidden = true;
    /* Done/Cancel hid the stage that held focus; land on the one action
       the upload stage offers (#679). Containment, not body: the focus
       fixup for the hidden trigger runs at the next render update. */
    var active = document.activeElement;
    if (
      active === document.body ||
      stages.preflight.contains(active) ||
      stages.response.contains(active)
    ) {
      drop.focus();
    }
  }
  cancelTopBtn.addEventListener("click", reset);

  /* ---- stage 3: execute and report ------------------------------------ */

  function executeFailed(detail) {
    executeError.textContent = messages.msgRequestFailed + (detail ? " — " + detail : "");
    executeError.hidden = false;
  }

  /* The server's body limit, stamped by the shell (#1662). */
  var MAX_BODY_SIZE = Number(root.dataset.maxBodySize) || 0;

  function execute() {
    executeError.hidden = true;
    if (!bundle || (lastReport && lastReport.errors.length)) {
      if (!bundle) fail(messages.msgInvalidJson);
      show("upload");
      return;
    }
    /* Auth on, no browser sign-in (#1560): nothing this page sends can be
       authenticated, so say why without sending. A large bundle would
       otherwise be refused mid-upload, and some platforms then report only
       "Failed to fetch" instead of the 401 (#1662). */
    if (document.getElementById("auth-bearer-only")) {
      executeFailed(messages.msgSignInRequired);
      return;
    }
    var payload = JSON.stringify(bundle);
    /* Over the limit the server answers before reading the body, with the
       same mid-upload hazard (#1662): refuse here, with the sizes. */
    var size = new Blob([payload]).size;
    if (MAX_BODY_SIZE && size > MAX_BODY_SIZE) {
      executeFailed(messages.msgTooLarge + " (" + size + " > " + MAX_BODY_SIZE + " bytes)");
      return;
    }
    /* The whole footer goes inert (#679): Execute spins, and Cancel
       disables with it — a mid-flight Cancel nulled `bundle` and
       crashed the settling renderResponse. Busy holds until the outcome is
       rendered, not merely until response headers arrive. */
    hfsBusy.during(
      [executeTopBtn],
      function () {
        return fetch(fhirUrl("/"), {
          method: "POST",
          headers: fhirHeaders({ "Content-Type": "application/fhir+json" }),
          credentials: "same-origin",
          body: payload,
        })
          .then(function (response) {
            return response
              .json()
              .catch(function () { return null; })
              .then(function (body) { renderResponse(response, body); });
          })
          .catch(function (e) {
            executeError.textContent = messages.msgRequestFailed + " (" + e.message + ")";
            executeError.hidden = false;
          });
      },
      { alsoDisable: [cancelTopBtn], region: busyRegion, label: messages.msgExecuting }
    );
  }
  executeTopBtn.addEventListener("click", execute);
  /* The run already happened: Done lands back on a clean upload stage
     rather than offering to re-run a mutation that succeeded (#675). */
  doneBtn.addEventListener("click", function () {
    /* The pasted bundle ran: start the next one from an empty editor. */
    if (mode === "paste") setPasteText("");
    lastReport = null;
    reset();
  });

  function renderResponse(response, body) {
    overall.textContent = String(response.status);
    overall.className = "batch-badge " + (response.ok ? "batch-badge--ok" : "batch-badge--error");

    outcomes.textContent = "";
    var created = 0, updated = 0, other = 0, failed = 0;
    var responded = (body && Array.isArray(body.entry)) ? body.entry : [];

    // The whole-bundle failure case (e.g. a rolled-back transaction): show
    // the OperationOutcome text instead of pretending there are outcomes.
    if (!response.ok && (!responded.length || (body && body.resourceType === "OperationOutcome"))) {
      var diag = "";
      if (body && body.issue && body.issue[0]) {
        diag = body.issue[0].diagnostics || (body.issue[0].details && body.issue[0].details.text) || "";
      }
      // Auth on, no browser sign-in (#1560): the shell notice is the
      // marker, and the missing setting beats the raw header text.
      if (response.status === 401 && document.getElementById("auth-bearer-only")) {
        diag = messages.msgSignInRequired;
      }
      // Over HFS_MAX_BODY_SIZE (#1662): the limit, not the parser's wording.
      if (response.status === 413) {
        diag = messages.msgTooLarge;
      }
      executeFailed(diag);
      return;
    }

    responded.forEach(function (entry, i) {
      var status = (entry.response && entry.response.status) || "";
      var request = entries()[i] || {};
      var li = document.createElement("li");
      li.className = "batch-row";
      var head = document.createElement("div");
      head.className = "batch-row__head batch-row__head--static";

      var num = document.createElement("span");
      num.className = "batch-row__num";
      num.textContent = window.HfsNumber.format(i + 1);
      var method = methodOf(request);
      var chip = document.createElement("span");
      chip.className = "batch-chip batch-chip--" + method.toLowerCase();
      chip.textContent = method;
      var url = document.createElement("code");
      url.className = "batch-row__url";
      url.textContent = (request.request && request.request.url) || "";
      var badge = document.createElement("span");
      var code = parseInt(status, 10) || 0;
      var ok = code >= 200 && code < 300;
      badge.className = "batch-badge " + (ok ? "batch-badge--ok" : "batch-badge--error");
      badge.textContent = status;

      if (!ok) failed++;
      else if (code === 201) created++;
      else if (method === "PUT" || method === "PATCH") updated++;
      else other++;

      head.appendChild(num);
      head.appendChild(chip);
      head.appendChild(url);
      head.appendChild(badge);
      li.appendChild(head);
      outcomes.appendChild(li);
    });

    /* The created count reads in the card head next to the status badge
       (#729); the rest of the tally keeps the summary line, failures above
       all. */
    createdBadge.textContent = created ? window.HfsNumber.format(created) + " " + messages.msgCreated : "";
    var parts = [];
    if (updated) parts.push(window.HfsNumber.format(updated) + " " + messages.msgUpdated);
    if (other) parts.push(window.HfsNumber.format(other) + " " + messages.msgOther);
    if (failed) parts.push(window.HfsNumber.format(failed) + " " + messages.msgFailed);
    summary.textContent = parts.join(" · ");
    summary.hidden = !parts.length;

    show("response");
    /* The disabled trigger was hidden with its stage; land on the one
       action this stage offers (#679). */
    doneBtn.focus();
  }
})();
