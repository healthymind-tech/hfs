/*
 * Server-side validation without importing (`$validate`), shared by the
 * Batch / Transaction page and the Validate page.
 *
 * The server operation is HFS's own `POST [base]/[type]/$validate`
 * (crates/rest/src/handlers/validate.rs): it validates the body against the
 * base resource definition, the resource's `meta.profile` claims and an
 * optional `profile` parameter, stores nothing, and always answers 200 with
 * an OperationOutcome for a well-formed request.
 *
 * `window.HfsValidate`:
 *   - `run(resource, options)` -> Promise<result>. `options.profile` is a
 *     canonical passed as `?profile=` for the top-level resource;
 *     `options.perEntry` additionally validates every Bundle entry's
 *     resource on its own `/[type]/$validate` (so each entry is checked
 *     against its own `meta.profile`, which a Bundle-level run need not
 *     descend into); `options.onProgress(done, total)` reports per-entry
 *     progress; `options.signal` aborts.
 *   - `render(container, result, options)` draws the outcome(s), grouped by
 *     severity, with each issue's FHIRPath and, for Bundle paths
 *     (`Bundle.entry[3].resource…`), the entry it belongs to ("entry 4
 *     (Observation/abc)"). `options.messages` holds the localized strings
 *     (`HfsBundleCheck.messagesFrom`).
 *
 * A result is `{ top: Run, entries: [ { index, label, profiles, run } ] }`
 * where a Run is `{ status, outcome, error }`: `outcome` is the returned
 * OperationOutcome (when the server sent one), `error` a transport or HTTP
 * failure text.
 */
