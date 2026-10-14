/*
 * IG Packages (/ui/packages and /ui/packages/{name}/{version}).
 *
 * Drives the server's package admin API, `[base]/admin/packages`
 * (crates/rest handlers/admin_packages.rs):
 *   - Installed: GET /admin/packages, GET /admin/packages/jobs
 *   - Browse marketplace: GET /admin/packages/registry/marketplace (the
 *     FHIR/ig-registry index, Taiwan first) and .../registry/search
 *   - Upload: POST /admin/packages with the .tgz as application/gzip
 *   - Install wizard: POST /admin/packages {package, dryRun: true, options}
 *     -> plan job -> dependency tree; POST without dryRun -> install job,
 *     polled through GET /admin/packages/jobs/{id}; DELETE cancels.
 *   - Detail: GET/DELETE /admin/packages/{name}/{version}
 *
 * Strings come from #pkg-messages (data-m-<key> = Fluent packages-<key>),
 * %name% placeholders filled here. Every DOM write uses textContent or
 * element construction: package metadata is remote, untrusted data.
 */
(function () {
  "use strict";

  var TENANT = (document.querySelector('meta[name="hfs-tenant"]') || {}).content || "";
  var FHIR_BASE = ((document.querySelector('meta[name="hfs-fhir-base"]') || {}).content || "").replace(/\/+$/, "");
  var API = FHIR_BASE + "/admin/packages";
  var MSG = (document.getElementById("pkg-messages") || { dataset: {} }).dataset;

  /* ---------------------------------------------------------- helpers */

  function M(key, values) {
    var camel = "m" + key.replace(/(^|-)([a-z0-9])/g, function (_, __, c) {
      return c.toUpperCase();
    });
    var text = MSG[camel];
    if (text == null) text = key;
    if (values) {
      text = text.replace(/%([a-z_]+)%/g, function (all, name) {
        return Object.prototype.hasOwnProperty.call(values, name) ? String(values[name]) : all;
      });
    }
    return text;
  }

  function num(n) {
    return window.HfsNumber && window.HfsNumber.format ? window.HfsNumber.format(n) : String(n);
  }

  function h(tag, attrs, children) {
    var node = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (k) {
        var v = attrs[k];
        if (v == null || v === false) return;
        if (k === "text") node.textContent = v;
        else if (k === "class") node.className = v;
        else if (k.slice(0, 2) === "on") node.addEventListener(k.slice(2), v);
        else node.setAttribute(k, v === true ? "" : v);
      });
    }
    (children || []).forEach(function (c) {
      if (c == null || c === false) return;
      node.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    });
    return node;
  }

  function clear(node) {
    while (node && node.firstChild) node.removeChild(node.firstChild);
    return node;
  }

  function show(node, visible) {
    if (node) node.hidden = !visible;
  }

  function api(path, options) {
    options = options || {};
    var headers = { Accept: "application/json" };
    if (TENANT) headers["X-Tenant-ID"] = TENANT;
    if (options.json !== undefined) headers["Content-Type"] = "application/json";
    if (options.contentType) headers["Content-Type"] = options.contentType;
    return fetch(API + path, {
      method: options.method || "GET",
      headers: headers,
      credentials: "same-origin",
      body: options.json !== undefined ? JSON.stringify(options.json) : options.body,
    }).then(function (response) {
      return response.text().then(function (text) {
        var body = null;
        try {
          body = text ? JSON.parse(text) : null;
        } catch (e) {
          body = null;
        }
        return { status: response.status, ok: response.ok, body: body, text: text };
      });
    });
  }

  function failure(res) {
    if (res.status === 401 || res.status === 403) {
      var diag = res.body && res.body.issue && res.body.issue[0] && res.body.issue[0].diagnostics;
      return diag || M("sign-in");
    }
    if (res.body && res.body.issue && res.body.issue[0]) {
      return res.body.issue[0].diagnostics || ("HTTP " + res.status);
    }
    if (res.body && res.body.error) return res.body.error;
    return "HTTP " + res.status;
  }

  function when(iso) {
    if (!iso) return "";
    var d = new Date(iso);
    if (isNaN(d.getTime())) return iso;
    try {
      return d.toLocaleString(document.documentElement.lang || undefined, {
        year: "numeric",
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
      });
    } catch (e) {
      return d.toISOString();
    }
  }

  function tag(text, kind) {
    return h("span", { class: "tag" + (kind ? " tag--" + kind : ""), text: text });
  }

  function stateTag(status) {
    var kinds = { installed: "complete", installing: "in-progress", failed: "failed", cancelled: "cancelled" };
    return tag(M("state-" + status), kinds[status] || "muted");
  }

  function jobTag(status) {
    var kinds = {
      queued: "muted",
      running: "in-progress",
      succeeded: "complete",
      "completed-with-errors": "conflict",
      failed: "failed",
      cancelled: "cancelled",
    };
    return tag(M("job-" + status), kinds[status] || "muted");
  }

  /* Numeric-aware version order; a pre-release sorts below its release. */
  function compareVersions(a, b) {
    function split(v) {
      var i = v.indexOf("-");
      return i < 0 ? [v, null] : [v.slice(0, i), v.slice(i + 1)];
    }
    function dotted(x, y) {
      var xs = x.split("."), ys = y.split(".");
      for (var i = 0; i < Math.max(xs.length, ys.length); i++) {
        var p = xs[i] || "0", q = ys[i] || "0";
        var np = /^\d+$/.test(p), nq = /^\d+$/.test(q);
        var c = np && nq ? Number(p) - Number(q) : np ? -1 : nq ? 1 : p < q ? -1 : p > q ? 1 : 0;
        if (c) return c;
      }
      return 0;
    }
    var sa = split(a), sb = split(b);
    var c = dotted(sa[0], sb[0]);
    if (c) return c;
    if (sa[1] === null) return sb[1] === null ? 0 : 1;
    if (sb[1] === null) return -1;
    return dotted(sa[1], sb[1]);
  }

  function packageHref(name, version) {
    return "/ui/packages/" + encodeURIComponent(name) + "/" + encodeURIComponent(version);
  }

  function pollJob(id, onUpdate) {
    var stopped = false;
    function tick() {
      if (stopped) return;
      api("/jobs/" + encodeURIComponent(id)).then(function (res) {
        if (stopped) return;
        if (!res.ok || !res.body) {
          onUpdate(null, failure(res));
          return;
        }
        onUpdate(res.body, null);
        var s = res.body.status;
        if (s === "queued" || s === "running") setTimeout(tick, 1000);
      }, function (e) {
        if (!stopped) onUpdate(null, String(e && e.message ? e.message : e));
      });
    }
    tick();
    return function () {
      stopped = true;
    };
  }

  function finished(job) {
    return job && job.status !== "queued" && job.status !== "running";
  }

  /* ------------------------------------------------------------- app */

  function initApp() {
    var config = null;

    // ---- status strip
    api("/config").then(function (res) {
      if (!res.ok || !res.body) return;
      config = res.body;
      var reg = document.getElementById("pkg-status-registry");
      reg.textContent =
        config.registry === "off"
          ? M("registry-off")
          : config.registry === "mirror"
            ? M("registry-mirror", { urls: (config.registrySources || []).join(", ") })
            : M("registry-public");
      var tx = document.getElementById("pkg-status-terminology");
      tx.textContent = config.terminologyServer || M("terminology-none");
      var cache = document.querySelector("#pkg-status-cache code");
      cache.textContent = config.cacheDir || "";
    });

    // ---- tabs
    var tabs = Array.prototype.slice.call(document.querySelectorAll("[data-pkg-tab]"));
    var loaded = {};
    function select(name, focus) {
      tabs.forEach(function (t) {
        var on = t.getAttribute("data-pkg-tab") === name;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        show(document.getElementById(t.getAttribute("aria-controls")), on);
        if (on && focus) t.focus();
      });
      try {
        history.replaceState(null, "", "#" + name);
      } catch (e) {
        /* ignore */
      }
      if (!loaded[name]) {
        loaded[name] = true;
        if (name === "market") loadMarket(true);
      }
    }
    tabs.forEach(function (t, i) {
      t.addEventListener("click", function () {
        select(t.getAttribute("data-pkg-tab"));
      });
      t.addEventListener("keydown", function (e) {
        var next = null;
        if (e.key === "ArrowRight") next = tabs[(i + 1) % tabs.length];
        else if (e.key === "ArrowLeft") next = tabs[(i - 1 + tabs.length) % tabs.length];
        else if (e.key === "Home") next = tabs[0];
        else if (e.key === "End") next = tabs[tabs.length - 1];
        if (next) {
          e.preventDefault();
          select(next.getAttribute("data-pkg-tab"), true);
        }
      });
    });

    // ---- installed
    var installed = [];
    var rows = document.getElementById("pkg-installed-rows");
    var filter = document.getElementById("pkg-installed-filter");
    function renderInstalled() {
      clear(rows);
      var q = (filter.value || "").trim().toLowerCase();
      var list = installed.filter(function (p) {
        return !q || (p.name + " " + (p.title || "")).toLowerCase().indexOf(q) >= 0;
      });
      document.getElementById("pkg-installed-count").textContent = M("installed-count", { count: num(installed.length) });
      if (!list.length) {
        rows.appendChild(h("tr", null, [h("td", { colspan: "7", class: "data-table__empty", text: installed.length ? M("market-empty") : M("installed-empty") })]));
        return;
      }
      list.forEach(function (p) {
        var deps = Object.keys(p.resolved || p.dependencies || {}).filter(function (d) {
          var v = (p.resolved || {})[d];
          return v !== "embedded" && v !== "skipped";
        });
        rows.appendChild(
          h("tr", null, [
            h("td", { class: "col-name", "data-label": M("col-name") }, [
              h("a", { href: packageHref(p.name, p.version), text: p.name }),
              p.title ? h("div", { class: "muted pkg-subtitle", text: p.title }) : null,
            ]),
            h("td", { "data-label": M("col-version") }, [h("code", { text: p.version })]),
            h("td", { "data-label": M("col-fhir"), text: (p.fhirVersions || []).join(", ") }),
            h("td", { "data-label": M("col-installed"), text: when(p.installedAt) }),
            h("td", { class: "num", "data-label": M("col-resources"), text: num(p.resourceCount || 0) }),
            h("td", { "data-label": M("col-dependencies") }, [
              h("span", { title: deps.join("\n"), text: num(deps.length) }),
              p.dependents && p.dependents.length ? h("div", { class: "muted pkg-subtitle", text: M("detail-dependents") + ": " + p.dependents.join(", ") }) : null,
            ]),
            h("td", { "data-label": M("col-status") }, [stateTag(p.status), " ", tag(p.explicit ? M("explicit") : M("dependency-of"), "muted")]),
          ])
        );
      });
    }
    function loadInstalled() {
      return api("").then(function (res) {
        if (!res.ok || !res.body) {
          clear(rows).appendChild(h("tr", null, [h("td", { colspan: "7", class: "data-table__empty", text: M("request-failed", { detail: failure(res) }) })]));
          return;
        }
        installed = res.body.packages || [];
        renderInstalled();
      });
    }
    filter.addEventListener("input", renderInstalled);

    // ---- jobs
    var jobsList = document.getElementById("pkg-jobs");
    function loadJobs() {
      return api("/jobs").then(function (res) {
        clear(jobsList);
        var jobs = (res.body && res.body.jobs) || [];
        if (!jobs.length) {
          jobsList.appendChild(h("li", { class: "muted", text: M("jobs-empty") }));
          return;
        }
        jobs.slice(0, 15).forEach(function (j) {
          var progress = j.resourcesTotal ? " · " + M("progress-resources", { done: num(j.resourcesDone), total: num(j.resourcesTotal) }) : "";
          var item = h("li", { class: "pkg-job" }, [
            tag(M("job-" + j.kind), "type"),
            h("code", { text: j.package }),
            jobTag(j.status),
            h("span", { class: "muted", text: when(j.createdAt) + progress + (j.errors ? " · " + M("errors-heading") + " " + num(j.errors) : "") }),
          ]);
          if (j.kind === "install" && (j.status === "running" || j.status === "queued")) {
            item.appendChild(
              h("button", {
                type: "button",
                class: "btn btn--small",
                text: M("step-progress"),
                onclick: function () {
                  wizard.watch(j.id, j.package);
                },
              })
            );
          }
          jobsList.appendChild(item);
        });
      });
    }

    // ---- marketplace
    var marketForm = document.getElementById("pkg-market-form");
    var marketResults = document.getElementById("pkg-market-results");
    var marketCount = document.getElementById("pkg-market-count");
    var marketError = document.getElementById("pkg-market-error");
    var moreButton = document.getElementById("pkg-market-more");
    var registryBox = document.getElementById("pkg-registry");
    var registryList = document.getElementById("pkg-registry-results");
    var facetsFilled = false;
    var marketOffset = 0;
    var marketNames = {};
    var searchTimer = null;

    function marketQuery(offset) {
      var data = new FormData(marketForm);
      var params = new URLSearchParams();
      data.forEach(function (v, k) {
        if (v) params.set(k, v);
      });
      params.set("offset", String(offset));
      params.set("limit", "24");
      return params;
    }

    function fillFacets(facets) {
      if (facetsFilled || !facets) return;
      facetsFilled = true;
      [
        ["pkg-market-country", facets.countries],
        ["pkg-market-category", facets.categories],
        ["pkg-market-authority", facets.authorities],
      ].forEach(function (pair) {
        var select = document.getElementById(pair[0]);
        var entries = Object.keys(pair[1] || {})
          .filter(function (k) {
            return k;
          })
          .sort(function (a, b) {
            return (pair[1][b] - pair[1][a]) || a.localeCompare(b);
          });
        entries.forEach(function (k) {
          select.appendChild(h("option", { value: k, text: (pair[0] === "pkg-market-country" ? k.toUpperCase() : k) + " (" + pair[1][k] + ")" }));
        });
      });
    }

    function card(entry) {
      var versions = (entry.editions || [])
        .map(function (e) {
          return e.version;
        })
        .filter(function (v, i, a) {
          return v && a.indexOf(v) === i;
        })
        .sort(compareVersions)
        .reverse();
      var select = h("select", { class: "field__input pkg-card__version", "aria-label": M("install-version") },
        versions.map(function (v) {
          return h("option", { value: v, text: v });
        }));
      var meta = [entry.authority, entry.country ? entry.country.toUpperCase() : null].filter(Boolean).join(" · ");
      return h("li", { class: "pkg-card" }, [
        h("div", { class: "pkg-card__head" }, [
          h("h4", { class: "pkg-card__title", text: entry.title || entry.name }),
          entry.category ? tag(entry.category, "type") : null,
        ]),
        h("code", { class: "pkg-card__name", text: entry.name }),
        meta ? h("p", { class: "muted pkg-card__meta", text: meta }) : null,
        entry.description ? h("p", { class: "pkg-card__desc", text: entry.description }) : null,
        entry.canonical ? h("p", { class: "pkg-card__canonical muted" }, [h("code", { text: entry.canonical })]) : null,
        h("p", { class: "pkg-card__versions" }, [
          entry.latest ? h("span", { text: M("latest", { version: entry.latest }) }) : null,
          entry.installed && entry.installed.length ? tag(M("installed-versions", { versions: entry.installed.join(", ") }), "complete") : null,
        ]),
        h("div", { class: "pkg-card__actions" }, [
          versions.length ? select : null,
          h("button", {
            type: "button",
            class: "btn btn--primary",
            text: M("install"),
            onclick: function () {
              wizard.open(entry.name, select.value || entry.latest || null);
            },
          }),
        ]),
      ]);
    }

    function loadMarket(reset) {
      if (reset) marketOffset = 0;
      var params = marketQuery(marketOffset);
      return api("/registry/marketplace?" + params.toString()).then(function (res) {
        if (reset) {
          clear(marketResults);
          marketNames = {};
        }
        var body = res.body || {};
        if (!res.ok || body.available === false) {
          marketError.textContent = M("market-unavailable", { detail: body.error || failure(res) });
          show(marketError, true);
        } else {
          show(marketError, false);
        }
        fillFacets(body.facets);
        (body.results || []).forEach(function (e) {
          marketNames[e.name] = true;
          marketResults.appendChild(card(e));
        });
        marketOffset += (body.results || []).length;
        marketCount.textContent = body.total ? M("market-count", { count: num(body.total) }) : body.available === false ? "" : M("market-empty");
        show(moreButton, marketOffset < (body.total || 0));
        var q = document.getElementById("pkg-market-q").value.trim();
        if (reset) loadRegistry(q);
      });
    }

    function loadRegistry(q) {
      clear(registryList);
      if (q.length < 2) {
        show(registryBox, false);
        return;
      }
      var params = new URLSearchParams({ q: q });
      var fhir = document.getElementById("pkg-market-fhir").value;
      if (fhir) params.set("fhirversion", fhir);
      api("/registry/search?" + params.toString()).then(function (res) {
        var results = ((res.body && res.body.results) || []).filter(function (r) {
          return !marketNames[r.name];
        });
        show(registryBox, true);
        if (!results.length) {
          registryList.appendChild(h("li", { class: "muted", text: M("registry-results-empty") }));
          return;
        }
        results.slice(0, 40).forEach(function (r) {
          registryList.appendChild(
            h("li", { class: "pkg-registry__item" }, [
              h("div", null, [
                h("code", { text: r.name }),
                r.fhirVersion ? " " : null,
                r.fhirVersion ? tag(r.fhirVersion, "muted") : null,
                r.installed && r.installed.length ? tag(M("installed-versions", { versions: r.installed.join(", ") }), "complete") : null,
                r.description ? h("div", { class: "muted pkg-subtitle", text: r.description }) : null,
              ]),
              h("button", {
                type: "button",
                class: "btn",
                text: M("install"),
                onclick: function () {
                  wizard.open(r.name, null);
                },
              }),
            ])
          );
        });
      });
    }

    marketForm.addEventListener("submit", function (e) {
      e.preventDefault();
      loadMarket(true);
    });
    marketForm.addEventListener("change", function () {
      loadMarket(true);
    });
    document.getElementById("pkg-market-q").addEventListener("input", function () {
      clearTimeout(searchTimer);
      searchTimer = setTimeout(function () {
        loadMarket(true);
      }, 300);
    });
    moreButton.addEventListener("click", function () {
      loadMarket(false);
    });

    // ---- install by name
    var byName = document.getElementById("pkg-byname");
    var byNameName = document.getElementById("pkg-byname-name");
    var byNameVersion = document.getElementById("pkg-byname-version");
    var versionsList = document.getElementById("pkg-byname-versions");
    byNameName.addEventListener("change", function () {
      clear(versionsList);
      var name = byNameName.value.trim();
      if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name)) return;
      api("/registry/versions/" + encodeURIComponent(name)).then(function (res) {
        ((res.body && res.body.versions) || []).forEach(function (v) {
          versionsList.appendChild(h("option", { value: v.version }));
        });
      });
    });
    byName.addEventListener("submit", function (e) {
      e.preventDefault();
      var name = byNameName.value.trim();
      if (!byNameName.checkValidity() || !name) {
        byNameName.reportValidity();
        return;
      }
      wizard.open(name, byNameVersion.value.trim() || null);
    });

    // ---- upload
    var drop = document.getElementById("pkg-drop");
    var fileInput = document.getElementById("pkg-file");
    var uploadStatus = document.getElementById("pkg-upload-status");
    var uploadError = document.getElementById("pkg-upload-error");
    function upload(file) {
      if (!file) return;
      show(uploadError, false);
      uploadStatus.querySelector("[data-busy-label]").textContent = M("upload-reading", { name: file.name });
      show(uploadStatus, true);
      file.arrayBuffer().then(function (buffer) {
        return api("?wait=55", { method: "POST", contentType: "application/gzip", body: buffer });
      }).then(function (res) {
        show(uploadStatus, false);
        fileInput.value = "";
        if (!res.ok || !res.body || !res.body.id) {
          uploadError.textContent = M("upload-failed", { detail: failure(res) });
          show(uploadError, true);
          return;
        }
        var spec = res.body.package || "";
        var parts = spec.split("#");
        wizard.fromPlanJob(res.body, parts[0], parts[1] || null);
      }, function (e) {
        show(uploadStatus, false);
        uploadError.textContent = M("upload-failed", { detail: String(e && e.message ? e.message : e) });
        show(uploadError, true);
      });
    }
    drop.addEventListener("click", function () {
      fileInput.click();
    });
    fileInput.addEventListener("change", function () {
      upload(fileInput.files && fileInput.files[0]);
    });
    ["dragenter", "dragover"].forEach(function (type) {
      drop.addEventListener(type, function (e) {
        e.preventDefault();
        drop.classList.add("batch-drop--over");
      });
    });
    ["dragleave", "drop"].forEach(function (type) {
      drop.addEventListener(type, function (e) {
        e.preventDefault();
        drop.classList.remove("batch-drop--over");
      });
    });
    drop.addEventListener("drop", function (e) {
      upload(e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files[0]);
    });

    // ---- wizard
    var wizard = makeWizard({
      onClose: function () {
        loadInstalled();
        loadJobs();
      },
    });

    // ---- boot
    var initial = (location.hash || "").replace("#", "");
    select(["installed", "market", "upload"].indexOf(initial) >= 0 ? initial : "installed");
    loadInstalled();
    loadJobs();
    var params = new URLSearchParams(location.search);
    if (params.get("install")) wizard.open(params.get("install"), params.get("version") || null);
  }

  /* ---------------------------------------------------------- wizard */

  function makeWizard(hooks) {
    var root = document.getElementById("pkg-wizard");
    var views = document.getElementById("pkg-views");
    var title = document.getElementById("pkg-wizard-title");
    var steps = ["tree", "options", "progress", "result"];
    var state = { name: null, version: null, plan: null, job: null, skip: {}, stop: null, report: null };
    var optionsForm = document.getElementById("pkg-options");

    function step(name) {
      steps.forEach(function (s) {
        show(document.getElementById("pkg-step-" + s), s === name);
        var li = root.querySelector('.pkg-steps [data-step="' + s + '"]');
        if (!li) return;
        var idx = steps.indexOf(s);
        var cur = steps.indexOf(name);
        li.className = idx < cur ? "is-done" : idx === cur ? "is-current" : "";
        if (idx === cur) li.setAttribute("aria-current", "step");
        else li.removeAttribute("aria-current");
      });
    }

    function options() {
      var data = new FormData(optionsForm);
      return {
        includeDependencies: !!data.get("includeDependencies"),
        includeExamples: !!data.get("includeExamples"),
        mode: data.get("mode") || "upgrade",
        dependencyPolicy: data.get("dependencyPolicy") || "newer",
        terminology: data.get("terminology") || "auto",
        skip: Object.keys(state.skip).filter(function (k) {
          return state.skip[k];
        }),
      };
    }

    function spec() {
      return state.version ? state.name + "#" + state.version : state.name;
    }

    function openShell(name, version) {
      if (state.stop) state.stop();
      state = { name: name, version: version, plan: null, job: null, skip: {}, stop: null, report: null };
      title.textContent = M("wizard-heading", { package: spec() });
      show(views, false);
      show(root, true);
      root.focus();
    }

    function close() {
      if (state.stop) state.stop();
      state.stop = null;
      show(root, false);
      show(views, true);
      try {
        var url = new URL(location.href);
        url.searchParams.delete("install");
        url.searchParams.delete("version");
        history.replaceState(null, "", url.pathname + url.search + url.hash);
      } catch (e) {
        /* ignore */
      }
      if (hooks.onClose) hooks.onClose();
    }

    function planInto(job) {
      var busy = document.getElementById("pkg-plan-busy");
      var error = document.getElementById("pkg-plan-error");
      if (!finished(job)) {
        state.stop = pollJob(job.id, function (j, err) {
          if (err) {
            show(busy, false);
            error.textContent = M("plan-failed", { detail: err });
            show(error, true);
            return;
          }
          if (finished(j)) planInto(j);
        });
        return;
      }
      show(busy, false);
      var result = job.result || {};
      if (job.status === "failed" || !result.plan) {
        error.textContent = M("plan-failed", { detail: result.error || (job.errors[0] && job.errors[0].message) || job.status });
        show(error, true);
        return;
      }
      state.plan = result;
      var rootKey = result.plan.root || "";
      if (!state.version && rootKey.indexOf("#") > 0) {
        state.version = rootKey.split("#")[1];
        title.textContent = M("wizard-heading", { package: spec() });
      }
      renderTree(result);
      document.getElementById("pkg-to-options").disabled = !(result.plan.installOrder || []).length;
    }

    function runPlan() {
      step("tree");
      show(document.getElementById("pkg-plan-busy"), true);
      show(document.getElementById("pkg-plan-error"), false);
      clear(document.getElementById("pkg-tree"));
      document.getElementById("pkg-plan-totals").textContent = "";
      document.getElementById("pkg-to-options").disabled = true;
      api("?wait=50", { method: "POST", json: { package: spec(), dryRun: true, options: options() } }).then(function (res) {
        if (!res.body || !res.body.id) {
          show(document.getElementById("pkg-plan-busy"), false);
          var error = document.getElementById("pkg-plan-error");
          error.textContent = M("plan-failed", { detail: failure(res) });
          show(error, true);
          return;
        }
        planInto(res.body);
      });
    }

    function actionText(action) {
      switch (action.kind) {
        case "install":
          return [M("action-install"), "complete"];
        case "reinstall":
          return [M("action-reinstall"), "in-progress"];
        case "upgrade":
          return [M("action-upgrade", { from: (action.from || []).join(", ") }), "in-progress"];
        case "satisfied":
          return [M("action-satisfied", { by: action.by }), "muted"];
        case "embedded":
          return [M("action-embedded"), "embedded"];
        case "skipped":
          return [M("action-skipped"), "excluded"];
        default:
          return [M("action-missing"), "failed"];
      }
    }

    function sum(obj) {
      return Object.keys(obj || {}).reduce(function (a, k) {
        return a + obj[k];
      }, 0);
    }

    function renderTree(result) {
      var plan = result.plan;
      var reports = {};
      (result.packages || []).forEach(function (p) {
        reports[p.package] = p;
      });
      var byKey = {};
      plan.nodes.forEach(function (n) {
        byKey[n.version ? n.name + "#" + n.version : n.name] = n;
      });
      var children = {};
      plan.nodes.forEach(function (n) {
        var key = n.version ? n.name + "#" + n.version : n.name;
        (n.required || []).forEach(function (r) {
          (children[r.parent] = children[r.parent] || []).push(key);
        });
      });
      var seen = {};
      function item(key, depth) {
        var n = byKey[key];
        if (!n) return null;
        var a = actionText(n.action);
        var report = reports[key];
        var again = seen[key];
        seen[key] = true;
        var lines = [];
        if (report && !again) {
          var c = report.counts || {};
          if (sum(c.hfs)) lines.push(M("counts-hfs", { count: num(sum(c.hfs)) }));
          if (sum(c.hts)) lines.push(M("counts-hts", { count: num(sum(c.hts)) }));
          if (sum(c.examplesSkipped)) lines.push(M("counts-examples", { count: num(sum(c.examplesSkipped)) }));
          var conflicts = (report.conflicts && report.conflicts.counts) || {};
          Object.keys(conflicts).forEach(function (k) {
            lines.push(M("conflict-" + k, { count: num(conflicts[k]) }));
          });
          if (report.error) lines.push(M("plan-package-error", { package: key, detail: report.error }));
        }
        var skippable = depth > 0 && n.action && (n.action.kind === "install" || n.action.kind === "upgrade" || n.action.kind === "reinstall");
        var head = h("div", { class: "pkg-node__head" }, [
          h("code", { class: "pkg-node__name", text: n.name + (n.version ? "#" + n.version : "") }),
          tag(a[0], a[1]),
          n.title && !again ? h("span", { class: "muted pkg-node__title", text: n.title }) : null,
        ]);
        if (skippable && !again) {
          var box = h("input", { type: "checkbox" });
          box.checked = !!state.skip[n.name];
          box.addEventListener("change", function () {
            state.skip[n.name] = box.checked;
          });
          head.appendChild(h("label", { class: "pkg-check pkg-node__skip" }, [box, " " + M("skip-dependency")]));
        }
        var li = h("li", { class: "pkg-node" + (again ? " pkg-node--repeat" : "") }, [head]);
        if (lines.length) li.appendChild(h("p", { class: "muted pkg-node__counts", text: lines.join(" · ") }));
        if (!again && children[key] && children[key].length) {
          var ul = h("ul", { class: "pkg-tree__children" });
          children[key].forEach(function (child) {
            var c = item(child, depth + 1);
            if (c) ul.appendChild(c);
          });
          li.appendChild(ul);
        }
        return li;
      }
      var tree = clear(document.getElementById("pkg-tree"));
      var top = item(plan.root, 0);
      if (top) tree.appendChild(top);
      var totals = result.totals || {};
      document.getElementById("pkg-plan-totals").textContent = M("plan-totals", {
        packages: num((plan.installOrder || []).length),
        hfs: num(totals.hfs || 0),
        hts: num(totals.hts || 0),
      });
      var error = document.getElementById("pkg-plan-error");
      var warnings = (plan.warnings || []).map(function (w) {
        return M("plan-warning", { detail: w });
      });
      if (warnings.length) {
        error.textContent = warnings.join("\n");
        show(error, true);
      }
    }

    function renderProgress(job) {
      document.getElementById("pkg-phase").textContent = (job.current ? job.current + " — " : "") + job.phase;
      function bar(id, labelId, done, total, label) {
        var el = document.getElementById(id);
        var pct = total ? Math.round((done / total) * 100) : 0;
        el.querySelector(".progress-fill").style.width = pct + "%";
        el.setAttribute("aria-valuenow", String(pct));
        el.setAttribute("aria-valuemin", "0");
        el.setAttribute("aria-valuemax", "100");
        document.getElementById(labelId).textContent = label;
      }
      bar("pkg-progress-packages", "pkg-progress-packages-label", job.packagesDone, job.packagesTotal,
        M("progress-packages", { done: num(Math.min(job.packagesDone + (finished(job) ? 0 : 1), job.packagesTotal || 0)), total: num(job.packagesTotal || 0) }));
      bar("pkg-progress-resources", "pkg-progress-resources-label", job.resourcesDone, job.resourcesTotal,
        M("progress-resources", { done: num(job.resourcesDone), total: num(job.resourcesTotal) }));
      var tbody = clear(document.getElementById("pkg-counts"));
      Object.keys(job.counts || {}).forEach(function (pkg) {
        var types = job.counts[pkg];
        Object.keys(types).forEach(function (t) {
          var c = types[t];
          tbody.appendChild(
            h("tr", null, [
              h("td", { class: "col-name", "data-label": M("col-package") }, [h("code", { text: pkg })]),
              h("td", { "data-label": M("col-type"), text: t }),
              h("td", { class: "num", "data-label": M("col-written"), text: num(c.written) }),
              h("td", { class: "num", "data-label": M("col-failed"), text: num(c.failed) }),
              h("td", { class: "num", "data-label": M("col-skipped"), text: num(c.skipped) }),
            ])
          );
        });
      });
      document.getElementById("pkg-log").textContent = (job.log || []).join("\n");
    }

    function renderResult(job) {
      step("result");
      state.report = job;
      var verdict = clear(document.getElementById("pkg-verdict"));
      var result = job.result || {};
      var installed = (result.installed || []).length;
      var text;
      var kind;
      if (job.status === "succeeded") {
        text = M("result-ok", { count: num(installed) });
        kind = "ok";
      } else if (job.status === "cancelled") {
        text = M("result-cancelled");
        kind = "warn";
      } else if (job.status === "failed") {
        text = M("result-failed", { detail: result.error || (job.errors[0] && job.errors[0].message) || "" });
        kind = "error";
      } else {
        text = M("result-errors", { count: num(installed), errors: num(job.errors.length + (job.errorsDropped || 0)) });
        kind = "warn";
      }
      verdict.className = "pkg-verdict pkg-verdict--" + kind;
      verdict.appendChild(h("strong", { text: text }));
      if (installed) verdict.appendChild(h("p", { class: "muted", text: result.installed.join(", ") }));

      var notes = clear(document.getElementById("pkg-notes"));
      if (result.searchParameters) {
        notes.appendChild(h("p", { text: M("reindex-hint", { count: num(result.searchParameters) }) }));
        var reindex = h("button", { type: "button", class: "btn", text: M("reindex") });
        reindex.addEventListener("click", function () {
          reindex.disabled = true;
          var headers = { Accept: "application/fhir+json", Prefer: "respond-async" };
          if (TENANT) headers["X-Tenant-ID"] = TENANT;
          fetch(FHIR_BASE + "/$reindex", { method: "POST", headers: headers, credentials: "same-origin" }).then(function (r) {
            notes.appendChild(h("p", { class: "muted", text: r.ok ? M("reindex-started") : M("request-failed", { detail: "HTTP " + r.status }) }));
          });
        });
        notes.appendChild(reindex);
        show(notes, true);
      } else {
        show(notes, false);
      }

      var tbody = clear(document.getElementById("pkg-error-rows"));
      (job.errors || []).slice(0, 300).forEach(function (e) {
        tbody.appendChild(
          h("tr", null, [
            h("td", { class: "col-name", "data-label": M("col-package") }, [h("code", { text: e.package })]),
            h("td", { "data-label": M("col-resource"), text: e.resource }),
            h("td", { "data-label": M("col-message"), text: e.message }),
          ])
        );
      });
      show(document.getElementById("pkg-errors"), (job.errors || []).length > 0);
    }

    function watch(id, label) {
      if (label && !state.name) {
        var parts = label.split("#");
        openShell(parts[0], parts[1] || null);
      }
      step("progress");
      if (state.stop) state.stop();
      state.stop = pollJob(id, function (job, err) {
        if (err) {
          document.getElementById("pkg-phase").textContent = M("request-failed", { detail: err });
          return;
        }
        state.job = job;
        renderProgress(job);
        if (finished(job)) renderResult(job);
      });
    }

    document.getElementById("pkg-wizard-close").addEventListener("click", close);
    Array.prototype.forEach.call(root.querySelectorAll("[data-pkg-close]"), function (b) {
      b.addEventListener("click", close);
    });
    document.getElementById("pkg-to-options").addEventListener("click", function () {
      step("options");
    });
    document.getElementById("pkg-back-tree").addEventListener("click", function () {
      step("tree");
    });
    document.getElementById("pkg-replan").addEventListener("click", runPlan);
    document.getElementById("pkg-start").addEventListener("click", function () {
      var start = document.getElementById("pkg-start");
      start.disabled = true;
      api("", { method: "POST", json: { package: spec(), options: options() } }).then(function (res) {
        start.disabled = false;
        if (!res.body || !res.body.id) {
          step("tree");
          var error = document.getElementById("pkg-plan-error");
          error.textContent = M("request-failed", { detail: failure(res) });
          show(error, true);
          return;
        }
        watch(res.body.id);
      });
    });
    document.getElementById("pkg-stop").addEventListener("click", function () {
      if (!state.job) return;
      api("/jobs/" + encodeURIComponent(state.job.id), { method: "DELETE" });
    });
    document.getElementById("pkg-download").addEventListener("click", function () {
      if (!state.report) return;
      var blob = new Blob([JSON.stringify(state.report, null, 2)], { type: "application/json" });
      var a = h("a", { href: URL.createObjectURL(blob), download: "package-install-" + (state.report.package || "report").replace(/[^A-Za-z0-9._-]+/g, "_") + ".json" });
      document.body.appendChild(a);
      a.click();
      setTimeout(function () {
        URL.revokeObjectURL(a.href);
        a.remove();
      }, 0);
    });
    document.getElementById("pkg-done").addEventListener("click", function () {
      close();
      var tab = document.getElementById("pkg-tab-installed");
      if (tab) tab.click();
    });
    optionsForm.addEventListener("change", function () {
      document.getElementById("pkg-replan").classList.add("btn--primary");
    });

    return {
      open: function (name, version) {
        openShell(name, version);
        runPlan();
      },
      fromPlanJob: function (job, name, version) {
        openShell(name, version);
        step("tree");
        show(document.getElementById("pkg-plan-busy"), true);
        show(document.getElementById("pkg-plan-error"), false);
        planInto(job);
      },
      watch: function (id, label) {
        state.name = null;
        watch(id, label);
      },
    };
  }

  /* ---------------------------------------------------------- detail */

  function initDetail() {
    var root = document.getElementById("pkg-detail");
    var name = root.getAttribute("data-name");
    var version = root.getAttribute("data-version");
    var busy = document.getElementById("pkg-detail-busy");
    var error = document.getElementById("pkg-detail-error");
    var body = document.getElementById("pkg-detail-body");
    var detail = null;

    function kv(dl, label, value) {
      if (value == null || value === "" || (Array.isArray(value) && !value.length)) return;
      dl.appendChild(h("div", { class: "detail__field" }, [h("dt", { text: label }), h("dd", null, [typeof value === "string" ? value : value])]));
    }

    function resourcesTable(type, rows) {
      var tbody = h("tbody");
      rows.forEach(function (r) {
        var idCell = h("td", { class: "col-name", "data-label": M("col-id") });
        if (r.target === "hfs" && r.id) {
          idCell.appendChild(h("a", { href: "/ui/resources?url=" + encodeURIComponent(type + "?_id=" + r.id), text: r.id }));
        } else {
          idCell.appendChild(h("span", { class: "muted", text: "—" }));
        }
        var note = [];
        if (r.preexisting) note.push(M("res-preexisting"));
        if (r.sharedWith && r.sharedWith.length) note.push(M("res-shared", { packages: r.sharedWith.join(", ") }));
        tbody.appendChild(
          h("tr", null, [
            idCell,
            h("td", { "data-label": M("col-url") }, [r.url ? h("code", { text: r.url + (r.version ? "|" + r.version : "") }) : ""]),
            h("td", { "data-label": M("col-where"), text: r.target === "hts" ? M("target-hts") : M("target-hfs") }),
            h("td", { "data-label": M("col-note"), text: note.join(" · ") }),
          ])
        );
      });
      return h("div", { class: "table-wrap", tabindex: "0" }, [
        h("table", { class: "data-table data-table--stack" }, [
          h("thead", null, [h("tr", null, [
            h("th", { scope: "col", text: M("col-id") }),
            h("th", { scope: "col", text: M("col-url") }),
            h("th", { scope: "col", text: M("col-where") }),
            h("th", { scope: "col", text: M("col-note") }),
          ])]),
          tbody,
        ]),
      ]);
    }

    function render(d) {
      detail = d;
      document.getElementById("pkg-detail-lede").textContent = d.title || d.description || "";
      var meta = clear(document.getElementById("pkg-meta"));
      kv(meta, M("meta-status"), stateTag(d.status));
      kv(meta, M("meta-title"), d.title || "");
      kv(meta, M("meta-canonical"), d.canonical ? h("code", { text: d.canonical }) : "");
      kv(meta, M("meta-fhir"), (d.fhirVersions || []).join(", "));
      kv(meta, M("meta-installed-at"), when(d.installedAt));
      kv(meta, M("meta-source"), d.source ? h("code", { text: d.source }) : "");
      kv(meta, M("meta-sha1"), d.sha1 ? h("span", null, [h("code", { text: d.sha1 }), d.shasumVerified ? " · " + M("meta-verified") : ""]) : "");
      kv(meta, M("meta-requested-by"), d.requestedBy || "");

      var deps = clear(document.getElementById("pkg-deps"));
      var resolved = d.resolved || {};
      var depNames = Object.keys(d.dependencies || {});
      if (!depNames.length) deps.appendChild(h("li", { class: "muted", text: M("detail-no-dependencies") }));
      depNames.forEach(function (dep) {
        var target = resolved[dep] || "";
        var label;
        if (target === "embedded") label = tag(M("dep-embedded"), "embedded");
        else if (target === "skipped") label = tag(M("dep-skipped"), "excluded");
        else if (target.indexOf("#") > 0) {
          var p = target.split("#");
          label = h("a", { href: packageHref(p[0], p[1]), text: target });
        } else label = tag(M("dep-missing"), "failed");
        deps.appendChild(h("li", null, [h("code", { text: dep + "#" + d.dependencies[dep] }), " → ", label]));
      });
      var dependents = clear(document.getElementById("pkg-dependents"));
      if (!(d.dependents || []).length) dependents.appendChild(h("li", { class: "muted", text: M("detail-no-dependents") }));
      (d.dependents || []).forEach(function (k) {
        var p = k.split("#");
        dependents.appendChild(h("li", null, [h("a", { href: packageHref(p[0], p[1]), text: k })]));
      });

      document.getElementById("pkg-resources-count").textContent = M("detail-resources-count", { count: num(d.resourceCount || 0) });
      var types = clear(document.getElementById("pkg-types"));
      var resources = d.resources || {};
      Object.keys(resources).forEach(function (type) {
        var rows = resources[type];
        var details = h("details", { class: "pkg-type" }, [
          h("summary", null, [
            h("strong", { text: type }),
            " ",
            h("span", { class: "muted", text: num(rows.length) }),
          ]),
        ]);
        var filled = false;
        details.addEventListener("toggle", function () {
          if (!details.open || filled) return;
          filled = true;
          if (rows.some(function (r) { return r.target === "hfs"; })) {
            details.appendChild(h("p", null, [h("a", { href: "/ui/resources?type=" + encodeURIComponent(type), text: M("res-browse-type", { type: type }) })]));
          }
          details.appendChild(resourcesTable(type, rows));
        });
        types.appendChild(details);
      });
      show(body, true);
    }

    function load() {
      return api("/" + encodeURIComponent(name) + "/" + encodeURIComponent(version)).then(function (res) {
        show(busy, false);
        if (res.status === 404) {
          error.textContent = M("detail-not-installed", { package: name + "#" + version });
          show(error, true);
          return;
        }
        if (!res.ok || !res.body) {
          error.textContent = M("request-failed", { detail: failure(res) });
          show(error, true);
          return;
        }
        render(res.body);
      });
    }

    // ---- upgrade
    var upgrade = document.getElementById("pkg-upgrade");
    var upgradeNote = document.getElementById("pkg-upgrade-note");
    upgrade.addEventListener("click", function () {
      upgrade.disabled = true;
      api("/registry/versions/" + encodeURIComponent(name)).then(function (res) {
        upgrade.disabled = false;
        var newest = res.body && res.body.versions && res.body.versions[0] && res.body.versions[0].version;
        var latest = (res.body && res.body.latest) || newest;
        if (!res.ok || !latest) {
          upgradeNote.textContent = M("request-failed", { detail: failure(res) });
          return;
        }
        if (latest === version) {
          upgradeNote.textContent = M("upgrade-none");
          return;
        }
        location.href = "/ui/packages?install=" + encodeURIComponent(name) + "&version=" + encodeURIComponent(latest);
      });
    });

    // ---- remove (typed-name confirmation)
    var dialog = document.getElementById("pkg-remove-dialog");
    var input = document.getElementById("pkg-remove-input");
    var submit = document.getElementById("pkg-remove-submit");
    var blocked = document.getElementById("pkg-remove-blocked");
    var removeStatus = document.getElementById("pkg-remove-status");
    var removeError = document.getElementById("pkg-remove-error");
    document.getElementById("pkg-remove-title").textContent = M("remove-title", { package: name + "#" + version });
    document.getElementById("pkg-remove-label").textContent = M("remove-confirm", { name: name });
    input.setAttribute("placeholder", name);
    document.getElementById("pkg-remove").addEventListener("click", function () {
      input.value = "";
      submit.disabled = true;
      show(removeError, false);
      show(removeStatus, false);
      var deps = (detail && detail.dependents) || [];
      blocked.textContent = deps.length ? M("remove-blocked", { packages: deps.join(", ") }) : "";
      show(blocked, deps.length > 0);
      input.disabled = deps.length > 0;
      if (dialog.showModal) dialog.showModal();
      else dialog.setAttribute("open", "");
      if (!deps.length) input.focus();
    });
    document.getElementById("pkg-remove-cancel").addEventListener("click", function () {
      dialog.close ? dialog.close() : dialog.removeAttribute("open");
    });
    input.addEventListener("input", function () {
      submit.disabled = input.value.trim() !== name;
    });
    document.getElementById("pkg-remove-form").addEventListener("submit", function (e) {
      e.preventDefault();
      if (input.value.trim() !== name) return;
      submit.disabled = true;
      input.disabled = true;
      removeStatus.querySelector("[data-busy-label]").textContent = M("removing");
      show(removeStatus, true);
      api("/" + encodeURIComponent(name) + "/" + encodeURIComponent(version) + "?wait=20", { method: "DELETE" }).then(function (res) {
        if (!res.body || !res.body.id) {
          show(removeStatus, false);
          removeError.textContent = M("request-failed", { detail: failure(res) });
          show(removeError, true);
          input.disabled = false;
          return;
        }
        pollJob(res.body.id, function (job, err) {
          if (err) {
            show(removeStatus, false);
            removeError.textContent = M("request-failed", { detail: err });
            show(removeError, true);
            return;
          }
          if (!finished(job)) {
            removeStatus.querySelector("[data-busy-label]").textContent = M("removing") + " " + M("progress-resources", { done: num(job.resourcesDone), total: num(job.resourcesTotal) });
            return;
          }
          var r = job.result || {};
          var deleted = Object.keys(r.deleted || {}).reduce(function (a, k) { return a + r.deleted[k]; }, 0);
          removeStatus.querySelector("[data-busy-label]").textContent = M("removed", { deleted: num(deleted), kept: num((r.keptShared || 0) + (r.keptPreexisting || 0)) });
          removeStatus.querySelector(".spinner").hidden = true;
          if (job.status === "failed") {
            removeError.textContent = M("result-failed", { detail: r.error || "" });
            show(removeError, true);
            return;
          }
          setTimeout(function () {
            location.href = "/ui/packages";
          }, 1500);
        });
      });
    });

    load();
  }

  if (document.getElementById("pkg-app")) initApp();
  if (document.getElementById("pkg-detail")) initDetail();
})();
