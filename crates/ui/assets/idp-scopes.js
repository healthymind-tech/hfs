/*
 * SMART scope builder of an app (crate::idp::scopes,
 * partials/idp/scope-builder.html): presets, sign-in / launch toggles,
 * patient/ user/ system/ contexts (several at once), a searchable grouped
 * list of every resource type the realm's endpoints serve with
 * Create/Read/Update/Delete/Search per context, the resulting scopes as
 * chips, live server analysis (warnings, scopes another one covers), Copy
 * all / Paste, and the raw text box (`scopes_text`, one per line) kept in
 * sync both ways. The text box is the only form field: without JavaScript
 * it is the editor.
 *
 * The scope list is the source of truth; a type row edits only the
 * unfiltered scopes of its context and type and leaves every other scope
 * (granular `?category=` scopes, v1 spellings, custom scopes) exactly as
 * written. Messages come from data-msg-* (`%name%` placeholders).
 * Idempotent: safe to load twice and after in-place navigation.
 */
(function () {
  "use strict";
  if (window.HfsScopes) { window.HfsScopes.init(document); return; }

  var LETTERS = ["c", "r", "u", "d", "s"];
  var BIT = { c: 1, r: 2, u: 4, d: 8, s: 16 };
  var ALL = 31, RS = BIT.r | BIT.s;
  var RES = /^(patient|user|system)\/(\*|[A-Z][A-Za-z0-9]*)\.([a-z*]+)(\?.*)?$/;

  function $(sel, root) { return (root || document).querySelector(sel); }
  function $$(sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); }
  function fmt(s, args) {
    return String(s || "").replace(/%(\w+)%/g, function (_, k) { return args && args[k] != null ? args[k] : ""; });
  }
  function motion() { return (window.HM && window.HM.motion) || null; }
  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  /* ---- scope grammar (mirrors helios_idp::scope::parse_scope) -------- */
  function parseList(text) {
    var seen = Object.create(null), out = [];
    String(text || "").split(/[\s,]+/).forEach(function (t) {
      if (t && !seen[t]) { seen[t] = true; out.push(t); }
    });
    return out;
  }
  function permBits(p) {
    if (p === "read") return { bits: RS, v1: true };
    if (p === "write") return { bits: BIT.c | BIT.u | BIT.d, v1: true };
    if (p === "*") return { bits: ALL, v1: true };
    var bits = 0, idx = 0;
    for (var i = 0; i < p.length; i++) {
      var pos = LETTERS.indexOf(p[i], idx);
      if (pos < 0) return null;
      bits |= BIT[p[i]];
      idx = pos + 1;
    }
    return bits ? { bits: bits, v1: false } : null;
  }
  function parseRes(s) {
    var m = RES.exec(s);
    if (!m) return null;
    var p = permBits(m[3]);
    if (!p) return null;
    if (m[4] !== undefined && (p.v1 || m[4].length < 2)) return null;
    return { ctx: m[1], type: m[2], bits: p.bits, v1: p.v1, query: m[4] ? m[4].slice(1) : null };
  }
  function letters(bits) { return LETTERS.filter(function (l) { return bits & BIT[l]; }).join(""); }
  function v1Of(ctx, type, bits) {
    var p = bits === RS ? "read" : bits === (BIT.c | BIT.u | BIT.d) ? "write" : bits === ALL ? "*" : null;
    return p ? ctx + "/" + type + "." + p : null;
  }

  /* ---- toast with Undo (same look as the resource-access editor) ------ */
  function toast(message, action) {
    var old = document.querySelector(".toast.access-toast");
    if (old) old.remove();
    var t = el("div", "toast access-toast");
    t.setAttribute("role", "status");
    t.appendChild(el("span", null, message));
    var timer;
    function leave() {
      clearTimeout(timer);
      t.classList.add("is-leaving");
      setTimeout(function () { t.remove(); }, 200);
    }
    if (action) {
      var b = el("button", "btn btn--ghost btn--small access-toast__btn", action.label);
      b.type = "button";
      b.addEventListener("click", function () { leave(); action.run(); });
      t.appendChild(b);
    }
    document.body.appendChild(t);
    timer = setTimeout(leave, action ? 8000 : 2600);
  }
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

  /* ---- one builder ---------------------------------------------------- */
  function Builder(root) {
    this.root = root;
    this.form = root.closest("form");
    this.text = $("[data-scopeb-text]", root);
    this.table = $("[data-scopeb-table]", root);
    this.v1 = $("[data-scopeb-v1]", root);
    try { this.cat = JSON.parse($("[data-scopeb-catalog]", root).textContent || "{}"); } catch (e) { this.cat = {}; }
    this.cat.groups = this.cat.groups || [];
    this.cat.contexts = this.cat.contexts || [];
    this.cat.perms = this.cat.perms || [];
    this.list = parseList(this.text.value);
    this.q = "";
    this.selectedOnly = false;
    this.redundant = Object.create(null);
    this.invalid = Object.create(null);
    this.seq = 0;
    this.ctxs = this.contextsInList();
    if (!this.ctxs.length) this.ctxs = [this.kind() === "backend" ? "system" : "patient"];
    // Groups with a selection start open; the rest start closed.
    this.open = Object.create(null);
    var self = this;
    this.cat.groups.forEach(function (g) {
      if (g.types.some(function (t) { return self.cat.contexts.some(function (c) { return self.explicit(c.id, t); }); })) self.open[g.id] = true;
    });
    if (!this.cat.groups.some(function (g) { return self.open[g.id]; }) && this.cat.groups.length) this.open[this.cat.groups[0].id] = true;

    $("[data-scopeb-ui]", root).hidden = false;
    $("[data-scopeb-actions]", root).hidden = false;
    var raw = $("[data-scopeb-raw]", root);
    if (raw && !root.hasAttribute("data-error")) raw.open = false;
    this.bind();
    this.renderAll();
    this.scheduleCheck(0);
  }

  Builder.prototype.msg = function (k, args) { return fmt(this.root.getAttribute("data-msg-" + k), args); };

  Builder.prototype.kind = function () {
    var k = this.root.getAttribute("data-kind");
    if (k) return k;
    var r = this.form && this.form.querySelector('input[name="client_kind"]:checked');
    return r ? r.value : "public";
  };
  Builder.prototype.launch = function () {
    if (!this.form) return "";
    return $$('input[name="launch"]:checked', this.form).filter(function (i) { return !i.disabled; }).map(function (i) { return i.value; }).join(" ");
  };
  Builder.prototype.acceptV1 = function () { return !!(this.v1 && this.v1.checked); };

  Builder.prototype.contextsInList = function () {
    var seen = {};
    this.list.forEach(function (s) { var r = parseRes(s); if (r) seen[r.ctx] = true; });
    return ["patient", "user", "system"].filter(function (c) { return seen[c]; });
  };
  /* Bits written for ctx/type without a query. */
  Builder.prototype.explicit = function (ctx, type) {
    var bits = 0;
    this.list.forEach(function (s) {
      var r = parseRes(s);
      if (r && !r.query && r.ctx === ctx && r.type === type) bits |= r.bits;
    });
    return bits;
  };
  Builder.prototype.cover = function (ctx, type) { return type === "*" ? 0 : this.explicit(ctx, "*"); };

  Builder.prototype.setBits = function (ctx, type, bits) {
    var at = -1, out = [];
    this.list.forEach(function (s) {
      var r = parseRes(s);
      if (r && !r.query && r.ctx === ctx && r.type === type) { if (at < 0) at = out.length; return; }
      out.push(s);
    });
    if (bits) {
      var tok = ctx + "/" + type + "." + letters(bits);
      if (at < 0) out.push(tok); else out.splice(at, 0, tok);
    }
    this.list = out;
    this.changed();
  };
  Builder.prototype.toggleToken = function (tok, on) {
    var i = this.list.indexOf(tok);
    if (on && i < 0) this.list.push(tok);
    if (!on && i >= 0) this.list.splice(i, 1);
    this.changed();
  };
  Builder.prototype.replaceAll = function (list, toastKey) {
    var before = this.list.slice(), ctxBefore = this.ctxs.slice(), self = this;
    this.list = list;
    var c = this.contextsInList();
    if (c.length) this.ctxs = c;
    this.changed();
    if (toastKey) {
      toast(this.msg(toastKey, { count: list.length }), {
        label: this.msg("undo"),
        run: function () { self.list = before; self.ctxs = ctxBefore; self.changed(); toast(self.msg("undone")); },
      });
    }
  };

  /* The list changed: write the text box, re-render, re-check. */
  Builder.prototype.changed = function (fromText) {
    if (!fromText) {
      this.text.value = this.list.join("\n");
      // The unsaved-changes guard listens for input on the form.
      this.text.dispatchEvent(new Event("input", { bubbles: true }));
    }
    this.renderAll();
    this.scheduleCheck(250);
  };

  /* ---- events --------------------------------------------------------- */
  Builder.prototype.bind = function () {
    var self = this, root = this.root;
    var textTimer;
    this.text.addEventListener("input", function (e) {
      if (!e.isTrusted) return;
      clearTimeout(textTimer);
      textTimer = setTimeout(function () {
        self.list = parseList(self.text.value);
        var c = self.contextsInList();
        c.forEach(function (x) { if (self.ctxs.indexOf(x) < 0) self.ctxs.push(x); });
        self.changed(true);
      }, 200);
    });
    root.addEventListener("click", function (e) {
      var t = e.target;
      var preset = t.closest("[data-scopeb-preset]");
      if (preset) { self.applyPreset(preset); return; }
      var ctx = t.closest("[data-scopeb-ctx]");
      if (ctx) { self.toggleCtx(ctx.getAttribute("data-scopeb-ctx")); return; }
      var show = t.closest("[data-scopeb-show]");
      if (show) {
        self.selectedOnly = show.getAttribute("data-scopeb-show") === "selected";
        $$("[data-scopeb-show]", root).forEach(function (b) { b.setAttribute("aria-pressed", b === show ? "true" : "false"); });
        self.renderTable();
        return;
      }
      var grp = t.closest("[data-scopeb-group]");
      if (grp) {
        var id = grp.getAttribute("data-scopeb-group");
        self.open[id] = !self.open[id];
        self.renderTable(id);
        return;
      }
      var quick = t.closest("[data-scopeb-quick]");
      if (quick) {
        var row = quick.closest("[data-type]");
        var v = quick.getAttribute("data-scopeb-quick");
        self.setBits(quick.getAttribute("data-ctx"), row.getAttribute("data-type"), v === "rs" ? RS : v === "cruds" ? ALL : 0);
        return;
      }
      var rm = t.closest("[data-scopeb-rm]");
      if (rm) { self.toggleToken(rm.getAttribute("data-scopeb-rm"), false); return; }
      var cp = t.closest("[data-scopeb-copy]");
      if (cp) { self.copy(cp.getAttribute("data-scopeb-copy")); var m = cp.closest("details"); if (m) m.open = false; return; }
      if (t.closest("[data-scopeb-paste-open]")) { self.openPaste(); return; }
    });
    root.addEventListener("change", function (e) {
      var t = e.target;
      if (t.matches("[data-scopeb-nr]")) { self.toggleToken(t.getAttribute("data-scopeb-nr"), t.checked); return; }
      if (t.matches("[data-scopeb-perm]")) {
        var row = t.closest("[data-type]");
        var ctx = t.getAttribute("data-ctx"), type = row.getAttribute("data-type");
        var bits = self.explicit(ctx, type);
        var b = BIT[t.getAttribute("data-scopeb-perm")];
        bits = t.checked ? bits | b : bits & ~b;
        self.setBits(ctx, type, bits);
        return;
      }
      if (t === self.v1) { self.renderAll(); self.scheduleCheck(0); }
    });
    var q = $("[data-scopeb-q]", root);
    var qTimer;
    q.addEventListener("input", function () {
      clearTimeout(qTimer);
      qTimer = setTimeout(function () { self.q = q.value.trim().toLowerCase(); self.renderTable(); }, 120);
    });
    q.addEventListener("keydown", function (e) { if (e.key === "Enter") e.preventDefault(); });
    if (this.form) {
      this.form.addEventListener("change", function (e) {
        if (e.target.name === "client_kind" || e.target.name === "launch") self.scheduleCheck(0);
      });
    }
    this.bindPaste();
  };

  Builder.prototype.applyPreset = function (btn) {
    var list = parseList(btn.getAttribute("data-scopeb-preset"));
    if (btn.hasAttribute("data-backend") && this.form) {
      var radio = this.form.querySelector('input[name="client_kind"][value="backend"]');
      if (radio && !radio.checked && !this.root.getAttribute("data-kind")) {
        radio.checked = true;
        radio.dispatchEvent(new Event("change", { bubbles: true }));
        toast(this.msg("backend-kind"));
      }
    }
    this.ctxs = [];
    this.replaceAll(list, "applied");
    var m = motion();
    if (m) m.enter($("[data-scopeb-chips]", this.root), { dy: 4 });
  };

  Builder.prototype.toggleCtx = function (c) {
    var self = this, i = this.ctxs.indexOf(c);
    if (i < 0) {
      this.ctxs.push(c);
      this.ctxs.sort(function (a, b) { return ["patient", "user", "system"].indexOf(a) - ["patient", "user", "system"].indexOf(b); });
      this.renderAll();
      return;
    }
    var mine = this.list.filter(function (s) { var r = parseRes(s); return r && r.ctx === c; });
    function drop() {
      self.ctxs.splice(self.ctxs.indexOf(c), 1);
      if (mine.length) {
        self.list = self.list.filter(function (s) { return mine.indexOf(s) < 0; });
        self.changed();
      } else self.renderAll();
    }
    if (!mine.length) { drop(); return; }
    confirmAsk(this.msg("remove-ctx", { ctx: c + "/", count: mine.length })).then(function (ok) { if (ok) drop(); });
  };

  Builder.prototype.copy = function (how) {
    var self = this;
    if (!this.list.length) { toast(this.msg("nothing")); return; }
    copyText(this.list.join(how === "space" ? " " : "\n")).then(
      function () { toast(self.msg("copied", { count: self.list.length })); },
      function () { toast(self.msg("copy-failed")); }
    );
  };

  /* ---- paste dialog --------------------------------------------------- */
  Builder.prototype.bindPaste = function () {
    var self = this, d = $("[data-scopeb-paste]", this.root);
    if (!d) return;
    this.paste = d;
    // A <dialog> inside the form would submit it on Enter: keep it outside.
    document.body.appendChild(d);
    var ta = $("[data-scopeb-paste-text]", d), apply = $("[data-scopeb-paste-apply]", d), st = $("[data-scopeb-paste-status]", d);
    var mode = "add";
    function sync() {
      var l = parseList(ta.value);
      apply.disabled = !l.length;
      st.textContent = l.length ? self.msg("lines", { count: l.length }) : "";
    }
    ta.addEventListener("input", sync);
    d.addEventListener("click", function (e) {
      if (e.target === d || e.target.closest("[data-scopeb-close]")) { d.close(); return; }
      var m = e.target.closest("[data-scopeb-paste-mode]");
      if (m) {
        mode = m.getAttribute("data-scopeb-paste-mode");
        $$("[data-scopeb-paste-mode]", d).forEach(function (b) { b.setAttribute("aria-pressed", b === m ? "true" : "false"); });
        return;
      }
      if (e.target.closest("[data-scopeb-paste-apply]")) {
        var l = parseList(ta.value);
        var next = mode === "replace" ? l : self.list.concat(l.filter(function (s) { return self.list.indexOf(s) < 0; }));
        d.close();
        ta.value = "";
        sync();
        self.replaceAll(next, "pasted");
      }
    });
    this.openPaste = function () {
      if (typeof d.showModal === "function") { if (!d.open) d.showModal(); } else d.setAttribute("open", "");
      var m = motion();
      if (m) m.enter($(".access-dialog__form", d), { dy: 8 });
      if (navigator.clipboard && navigator.clipboard.readText && window.isSecureContext && !ta.value) {
        navigator.clipboard.readText().then(function (t) { if (t && !ta.value) { ta.value = t; sync(); } }, function () {});
      }
      ta.focus();
    };
  };

  /* ---- rendering ------------------------------------------------------ */
  Builder.prototype.renderAll = function () {
    var self = this, root = this.root;
    $("[data-scopeb-count]", root).textContent = this.list.length ? String(this.list.length) : "";
    var rn = $("[data-scopeb-raw-n]", root);
    if (rn) rn.textContent = this.msg("count", { count: this.list.length });
    $$("[data-scopeb-nr]", root).forEach(function (i) { i.checked = self.list.indexOf(i.getAttribute("data-scopeb-nr")) >= 0; });
    var counts = { patient: 0, user: 0, system: 0 };
    this.list.forEach(function (s) { var r = parseRes(s); if (r) counts[r.ctx]++; });
    $$("[data-scopeb-ctx]", root).forEach(function (b) {
      var c = b.getAttribute("data-scopeb-ctx");
      b.setAttribute("aria-pressed", self.ctxs.indexOf(c) >= 0 ? "true" : "false");
    });
    $$("[data-scopeb-ctx-n]", root).forEach(function (n) {
      var c = counts[n.getAttribute("data-scopeb-ctx-n")];
      n.textContent = c ? String(c) : "";
      n.hidden = !c;
    });
    $$("[data-scopeb-ctx-help]", root).forEach(function (li) { li.hidden = self.ctxs.indexOf(li.getAttribute("data-scopeb-ctx-help")) < 0; });
    this.renderTable();
    this.renderChips();
  };

  Builder.prototype.visibleTypes = function (g) {
    var self = this, q = this.q;
    return g.types.filter(function (t) {
      if (q && t.toLowerCase().indexOf(q) < 0) return false;
      if (self.selectedOnly) return self.ctxs.some(function (c) { return self.explicit(c, t); });
      return true;
    });
  };

  Builder.prototype.renderTable = function (animateGroup) {
    var self = this, box = this.table;
    var multi = this.ctxs.length > 1;
    box.classList.toggle("is-multi", multi);
    var frag = document.createDocumentFragment();
    if (!this.ctxs.length) {
      frag.appendChild(el("p", "scopeb-table__empty", this.msg("none")));
      box.replaceChildren(frag);
      return;
    }
    // Header (wide screens): Type · [Context] · Create … Search · Quick · Scope
    var head = el("div", "scopeb-row scopeb-row--head");
    head.setAttribute("aria-hidden", "true");
    head.appendChild(el("span", "scopeb-row__type", this.msg("type")));
    var hl = el("span", "scopeb-row__lines");
    var hline = el("span", "scopeb-line");
    if (multi) hline.appendChild(el("span", "scopeb-line__ctx", ""));
    var hp = el("span", "scopeb-perms");
    this.cat.perms.forEach(function (p) { hp.appendChild(el("span", "scopeb-perm-h", p.label)); });
    hline.appendChild(hp);
    hline.appendChild(el("span", "scopeb-line__quick", this.msg("quick")));
    hline.appendChild(el("span", "scopeb-line__scope", this.msg("scope")));
    hl.appendChild(hline);
    head.appendChild(hl);
    frag.appendChild(head);

    // Every type (*)
    var any = (!this.q || "*".indexOf(this.q) >= 0 || this.msg("all-types").toLowerCase().indexOf(this.q) >= 0) &&
      (!this.selectedOnly || this.ctxs.some(function (c) { return self.explicit(c, "*"); }));
    if (any) frag.appendChild(this.row("*", this.msg("all-types"), true));

    var shown = 0, animRows = [];
    this.cat.groups.forEach(function (g) {
      var types = self.visibleTypes(g);
      if (!types.length) return;
      shown += types.length;
      var sel = g.types.filter(function (t) { return self.ctxs.some(function (c) { return self.explicit(c, t); }); }).length;
      var open = !!(self.open[g.id] || self.q || self.selectedOnly);
      var gh = el("button", "scopeb-group");
      gh.type = "button";
      gh.setAttribute("data-scopeb-group", g.id);
      gh.setAttribute("aria-expanded", open ? "true" : "false");
      gh.appendChild(el("span", "scopeb-group__chev", "›"));
      gh.appendChild(el("span", "scopeb-group__name", g.label));
      gh.appendChild(el("span", "scopeb-group__n", self.msg("selected", { selected: sel, count: types.length })));
      if (sel) gh.classList.add("has-selection");
      frag.appendChild(gh);
      if (!open) return;
      types.forEach(function (t) {
        var r = self.row(t, t, false);
        frag.appendChild(r);
        if (animateGroup === g.id) animRows.push(r);
      });
    });
    if (!shown && !any) frag.appendChild(el("p", "scopeb-table__empty", this.msg("no-match")));
    box.replaceChildren(frag);
    var m = motion();
    if (m && animRows.length) m.stagger(animRows, { max: 14 });
  };

  Builder.prototype.row = function (type, label, star) {
    var self = this;
    var row = el("div", "scopeb-row" + (star ? " scopeb-row--star" : ""));
    row.setAttribute("data-type", type);
    var name = el("span", "scopeb-row__type");
    name.appendChild(el("span", "scopeb-row__name", label));
    if (star) name.appendChild(el("code", "scopeb-row__star", "*"));
    row.appendChild(name);
    var lines = el("span", "scopeb-row__lines");
    var anySel = false;
    this.ctxs.forEach(function (ctx) {
      var bits = self.explicit(ctx, type), cover = self.cover(ctx, type);
      if (bits) anySel = true;
      var line = el("span", "scopeb-line");
      if (self.ctxs.length > 1) line.appendChild(el("code", "scopeb-line__ctx", ctx + "/"));
      var perms = el("span", "scopeb-perms");
      perms.setAttribute("role", "group");
      perms.setAttribute("aria-label", ctx + "/" + type);
      self.cat.perms.forEach(function (p) {
        var b = BIT[p.l];
        var lab = el("label", "scopeb-perm");
        var inp = el("input", "scopeb-perm__input");
        inp.type = "checkbox";
        inp.setAttribute("data-scopeb-perm", p.l);
        inp.setAttribute("data-ctx", ctx);
        inp.checked = !!((bits | cover) & b);
        var covered = !!(cover & b) && !(bits & b);
        if (covered) {
          inp.disabled = true;
          lab.classList.add("is-covered");
          lab.title = self.msg("covered", { scope: ctx + "/*." + letters(cover) });
        }
        inp.setAttribute("aria-label", p.label);
        lab.appendChild(inp);
        lab.appendChild(el("span", "scopeb-perm__full", p.label));
        lab.appendChild(el("span", "scopeb-perm__short", p.l.toUpperCase()));
        if (!covered) lab.title = p.label;
        perms.appendChild(lab);
      });
      line.appendChild(perms);
      var quick = el("span", "scopeb-line__quick");
      [["rs", "read-only"], ["cruds", "full"], ["", "clear"]].forEach(function (q) {
        if (q[0] === "" && !bits) return;
        var b = el("button", "btn btn--ghost btn--small scopeb-quick", self.msg(q[1]));
        b.type = "button";
        b.setAttribute("data-scopeb-quick", q[0]);
        b.setAttribute("data-ctx", ctx);
        b.setAttribute("aria-label", self.msg(q[1]) + " — " + ctx + "/" + type);
        if (q[0] === "rs" && bits === RS) b.setAttribute("aria-pressed", "true");
        if (q[0] === "cruds" && bits === ALL) b.setAttribute("aria-pressed", "true");
        quick.appendChild(b);
      });
      line.appendChild(quick);
      var out = el("span", "scopeb-line__scope");
      if (bits) {
        var tok = ctx + "/" + type + "." + letters(bits);
        out.appendChild(el("code", "scopeb-tok", tok));
        if (self.acceptV1()) {
          var v1 = v1Of(ctx, type, bits);
          if (v1) out.appendChild(el("code", "scopeb-tok scopeb-tok--v1", self.msg("v1", { scope: v1 })));
        }
        if ((bits & cover) === bits || self.redundant[tok]) {
          var tag = el("span", "scopeb-tag scopeb-tag--warn", self.msg("redundant"));
          tag.title = self.msg("covered", { scope: self.redundant[tok] || ctx + "/*." + letters(cover) });
          out.appendChild(tag);
        }
      } else {
        out.appendChild(el("span", "scopeb-line__none", "—"));
      }
      line.appendChild(out);
      lines.appendChild(line);
    });
    if (anySel) row.classList.add("is-selected");
    row.appendChild(lines);
    return row;
  };

  Builder.prototype.renderChips = function () {
    var self = this, box = $("[data-scopeb-chips]", this.root);
    var groups = [
      { id: "signin", label: this.msg("group-signin"), items: [] },
      { id: "patient", label: "patient/", items: [] },
      { id: "user", label: "user/", items: [] },
      { id: "system", label: "system/", items: [] },
      { id: "other", label: this.msg("group-other"), items: [] },
    ];
    var nr = $$("[data-scopeb-nr]", this.root).map(function (i) { return i.getAttribute("data-scopeb-nr"); });
    this.list.forEach(function (s) {
      var r = parseRes(s);
      var g = r ? r.ctx : nr.indexOf(s) >= 0 || s === "email" || /^launch\/[A-Za-z0-9]+(\?role=.+)?$/.test(s) ? "signin" : "other";
      groups.filter(function (x) { return x.id === g; })[0].items.push(s);
    });
    var frag = document.createDocumentFragment();
    if (!this.list.length) frag.appendChild(el("p", "scopeb-table__empty", this.msg("none")));
    groups.forEach(function (g) {
      if (!g.items.length) return;
      var row = el("div", "scopeb-chips__row");
      row.appendChild(el("span", "scopeb-chips__label", g.label));
      var ul = el("ul", "scopeb-chips__list");
      g.items.forEach(function (s) {
        var li = el("li", "scopeb-chip");
        if (self.invalid[s]) { li.classList.add("scopeb-chip--bad"); li.title = self.invalid[s]; }
        else if (self.redundant[s]) { li.classList.add("scopeb-chip--dup"); li.title = self.msg("covered", { scope: self.redundant[s] }); }
        li.appendChild(el("code", null, s));
        var x = el("button", "scopeb-chip__x", "×");
        x.type = "button";
        x.setAttribute("data-scopeb-rm", s);
        x.setAttribute("aria-label", self.msg("remove", { scope: s }));
        li.appendChild(x);
        ul.appendChild(li);
      });
      row.appendChild(ul);
      frag.appendChild(row);
    });
    box.replaceChildren(frag);
  };

  /* ---- live analysis -------------------------------------------------- */
  Builder.prototype.scheduleCheck = function (ms) {
    var self = this;
    clearTimeout(this.checkTimer);
    this.checkTimer = setTimeout(function () { self.check(); }, ms);
  };
  Builder.prototype.check = function () {
    var self = this, seq = ++this.seq;
    var url = this.root.getAttribute("data-check-url");
    if (!url) return;
    var p = new URLSearchParams();
    p.set("scopes", this.list.join(" "));
    p.set("kind", this.kind());
    p.set("launch", this.kind() === "backend" ? "" : this.launch());
    if (this.acceptV1()) p.set("accept_v1", "1");
    var box = $("[data-scopeb-findings]", this.root);
    box.setAttribute("aria-busy", "true");
    fetch(url + "?" + p.toString(), { credentials: "same-origin", headers: { Accept: "application/json" } })
      .then(function (r) { if (!r.ok) throw new Error("HTTP " + r.status); return r.json(); })
      .then(function (v) {
        if (seq !== self.seq) return;
        self.redundant = Object.create(null);
        (v.redundant || []).forEach(function (r) { self.redundant[r.scope] = r.coveredBy; });
        self.invalid = Object.create(null);
        (v.invalid || []).forEach(function (r) { self.invalid[r.scope] = r.message; });
        self.renderFindings(v);
        self.renderTable();
        self.renderChips();
      })
      .catch(function () { /* keep the last findings */ })
      .then(function () { if (seq === self.seq) box.removeAttribute("aria-busy"); });
  };
  Builder.prototype.renderFindings = function (v) {
    var box = $("[data-scopeb-findings]", this.root);
    var items = [];
    (v.invalid || []).forEach(function (f) { items.push({ warn: true, text: f.message }); });
    (v.findings || []).forEach(function (f) { items.push({ warn: f.level === "warn", text: f.message }); });
    (v.redundant || []).forEach(function (f) { items.push({ warn: false, text: f.message, dup: f.scope }); });
    items.sort(function (a, b) { return (b.warn ? 1 : 0) - (a.warn ? 1 : 0); });
    var frag = document.createDocumentFragment();
    var self = this;
    items.forEach(function (it) {
      var li = el("li", "scopeb-finding scopeb-finding--" + (it.warn ? "warn" : "info"));
      var ic = el("span", "scopeb-finding__icon", it.warn ? "!" : "i");
      ic.setAttribute("aria-hidden", "true");
      li.appendChild(ic);
      li.appendChild(el("span", "scopeb-finding__text", it.text));
      if (it.dup) {
        var b = el("button", "btn btn--ghost btn--small scopeb-finding__fix", self.msg("remove", { scope: it.dup }));
        b.type = "button";
        b.setAttribute("data-scopeb-rm", it.dup);
        li.appendChild(b);
      }
      frag.appendChild(li);
    });
    if (!items.length && this.list.length) {
      var ok = el("li", "scopeb-finding scopeb-finding--ok");
      var ic2 = el("span", "scopeb-finding__icon", "✓");
      ic2.setAttribute("aria-hidden", "true");
      ok.appendChild(ic2);
      ok.appendChild(el("span", "scopeb-finding__text", this.msg("all-good")));
      frag.appendChild(ok);
    }
    box.replaceChildren(frag);
  };

  function init(root) {
    $$("[data-scopeb]", root || document).forEach(function (b) {
      if (b.__scopeb) return;
      b.__scopeb = new Builder(b);
    });
  }
  window.HfsScopes = { init: init, parse: parseRes, parseList: parseList };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", function () { init(document); });
  else init(document);
  document.addEventListener("htmx:afterSwap", function (e) { init(e.target); });
  document.addEventListener("hm:navigated", function () { init(document); });
})();