(function () {
  "use strict";

  var TENANT = (document.querySelector('meta[name="hfs-tenant"]') || {}).content || "";
  var FHIR_BASE = ((document.querySelector('meta[name="hfs-fhir-base"]') || {}).content || "").replace(/\/+$/, "");
  var CONCURRENCY = 4;
  var SEVERITIES = ["fatal", "error", "warning", "information"];

  function fhirHeaders(extra) {
    var h = { Accept: "application/fhir+json" };
    if (TENANT) h["X-Tenant-ID"] = TENANT;
    if (extra) for (var k in extra) h[k] = extra[k];
    return h;
  }

  function fill(template, values) {
    return String(template || "").replace(/%([a-z_]+)%/g, function (all, name) {
      return Object.prototype.hasOwnProperty.call(values, name) ? String(values[name]) : all;
    });
  }

  function validateOne(resource, profile, signal) {
    var type = resource && resource.resourceType;
    if (!type || !/^[A-Z][A-Za-z]+$/.test(type)) {
      return Promise.resolve({ status: 0, outcome: null, error: "resourceType" });
    }
    var url = FHIR_BASE + "/" + type + "/$validate" + (profile ? "?profile=" + encodeURIComponent(profile) : "");
    return fetch(url, {
      method: "POST",
      headers: fhirHeaders({ "Content-Type": "application/fhir+json" }),
      credentials: "same-origin",
      body: JSON.stringify(resource),
      signal: signal,
    }).then(
      function (response) {
        return response
          .json()
          .catch(function () {
            return null;
          })
          .then(function (body) {
            var outcome = body && body.resourceType === "OperationOutcome" ? body : null;
            return {
              status: response.status,
              outcome: outcome,
              error: response.ok ? null : "HTTP " + response.status,
            };
          });
      },
      function (e) {
        if (e && e.name === "AbortError") throw e;
        return { status: 0, outcome: null, error: (e && e.message) || "network" };
      }
    );
  }

  function entryLabel(entry, index) {
    var n = index + 1;
    var resource = entry && entry.resource;
    var name = "";
    if (resource && resource.resourceType) {
      name = resource.resourceType + (resource.id ? "/" + resource.id : "");
    } else if (entry && entry.request && entry.request.url) {
      name = entry.request.url;
    } else if (entry && entry.fullUrl) {
      name = entry.fullUrl;
    }
    return { n: n, name: name };
  }

  function run(resource, options) {
    options = options || {};
    var signal = options.signal;
    var result = { top: null, entries: [] };
    return validateOne(resource, options.profile || "", signal).then(function (top) {
      result.top = top;
      var entries = resource && resource.resourceType === "Bundle" && Array.isArray(resource.entry) ? resource.entry : [];
      if (!options.perEntry || !entries.length) return result;
      var jobs = [];
      entries.forEach(function (entry, index) {
        if (entry && entry.resource && typeof entry.resource === "object") {
          var profiles = (entry.resource.meta && Array.isArray(entry.resource.meta.profile) && entry.resource.meta.profile) || [];
          jobs.push({ index: index, entry: entry, profiles: profiles });
        }
      });
      var done = 0;
      var cursor = 0;
      if (options.onProgress) options.onProgress(0, jobs.length);
      function worker() {
        if (cursor >= jobs.length) return Promise.resolve();
        var job = jobs[cursor++];
        return validateOne(job.entry.resource, "", signal).then(function (r) {
          result.entries.push({ index: job.index, label: entryLabel(job.entry, job.index), profiles: job.profiles, run: r });
          done++;
          if (options.onProgress) options.onProgress(done, jobs.length);
          return worker();
        });
      }
      var workers = [];
      for (var w = 0; w < Math.min(CONCURRENCY, jobs.length); w++) workers.push(worker());
      return Promise.all(workers).then(function () {
        result.entries.sort(function (a, b) {
          return a.index - b.index;
        });
        return result;
      });
    });
  }

  /* ---- rendering ----------------------------------------------------- */

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  function labelText(label, m) {
    return label.name ? fill(m.entryLabel, { n: label.n, name: label.name }) : fill(m.entryLabelBare, { n: label.n });
  }

  function severityOf(issue) {
    var s = issue && issue.severity;
    return SEVERITIES.indexOf(s) >= 0 ? s : "information";
  }

  function groupOf(severity) {
    return severity === "fatal" ? "error" : severity;
  }

  function issuesOf(outcome) {
    return (outcome && Array.isArray(outcome.issue) ? outcome.issue : []).slice();
  }

  function counts(issues) {
    var c = { error: 0, warning: 0, information: 0 };
    issues.forEach(function (issue) {
      c[groupOf(severityOf(issue))]++;
    });
    return c;
  }

  /* "Bundle.entry[3].resource.name[0]" -> { entry: 3, rest: "resource.name[0]" } */
  function bundlePath(path) {
    var m = /^Bundle\.entry\[(\d+)\](?:\.(.*))?$/.exec(path || "");
    return m ? { entry: parseInt(m[1], 10), rest: m[2] || "" } : null;
  }

  function issueWhere(issue) {
    var expr = (issue.expression && issue.expression[0]) || (issue.location && issue.location[0]) || "";
    return expr;
  }

  function messageOf(issue) {
    return issue.diagnostics || (issue.details && (issue.details.text || (issue.details.coding && issue.details.coding[0] && issue.details.coding[0].display))) || "";
  }

  function issueTable(issues, bundle, m) {
    var wrap = el("div", "table-wrap");
    wrap.tabIndex = 0;
    var table = el("table", "data-table data-table--stack validate-table");
    var thead = el("thead");
    var hr = el("tr");
    [m.colSeverity, m.colLocation, m.colCode, m.colMessage].forEach(function (h) {
      hr.appendChild(el("th", null, h));
    });
    thead.appendChild(hr);
    table.appendChild(thead);
    var tbody = el("tbody");
    issues.forEach(function (issue) {
      var tr = el("tr");
      var sev = severityOf(issue);
      var tdSev = el("td", "validate-table__sev");
      tdSev.setAttribute("data-label", m.colSeverity);
      tdSev.appendChild(el("span", "validate-sev validate-sev--" + groupOf(sev), m["sev" + sev.charAt(0).toUpperCase() + sev.slice(1)] || sev));
      var tdLoc = el("td", "validate-table__loc");
      tdLoc.setAttribute("data-label", m.colLocation);
      var where = issueWhere(issue);
      var bp = bundlePath(where);
      if (bp && bundle && Array.isArray(bundle.entry)) {
        var label = entryLabel(bundle.entry[bp.entry], bp.entry);
        tdLoc.appendChild(el("span", "validate-entry", labelText(label, m)));
      }
      if (where) tdLoc.appendChild(el("code", "validate-path", where));
      else tdLoc.appendChild(el("span", "muted", "—"));
      var tdCode = el("td", "validate-table__code");
      tdCode.setAttribute("data-label", m.colCode);
      tdCode.appendChild(el("code", null, issue.code || ""));
      var tdMsg = el("td", "validate-table__msg", messageOf(issue));
      tdMsg.setAttribute("data-label", m.colMessage);
      tr.appendChild(tdSev);
      tr.appendChild(tdLoc);
      tr.appendChild(tdCode);
      tr.appendChild(tdMsg);
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    wrap.appendChild(table);
    return wrap;
  }

  function verdict(c, m) {
    if (c.error) return { cls: "error", text: fill(m.verdictErrors, { errors: c.error, warnings: c.warning }) };
    if (c.warning) return { cls: "warning", text: fill(m.verdictWarnings, { warnings: c.warning }) };
    return { cls: "ok", text: m.verdictOk };
  }

  function badge(c, m) {
    var v = verdict(c, m);
    return el("span", "validate-badge validate-badge--" + v.cls, v.text);
  }

  /* One OperationOutcome, grouped by severity. */
  function renderRun(target, runResult, bundle, m) {
    if (runResult.error && !runResult.outcome) {
      target.appendChild(el("p", "alert", fill(m.requestFailed, { detail: runResult.error })));
      return;
    }
    if (runResult.error) target.appendChild(el("p", "alert", fill(m.requestFailed, { detail: runResult.error })));
    var issues = issuesOf(runResult.outcome);
    var c = counts(issues);
    var head = el("p", "validate-verdict");
    head.appendChild(badge(c, m));
    target.appendChild(head);
    ["error", "warning", "information"].forEach(function (group) {
      var subset = issues.filter(function (issue) {
        return groupOf(severityOf(issue)) === group;
      });
      if (!subset.length) return;
      var section = el("section", "validate-group validate-group--" + group);
      var h = el("h4", "validate-group__title", (m["group" + group.charAt(0).toUpperCase() + group.slice(1)] || group) + " · " + subset.length);
      section.appendChild(h);
      section.appendChild(issueTable(subset, bundle, m));
      target.appendChild(section);
    });
  }

  function render(container, result, options) {
    options = options || {};
    var m = options.messages || {};
    var bundle = options.resource && options.resource.resourceType === "Bundle" ? options.resource : null;
    container.textContent = "";

    var topCard = el("div", "validate-run");
    topCard.appendChild(el("h4", "validate-run__title", options.resource && options.resource.resourceType === "Bundle" ? m.headingBundle : m.headingResource));
    renderRun(topCard, result.top, bundle, m);
    container.appendChild(topCard);

    if (!result.entries.length) return;
    var list = el("div", "validate-entries");
    var total = { error: 0, warning: 0, information: 0 };
    result.entries.forEach(function (item) {
      var c = counts(issuesOf(item.run.outcome));
      total.error += c.error;
      total.warning += c.warning;
      var details = el("details", "validate-entry-result");
      if (c.error || item.run.error) details.open = true;
      var summary = el("summary", "validate-entry-result__head");
      summary.appendChild(el("span", "validate-entry", labelText(item.label, m)));
      if (item.profiles.length) summary.appendChild(el("code", "validate-entry-result__profiles", item.profiles.join(", ")));
      summary.appendChild(item.run.error && !item.run.outcome ? el("span", "validate-badge validate-badge--error", item.run.error) : badge(c, m));
      details.appendChild(summary);
      var body = el("div", "validate-entry-result__body");
      renderRun(body, item.run, null, m);
      details.appendChild(body);
      list.appendChild(details);
    });
    var heading = el("h4", "validate-run__title", m.headingEntries);
    var overall = el("p", "validate-verdict");
    overall.appendChild(badge(total, m));
    container.appendChild(heading);
    container.appendChild(overall);
    container.appendChild(list);
  }

  window.HfsValidate = { run: run, render: render, bundlePath: bundlePath, entryLabel: entryLabel };
})();
