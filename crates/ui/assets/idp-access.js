/*
 * Resource access of an authorization-server user (/ui/idp/{realm}/users/{id},
 * crate::idp::access): the table swaps in place (filters, sort, pages, page
 * size), selection across pages and "all matching", bulk remove, row menu
 * (copy, make identity, remove), Copy all in three formats, the add picker
 * (search the endpoint, keyboard, many at once) and Paste / import with a
 * preview. Every change shows a toast with Undo.
 *
 * Messages come from data-msg-* on the card (`%name%` placeholders).
 * Idempotent: safe to load twice and after in-place navigation.
 */
(function () {
  "use strict";
  if (window.HfsAccess) { window.HfsAccess.init(document); return; }

  function $(sel, root) { return (root || document).querySelector(sel); }
  function $$(sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); }
  function fmt(s, args) {
    return String(s || "").replace(/%(\w+)%/g, function (_, k) { return args && args[k] != null ? args[k] : ""; });
  }
  function motion() { return (window.HM && window.HM.motion) || null; }

  /* ---- toast with an action ------------------------------------------- */
  function toast(message, action) {
    var old = document.querySelector(".toast.access-toast");
    if (old) old.remove();
    var t = document.createElement("div");
    t.className = "toast access-toast";
    t.setAttribute("role", "status");
    var span = document.createElement("span");
    span.textContent = message;
    t.appendChild(span);
    var timer;
    function leave() {
      clearTimeout(timer);
      t.classList.add("is-leaving");
      setTimeout(function () { t.remove(); }, 200);
    }
    if (action) {
      var b = document.createElement("button");
      b.type = "button";
      b.className = "btn btn--ghost btn--small access-toast__btn";
      b.textContent = action.label;
      b.addEventListener("click", function () { leave(); action.run(); });
      t.appendChild(b);
    }
    document.body.appendChild(t);
    timer = setTimeout(leave, action ? 9000 : 2600);
    t.addEventListener("mouseenter", function () { clearTimeout(timer); });
    t.addEventListener("mouseleave", function () { timer = setTimeout(leave, 3000); });
  }

  /* ---- clipboard ------------------------------------------------------ */
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text).catch(function () { return legacyCopy(text); });
    }
    return legacyCopy(text);
  }
  function legacyCopy(text) {
    return new Promise(function (resolve, reject) {
      var ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.cssText = "position:fixed;top:0;left:0;opacity:0;pointer-events:none;";
      document.body.appendChild(ta);
      ta.select();
      var ok = false;
      try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
      ta.remove();
      if (ok) resolve(); else reject(new Error("copy refused"));
    });
  }

  function confirmAsk(message) {
    if (window.HfsConfirm) return window.HfsConfirm.ask(message, { danger: true });
    return Promise.resolve(window.confirm(message));
  }

  function postJson(url, body) {
    return fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify(body),
    }).then(readJson);
  }
  function postForm(url, data, accept) {
    var body = new URLSearchParams();
    Object.keys(data).forEach(function (k) { if (data[k] != null) body.append(k, data[k]); });
    return fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: accept || "application/json", "HX-Request": "true" },
      body: body.toString(),
    });
  }
  function readJson(r) {
    return r.text().then(function (t) {
      var v = {};
      try { v = t ? JSON.parse(t) : {}; } catch (e) { v = { error: t }; }
      if (!r.ok) { var err = new Error(v.error || ("HTTP " + r.status)); err.status = r.status; throw err; }
      return v;
    });
  }

  /* ---- one card ------------------------------------------------------- */
  function Card(el) {
    this.el = el;
    this.base = el.getAttribute("data-base");
    this.pageHref = el.getAttribute("data-page-href");
    this.canEdit = el.getAttribute("data-can-edit") === "1";
    this.tableBox = $("[data-access-table]", el);
    this.form = $("[data-access-filters]", el);
    this.selected = new Set();
    this.allMatching = false;
    this.seq = 0;
    this.state = this.readState();
    this.bind();
    this.afterTable(false);
  }

  Card.prototype.msg = function (k, args) {
    return fmt(this.el.getAttribute("data-msg-" + k), args);
  };

  Card.prototype.grid = function () { return $("[data-access-grid]", this.tableBox); };

  Card.prototype.readState = function () {
    var p = new URLSearchParams(location.search);
    var s = {};
    ["q", "type", "kind", "ep", "sort", "dir", "page", "size"].forEach(function (k) { s[k] = p.get(k) || ""; });
    return s;
  };

  Card.prototype.query = function (over) {
    var s = Object.assign({}, this.state, over || {});
    var p = new URLSearchParams();
    Object.keys(s).forEach(function (k) {
      if (s[k] && !(k === "page" && s[k] === "1") && !(k === "size" && s[k] === "25")) p.set(k, s[k]);
    });
    return p.toString();
  };

  Card.prototype.filterQuery = function () {
    return { q: this.state.q, type: this.state.type, kind: this.state.kind, ep: this.state.ep };
  };

  Card.prototype.load = function (over, opts) {
    var self = this;
    opts = opts || {};
    Object.assign(this.state, over || {});
    var qs = this.query();
    var seq = ++this.seq;
    this.tableBox.setAttribute("aria-busy", "true");
    this.tableBox.classList.add("is-loading");
    return fetch(this.base + (qs ? "?" + qs : ""), { credentials: "same-origin", headers: { "HX-Request": "true" } })
      .then(function (r) { if (!r.ok) throw new Error("HTTP " + r.status); return r.text(); })
      .then(function (html) {
        if (seq !== self.seq) return;
        self.tableBox.innerHTML = html;
        try {
          history.replaceState(history.state, "", self.pageHref + (qs ? "?" + qs : "") + "#access");
        } catch (e) {}
        self.afterTable(opts.animate !== false);
        if (opts.focusTable) {
          var first = $("tbody tr", self.tableBox);
          if (first) { var c = first.querySelector("input, a, summary"); if (c) c.focus({ preventScroll: false }); }
        }
      })
      .catch(function () { if (seq === self.seq) toast(self.msg("error")); })
      .then(function () {
        if (seq !== self.seq) return;
        self.tableBox.removeAttribute("aria-busy");
        self.tableBox.classList.remove("is-loading");
      });
  };

  Card.prototype.afterTable = function (animate) {
    var g = this.grid();
    if (!g) return;
    this.fingerprint = g.getAttribute("data-fingerprint");
    this.total = +g.getAttribute("data-total");
    this.matching = +g.getAttribute("data-matching");
    var t = $("[data-access-total]", this.el);
    if (t) t.textContent = this.total;
    // The filter options count the whole list (it may have just changed).
    var counts = {};
    try { counts = JSON.parse(g.getAttribute("data-counts") || "{}"); } catch (e) {}
    $$("option[data-label]", this.form).forEach(function (o) {
      o.textContent = o.getAttribute("data-label") + " (" + (counts[o.value] || 0).toLocaleString() + ")";
    });
    // Nothing to filter in an empty list.
    if (this.form) this.form.hidden = this.total === 0 && !this.query();
    var self = this;
    $$("[data-access-row]", this.tableBox).forEach(function (cb) {
      cb.checked = self.allMatching || self.selected.has(cb.value);
      cb.closest("tr").classList.toggle("is-selected", cb.checked);
    });
    this.syncBulk();
    if (animate && motion()) motion().stagger($$("tbody tr", this.tableBox), { max: 20, total: 240 });
  };

  Card.prototype.rowsOnPage = function () { return $$("[data-access-row]", this.tableBox); };

  Card.prototype.syncBulk = function () {
    var bar = $("[data-access-bulk]", this.el);
    if (!bar) return;
    var n = this.allMatching ? this.matching : this.selected.size;
    var show = n > 0;
    if (show && bar.hidden) {
      bar.hidden = false;
      if (motion()) motion().enter(bar, { dy: -4 });
    } else if (!show) bar.hidden = true;
    $("[data-access-selected-text]", bar).textContent = this.msg("selected", { count: n.toLocaleString() });
    var rows = this.rowsOnPage();
    var all = $("[data-access-all]", this.tableBox);
    var onPage = rows.filter(function (c) { return c.checked; }).length;
    if (all) {
      all.checked = rows.length > 0 && onPage === rows.length;
      all.indeterminate = onPage > 0 && onPage < rows.length;
    }
    var more = $("[data-access-select-matching]", bar);
    if (more) {
      var offer = !this.allMatching && rows.length > 0 && onPage === rows.length && this.matching > rows.length;
      more.hidden = !offer;
      more.textContent = this.msg("select-all", { count: this.matching.toLocaleString() });
    }
  };

  Card.prototype.clearSelection = function () {
    this.selected.clear();
    this.allMatching = false;
    this.afterTable(false);
  };

  /* ---- changes ------------------------------------------------------ */
  Card.prototype.changed = function (res, undoable) {
    var self = this;
    var parts = [];
    if (res.added) parts.push(this.msg("added", { count: res.added.toLocaleString() }));
    if (res.removed) parts.push(this.msg("removed", { count: res.removed.toLocaleString() }));
    if (res.changed) parts.push(this.msg("changed", { count: res.changed.toLocaleString() }));
    var text = parts.length ? parts.join(" · ") : this.msg("nothing");
    var undo = res.undo && ((res.undo.add || []).length || (res.undo.remove || []).length) ? res.undo : null;
    toast(text, undoable !== false && undo ? {
      label: this.msg("undo"),
      run: function () {
        postJson(self.base + "/patch", { add: undo.add, remove: undo.remove, action: "undo" })
          .then(function () { toast(self.msg("undone")); self.load({}, { animate: false }); })
          .catch(function (e) { toast(e.message || self.msg("error")); });
      },
    } : null);
    this.clearSelection();
    return this.load({}, { animate: true });
  };

  Card.prototype.patch = function (body) {
    var self = this;
    body.base = this.fingerprint;
    return postJson(this.base + "/patch", body)
      .then(function (res) { return self.changed(res); })
      .catch(function (e) {
        toast(e.message || self.msg("error"));
        if (e.status === 409) self.load({}, { animate: false });
      });
  };

  Card.prototype.removeSelected = function () {
    var self = this;
    var n = this.allMatching ? this.matching : this.selected.size;
    if (!n) return;
    var q = this.allMatching ? this.msg("remove-all-confirm", { count: n.toLocaleString() }) : this.msg("remove-confirm", { count: n.toLocaleString() });
    confirmAsk(q).then(function (ok) {
      if (!ok) return;
      if (self.allMatching) {
        self.patch({ removeMatching: self.filterQuery(), action: "remove" });
      } else {
        var remove = [];
        self.selected.forEach(function (k) {
          var i = k.indexOf("|");
          remove.push({ endpoint: k.slice(0, i), reference: k.slice(i + 1) });
        });
        self.patch({ remove: remove, action: "remove" });
      }
    });
  };

  Card.prototype.copyAll = function (format) {
    var self = this;
    var labels = { lines: "lines", csv: "CSV", json: "JSON" };
    fetch(this.base + "/export?format=" + encodeURIComponent(format), { credentials: "same-origin" })
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        var n = r.headers.get("X-Access-Count") || "";
        return r.text().then(function (t) { return { text: t, count: n }; });
      })
      .then(function (x) {
        return copyText(x.text).then(function () {
          toast(self.msg("copied", { count: (+x.count).toLocaleString(), format: labels[format] || format }));
        });
      })
      .catch(function () { toast(self.msg("copy-failed")); });
  };

  /* ---- events ------------------------------------------------------- */
  Card.prototype.bind = function () {
    var self = this;
    var el = this.el;
    var timer;
    if (this.form) {
      this.form.addEventListener("submit", function (e) { e.preventDefault(); self.readForm(); });
      this.form.addEventListener("input", function (e) {
        if (e.target.name !== "q") return;
        clearTimeout(timer);
        timer = setTimeout(function () { self.readForm(); }, 280);
      });
      this.form.addEventListener("change", function (e) {
        if (e.target.tagName === "SELECT") self.readForm();
      });
    }
    el.addEventListener("click", function (e) {
      var t = e.target;
      var nav = t.closest("[data-access-nav]");
      if (nav && self.tableBox.contains(nav)) {
        e.preventDefault();
        var u = new URL(nav.href, location.href);
        var s = {};
        ["q", "type", "kind", "ep", "sort", "dir", "page", "size"].forEach(function (k) { s[k] = u.searchParams.get(k) || ""; });
        self.state = s;
        self.load({}, { focusTable: nav.getAttribute("rel") !== null });
        return;
      }
      if (t.closest("[data-access-clear-filters]")) {
        e.preventDefault();
        if (self.form) self.form.reset();
        $$("input[name=q]", self.form).forEach(function (i) { i.value = ""; });
        $$("select", self.form).forEach(function (s) { s.value = ""; });
        self.readForm();
        return;
      }
      var copy = t.closest("[data-access-copy]");
      if (copy) {
        var menu = copy.closest("details");
        if (menu) menu.open = false;
        self.copyAll(copy.getAttribute("data-access-copy"));
        return;
      }
      var one = t.closest("[data-access-copy-one]");
      if (one) {
        one.closest("details").open = false;
        copyText(one.getAttribute("data-access-copy-one")).then(function () {
          toast(self.msg("copied-one", { reference: one.getAttribute("data-access-copy-one") }));
        }, function () { toast(self.msg("copy-failed")); });
        return;
      }
      var row = t.closest("tr[data-key]");
      if (t.closest("[data-access-remove-one]") && row) {
        t.closest("details").open = false;
        confirmAsk(self.msg("remove-confirm", { count: 1 })).then(function (ok) {
          if (ok) self.patch({ remove: [{ endpoint: row.getAttribute("data-endpoint"), reference: row.getAttribute("data-reference") }], action: "remove" });
        });
        return;
      }
      if (t.closest("[data-access-identity]") && row) {
        t.closest("details").open = false;
        confirmAsk(self.msg("identity-confirm", { reference: row.getAttribute("data-reference"), endpoint: row.getAttribute("data-endpoint") })).then(function (ok) {
          if (ok) self.patch({ add: [{ endpoint: row.getAttribute("data-endpoint"), reference: row.getAttribute("data-reference"), kind: "fhirUser" }], action: "identity" });
        });
        return;
      }
      if (t.closest("[data-access-remove-selected]")) { self.removeSelected(); return; }
      if (t.closest("[data-access-clear]")) { self.clearSelection(); return; }
      if (t.closest("[data-access-select-matching]")) {
        self.allMatching = true;
        self.afterTable(false);
        return;
      }
      var open = t.closest("[data-access-open]");
      if (open) {
        var which = open.getAttribute("data-access-open");
        if (which === "add") self.openAdd(); else self.openPaste();
      }
    });
    el.addEventListener("change", function (e) {
      var t = e.target;
      if (t.matches("[data-access-row]")) {
        if (self.allMatching && !t.checked) {
          // Leaving "all matching": keep this page minus the row.
          self.allMatching = false;
          self.selected.clear();
          self.rowsOnPage().forEach(function (c) { if (c.checked) self.selected.add(c.value); });
        }
        if (t.checked) self.selected.add(t.value); else self.selected.delete(t.value);
        t.closest("tr").classList.toggle("is-selected", t.checked);
        self.syncBulk();
      } else if (t.matches("[data-access-all]")) {
        self.allMatching = false;
        self.rowsOnPage().forEach(function (c) {
          c.checked = t.checked;
          c.closest("tr").classList.toggle("is-selected", t.checked);
          if (t.checked) self.selected.add(c.value); else self.selected.delete(c.value);
        });
        self.syncBulk();
      } else if (t.matches("[data-access-size]")) {
        self.load({ size: t.value, page: "" });
      }
    });
    if (this.canEdit) { this.bindAdd(); this.bindPaste(); }
  };

  Card.prototype.readForm = function () {
    var f = new FormData(this.form);
    var next = { q: (f.get("q") || "").toString().trim(), type: f.get("type") || "", kind: f.get("kind") || "", ep: f.get("ep") || "", page: "" };
    var same = ["q", "type", "kind", "ep"].every(function (k) { return (this.state[k] || "") === next[k]; }, this);
    if (same) return;
    this.allMatching = false;
    this.load(next);
  };

  /* ---- dialogs ------------------------------------------------------ */
  function openDialog(d) {
    if (!d) return;
    if (typeof d.showModal === "function") { if (!d.open) d.showModal(); } else d.setAttribute("open", "");
    var m = motion();
    if (m) m.enter(d.querySelector(".access-dialog__form") || d, { dy: 8 });
  }
  function closeDialog(d) {
    if (!d) return;
    if (typeof d.close === "function" && d.open) d.close(); else d.removeAttribute("open");
  }

  /* Add picker */
  Card.prototype.bindAdd = function () {
    var self = this;
    var d = $('[data-access-dialog="add"]', this.el);
    if (!d) return;
    this.addDialog = d;
    this.picked = new Map();
    var input = $("[data-access-pick-q]", d);
    var results = $("[data-access-results]", d);
    var timer;
    d.addEventListener("click", function (e) {
      if (e.target === d) { closeDialog(d); return; }
      if (e.target.closest("[data-access-close]")) { closeDialog(d); return; }
      var opt = e.target.closest('[role="option"]');
      if (opt) { self.toggleOption(opt); self.activate(opt); input.focus(); }
    });
    input.addEventListener("input", function () {
      clearTimeout(timer);
      timer = setTimeout(function () { self.search(); }, 250);
    });
    d.addEventListener("change", function (e) {
      if (e.target.matches("[data-access-pick-type], [data-access-pick-ep]")) self.search();
      if (e.target.matches("[data-access-pick-kind]")) self.syncPicked();
    });
    input.addEventListener("keydown", function (e) {
      var opts = $$('[role="option"]', results);
      var cur = results.querySelector(".is-active");
      var i = opts.indexOf(cur);
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        if (!opts.length) return;
        var n = e.key === "ArrowDown" ? Math.min(opts.length - 1, i + 1) : Math.max(0, i - 1);
        if (i < 0) n = e.key === "ArrowDown" ? 0 : opts.length - 1;
        self.activate(opts[n]);
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (e.ctrlKey || e.metaKey) { self.submitAdd(); return; }
        if (cur) self.toggleOption(cur);
        else if (opts.length === 1) self.toggleOption(opts[0]);
      } else if (e.key === "Escape") {
        // A search field eats the first Escape (it clears itself): close
        // the dialog at once instead.
        e.preventDefault();
        closeDialog(d);
      }
    });
    $("[data-access-add-form]", d).addEventListener("submit", function (e) { e.preventDefault(); self.submitAdd(); });
  };

  Card.prototype.activate = function (opt) {
    var d = this.addDialog;
    $$(".is-active", d).forEach(function (o) { o.classList.remove("is-active"); });
    opt.classList.add("is-active");
    $("[data-access-pick-q]", d).setAttribute("aria-activedescendant", opt.id);
    opt.scrollIntoView({ block: "nearest" });
  };

  Card.prototype.toggleOption = function (opt) {
    if (opt.getAttribute("aria-disabled") === "true") return;
    var key = opt.getAttribute("data-key");
    if (this.picked.has(key)) this.picked.delete(key); else this.picked.set(key, true);
    opt.setAttribute("aria-selected", this.picked.has(key) ? "true" : "false");
    this.syncPicked();
  };

  Card.prototype.syncPicked = function () {
    var d = this.addDialog;
    var n = this.picked.size;
    var btn = $("[data-access-add-submit]", d);
    btn.disabled = n === 0;
    btn.textContent = n ? this.msg("add-n", { count: n }) : btn.getAttribute("data-label") || btn.textContent;
    if (!btn.getAttribute("data-label")) btn.setAttribute("data-label", btn.textContent);
    $("[data-access-picked]", d).textContent = n ? this.msg("picked", { count: n }) : "";
  };

  Card.prototype.search = function () {
    var self = this;
    var d = this.addDialog;
    var results = $("[data-access-results]", d);
    var type = ($("[data-access-pick-type]:checked", d) || {}).value || "Patient";
    var ep = ($("[data-access-pick-ep]", d) || {}).value || "";
    var q = $("[data-access-pick-q]", d).value.trim();
    var seq = (this.searchSeq = (this.searchSeq || 0) + 1);
    results.setAttribute("aria-busy", "true");
    results.classList.add("is-loading");
    if (!results.firstChild) results.innerHTML = '<p class="access-pick__msg">' + this.msg("searching") + "</p>";
    fetch(this.base + "/search?" + new URLSearchParams({ ep: ep, type: type, q: q }).toString(), { credentials: "same-origin", headers: { "HX-Request": "true" } })
      .then(function (r) { return r.text(); })
      .then(function (html) {
        if (seq !== self.searchSeq) return;
        results.innerHTML = html;
        $$('[role="option"]', results).forEach(function (o) {
          if (self.picked.has(o.getAttribute("data-key"))) o.setAttribute("aria-selected", "true");
        });
        var first = results.querySelector('[role="option"]:not([aria-disabled="true"])');
        if (first) self.activate(first); else $("[data-access-pick-q]", d).removeAttribute("aria-activedescendant");
        if (motion()) motion().stagger($$('[role="option"]', results), { max: 12, total: 200 });
      })
      .catch(function () { if (seq === self.searchSeq) results.innerHTML = '<p class="access-pick__msg">' + self.msg("error") + "</p>"; })
      .then(function () { results.removeAttribute("aria-busy"); results.classList.remove("is-loading"); });
  };

  Card.prototype.openAdd = function () {
    var d = this.addDialog;
    if (!d) return;
    this.picked.clear();
    this.syncPicked();
    $("[data-access-pick-q]", d).value = "";
    $("[data-access-results]", d).innerHTML = "";
    openDialog(d);
    $("[data-access-pick-q]", d).focus();
    this.search();
  };

  Card.prototype.submitAdd = function () {
    var self = this;
    var d = this.addDialog;
    if (!this.picked.size) return;
    var kind = $("[data-access-pick-kind]", d).value;
    var add = [];
    this.picked.forEach(function (_, key) {
      var i = key.indexOf("|");
      var ref = key.slice(i + 1);
      var k = kind === "auto" ? (ref.indexOf("Patient/") === 0 ? "patient" : "link") : kind;
      if (k === "patient" && ref.indexOf("Patient/") !== 0) k = "link";
      add.push({ endpoint: key.slice(0, i), reference: ref, kind: k });
    });
    if (kind === "fhirUser" && add.length > 1) {
      $("[data-access-picked]", d).textContent = this.msg("one-identity");
      return;
    }
    var btn = $("[data-access-add-submit]", d);
    btn.disabled = true;
    btn.classList.add("is-busy");
    postJson(this.base + "/patch", { add: add, action: "add", base: this.fingerprint })
      .then(function (res) { closeDialog(d); return self.changed(res); })
      .catch(function (e) { $("[data-access-picked]", d).textContent = e.message || self.msg("error"); })
      .then(function () { btn.disabled = self.picked.size === 0; btn.classList.remove("is-busy"); });
  };

  /* Paste / import */
  Card.prototype.bindPaste = function () {
    var self = this;
    var d = $('[data-access-dialog="paste"]', this.el);
    if (!d) return;
    this.pasteDialog = d;
    var text = $("[data-access-paste-text]", d);
    var drop = $("[data-access-drop]", d);
    var file = $("[data-access-file]", d);
    var form = $("[data-access-paste-form]", d);
    var maxBytes = +this.el.getAttribute("data-max-bytes") || 4194304;
    function syncInput() {
      var v = text.value;
      var lines = v ? v.split(/\r?\n/).filter(function (l) { return l.trim() && l.trim()[0] !== "#"; }).length : 0;
      $("[data-access-line-count]", d).textContent = lines ? self.msg("lines", { count: lines.toLocaleString() }) : "";
      $("[data-access-preview-btn]", d).disabled = !v.trim();
    }
    text.addEventListener("input", syncInput);
    function readFile(f) {
      if (!f) return;
      if (f.size > maxBytes) { $("[data-access-paste-status]", d).textContent = self.msg("file-too-big"); return; }
      var r = new FileReader();
      r.onload = function () {
        text.value = String(r.result || "");
        $("[data-access-file-name]", d).textContent = f.name;
        $("[data-access-paste-status]", d).textContent = "";
        syncInput();
      };
      r.readAsText(f);
    }
    file.addEventListener("change", function () { readFile(file.files && file.files[0]); file.value = ""; });
    ["dragenter", "dragover"].forEach(function (ev) {
      drop.addEventListener(ev, function (e) {
        if (!e.dataTransfer || Array.prototype.indexOf.call(e.dataTransfer.types || [], "Files") < 0) return;
        e.preventDefault();
        drop.classList.add("is-over");
      });
    });
    ["dragleave", "drop"].forEach(function (ev) {
      drop.addEventListener(ev, function (e) {
        if (ev === "dragleave" && drop.contains(e.relatedTarget)) return;
        drop.classList.remove("is-over");
      });
    });
    drop.addEventListener("drop", function (e) {
      if (!e.dataTransfer || !e.dataTransfer.files || !e.dataTransfer.files.length) return;
      e.preventDefault();
      readFile(e.dataTransfer.files[0]);
    });
    d.addEventListener("click", function (e) {
      if (e.target === d || e.target.closest("[data-access-close]")) { closeDialog(d); return; }
      if (e.target.closest("[data-access-back]")) { self.pasteStep("input"); text.focus(); return; }
      if (e.target.closest("[data-access-apply]")) { self.applyPaste(); return; }
      var chip = e.target.closest("[data-access-pv-show]");
      if (chip) {
        var code = chip.getAttribute("data-access-pv-show");
        $$("[data-access-pv-show]", d).forEach(function (c) { c.setAttribute("aria-pressed", c === chip ? "true" : "false"); });
        $$("[data-access-preview] tbody tr", d).forEach(function (tr) { tr.hidden = !!code && tr.getAttribute("data-status") !== code; });
      }
    });
    d.addEventListener("change", function (e) {
      if (e.target.matches("[data-access-mode]")) {
        var hint = $("[data-access-mode-hint]", d);
        hint.textContent = hint.getAttribute("data-" + e.target.value) || "";
      }
      if (e.target.matches("[data-access-include-missing]")) self.syncApply();
    });
    form.addEventListener("submit", function (e) { e.preventDefault(); self.previewPaste(); });
  };

  Card.prototype.pasteStep = function (step) {
    var d = this.pasteDialog;
    $$("[data-access-step]", d).forEach(function (s) { s.hidden = s.getAttribute("data-access-step") !== step; });
    $$("[data-access-in]", d).forEach(function (b) { b.hidden = b.getAttribute("data-access-in") !== step; });
    var shown = $('[data-access-step="' + step + '"]', d);
    if (motion() && shown) motion().enter(shown, { dx: step === "preview" ? 12 : -12 });
  };

  Card.prototype.openPaste = function () {
    var d = this.pasteDialog;
    if (!d) return;
    this.pasteStep("input");
    $("[data-access-paste-status]", d).textContent = "";
    openDialog(d);
    $("[data-access-paste-text]", d).focus();
  };

  Card.prototype.pasteData = function () {
    var d = this.pasteDialog;
    var f = new FormData($("[data-access-paste-form]", d));
    return { text: (f.get("text") || "").toString(), mode: f.get("mode") || "add", ep: f.get("ep") || "" };
  };

  Card.prototype.previewPaste = function () {
    var self = this;
    var d = this.pasteDialog;
    var data = this.pasteData();
    if (!data.text.trim()) return;
    var btn = $("[data-access-preview-btn]", d);
    btn.disabled = true;
    btn.classList.add("is-busy");
    $("[data-access-paste-status]", d).textContent = this.msg("loading");
    postForm(this.base + "/preview", data, "text/html")
      .then(function (r) { return r.text(); })
      .then(function (html) {
        $("[data-access-preview]", d).innerHTML = html;
        $("[data-access-paste-status]", d).textContent = "";
        self.pasteStep("preview");
        self.syncApply();
        var a = $("[data-access-apply]", d);
        if (!a.disabled) a.focus(); else $("[data-access-back]", d).focus();
      })
      .catch(function () { $("[data-access-paste-status]", d).textContent = self.msg("error"); })
      .then(function () { btn.disabled = false; btn.classList.remove("is-busy"); });
  };

  Card.prototype.syncApply = function () {
    var d = this.pasteDialog;
    var pv = $("[data-access-pv]", d);
    var a = $("[data-access-apply]", d);
    if (!pv) { a.disabled = true; a.textContent = a.getAttribute("data-label") || a.textContent; return; }
    if (!a.getAttribute("data-label")) a.setAttribute("data-label", a.textContent);
    var inc = $("[data-access-include-missing]", d);
    var missing = ($("[data-access-skip]", d).value || "").split("\n").filter(Boolean).length;
    var added = +pv.getAttribute("data-added") + (inc && inc.checked ? missing : 0);
    var work = added + +pv.getAttribute("data-removed") + +pv.getAttribute("data-changed");
    a.disabled = work === 0;
    a.textContent = pv.getAttribute("data-label").replace(/\d[\d,]*/, function (m) {
      return pv.getAttribute("data-mode") === "add" ? added.toLocaleString() : m;
    });
  };

  Card.prototype.applyPaste = function () {
    var self = this;
    var d = this.pasteDialog;
    var pv = $("[data-access-pv]", d);
    if (!pv) return;
    var data = this.pasteData();
    var removed = +pv.getAttribute("data-removed");
    var go = data.mode === "replace" && removed > 0 ? confirmAsk(this.msg("replace-confirm", { count: removed.toLocaleString() })) : Promise.resolve(true);
    go.then(function (ok) {
      if (!ok) return;
      var a = $("[data-access-apply]", d);
      a.disabled = true;
      a.classList.add("is-busy");
      var inc = $("[data-access-include-missing]", d);
      data.skip = $("[data-access-skip]", d).value;
      data.include_missing = inc && inc.checked ? "1" : "0";
      data.base = pv.getAttribute("data-base");
      postForm(self.base + "/apply", data)
        .then(readJson)
        .then(function (res) {
          closeDialog(d);
          $("[data-access-paste-text]", d).value = "";
          $("[data-access-file-name]", d).textContent = "";
          return self.changed(res);
        })
        .catch(function (e) { $("[data-access-paste-status]", d).textContent = e.message || self.msg("error"); })
        .then(function () { a.classList.remove("is-busy"); self.syncApply(); });
    });
  };

  /* ---- boot --------------------------------------------------------- */
  function init(root) {
    $$("[data-access]", root || document).forEach(function (el) {
      if (el.__access) return;
      el.__access = new Card(el);
    });
  }
  window.HfsAccess = { init: init };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", function () { init(document); });
  else init(document);
  document.addEventListener("htmx:afterSwap", function (e) { init(e.target); });
  document.addEventListener("hm:navigated", function () { init(document); });
})();
