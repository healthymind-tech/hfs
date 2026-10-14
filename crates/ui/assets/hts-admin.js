// HTS terminology archive import — source switcher, resumable chunked upload,
// registry package picker. Loaded by `/ui/hts/import`
// (crates/hts-ui/templates/partials/hts-import-archive.html).
//
// Upload protocol (docs/hts-terminology-admin.md): POST {api}/uploads creates
// or — for the same file name + size + mtime — resumes a session; each chunk
// is a PUT {api}/uploads/{id}/chunks/{i}; POST …/complete hashes and detects.
// The server keeps the session, so a reloaded or killed browser resumes by
// picking the same file again: only the missing chunks are sent.
//
// Without JavaScript the panels stay visible and the import-dir / registry
// sources still work through the plain form POST.
(function () {
  "use strict";

  var form = document.getElementById("hts-admin-import");
  if (!form) return;

  var api = form.dataset.api;
  var msg = form.dataset;
  var PARALLEL = 3;
  var RETRIES = 5;

  function fmt(template, values) {
    return String(template || "").replace(/\{(\w+)\}/g, function (_, k) {
      return values[k] !== undefined ? values[k] : "";
    });
  }

  function bytes(n) {
    var units = ["B", "KB", "MB", "GB", "TB"];
    var u = 0;
    while (n >= 1000 && u < units.length - 1) {
      n /= 1000;
      u++;
    }
    return (u === 0 ? n : n.toFixed(1)) + " " + units[u];
  }

  function duration(s) {
    if (!isFinite(s) || s < 0) return "–";
    s = Math.round(s);
    var h = Math.floor(s / 3600);
    var m = Math.floor((s % 3600) / 60);
    var sec = s % 60;
    var mm = (h ? String(m).padStart(2, "0") : m) + ":" + String(sec).padStart(2, "0");
    return h ? h + ":" + mm : mm;
  }

  // ── Source switcher ────────────────────────────────────────────────────
  var radios = form.querySelectorAll('input[name="admin_source"]');
  var panels = form.querySelectorAll("[data-source-panel]");

  function selectSource(value) {
    radios.forEach(function (r) {
      r.checked = r.value === value;
    });
    applySource();
  }

  function applySource() {
    var value = "upload";
    radios.forEach(function (r) {
      if (r.checked) value = r.value;
    });
    panels.forEach(function (p) {
      p.hidden = p.dataset.sourcePanel !== value;
    });
  }

  radios.forEach(function (r) {
    r.addEventListener("change", applySource);
  });
  applySource();

  // ── Registry picker ────────────────────────────────────────────────────
  var pkgInput = document.getElementById("hts-reg-package");
  form.addEventListener("click", function (ev) {
    var btn = ev.target.closest && ev.target.closest("[data-pick-package]");
    if (!btn || !pkgInput) return;
    pkgInput.value = btn.dataset.pickPackage;
    selectSource("registry");
    pkgInput.focus();
  });

  // ── Chunked upload ─────────────────────────────────────────────────────
  var drop = document.getElementById("hts-up-drop");
  var input = document.getElementById("hts-up-file");
  var box = document.getElementById("hts-up-progress");
  var bar = document.getElementById("hts-up-bar");
  var fill = document.getElementById("hts-up-fill");
  var status = document.getElementById("hts-up-status");
  var errorEl = document.getElementById("hts-up-error");
  var pauseBtn = document.getElementById("hts-up-pause");
  var resumeBtn = document.getElementById("hts-up-resume");
  var detBox = document.getElementById("hts-up-detection");
  var list = document.getElementById("hts-up-list");
  var none = document.getElementById("hts-up-none");
  if (!drop || !input) return;

  var current = null; // { file, session, received, controller, paused, samples }

  function setProgress(received, total) {
    var pct = total ? Math.floor((received / total) * 1000) / 10 : 0;
    fill.style.width = pct + "%";
    bar.setAttribute("aria-valuenow", String(Math.floor(pct)));
    return pct;
  }

  function showError(text) {
    errorEl.textContent = text;
    errorEl.hidden = !text;
  }

  function speed() {
    var s = current.samples;
    if (s.length < 2) return 0;
    var a = s[0];
    var b = s[s.length - 1];
    var dt = (b.t - a.t) / 1000;
    return dt > 0 ? (b.bytes - a.bytes) / dt : 0;
  }

  function report() {
    var total = current.file.size;
    var pct = setProgress(current.received, total);
    current.samples.push({ t: Date.now(), bytes: current.received });
    while (current.samples.length > 2 && Date.now() - current.samples[0].t > 10000) {
      current.samples.shift();
    }
    var bps = speed();
    status.textContent = fmt(msg.msgUploading, {
      name: current.file.name,
      done: bytes(current.received),
      total: bytes(total),
      pct: pct,
      speed: bytes(bps),
      eta: duration(bps > 0 ? (total - current.received) / bps : NaN),
    });
  }

  async function hashHex(buf) {
    if (!(window.crypto && window.crypto.subtle)) return null; // insecure context
    var digest = await window.crypto.subtle.digest("SHA-256", buf);
    return Array.from(new Uint8Array(digest))
      .map(function (b) {
        return b.toString(16).padStart(2, "0");
      })
      .join("");
  }

  async function sendChunk(index) {
    var s = current.session;
    var start = index * s.chunk_size;
    var end = Math.min(start + s.chunk_size, current.file.size);
    var blob = current.file.slice(start, end);
    var buf = await blob.arrayBuffer();
    var sum = await hashHex(buf);
    for (var attempt = 0; ; attempt++) {
      if (current.paused) throw new Error("paused");
      try {
        var headers = { "Content-Type": "application/octet-stream" };
        if (sum) headers["X-Chunk-Sha256"] = sum;
        var r = await fetch(api + "/uploads/" + s.id + "/chunks/" + index, {
          method: "PUT",
          headers: headers,
          body: buf,
          signal: current.controller.signal,
        });
        if (r.ok || r.status === 409) {
          current.received += end - start;
          report();
          return;
        }
        var body = await r.json().catch(function () {
          return {};
        });
        if (r.status >= 400 && r.status < 500 && r.status !== 408 && r.status !== 429) {
          throw new Error(body.message || "HTTP " + r.status);
        }
        if (attempt >= RETRIES) throw new Error(body.message || "HTTP " + r.status);
      } catch (e) {
        if (current.paused || (e && e.name === "AbortError")) throw new Error("paused");
        if (attempt >= RETRIES) throw e;
      }
      await new Promise(function (res) {
        setTimeout(res, 1000 * Math.pow(2, attempt));
      });
    }
  }

  async function run() {
    var s = current.session;
    var queue = (s.missing || []).slice();
    current.received = s.received_bytes || 0;
    report();
    async function worker() {
      while (queue.length) {
        await sendChunk(queue.shift());
      }
    }
    var workers = [];
    for (var i = 0; i < PARALLEL; i++) workers.push(worker());
    await Promise.all(workers);
    status.textContent = msg.msgVerifying;
    var r = await fetch(api + "/uploads/" + s.id + "/complete", { method: "POST" });
    var done = await r.json().catch(function () {
      return {};
    });
    if (r.status === 409) {
      // Some chunk was lost (e.g. the server restarted mid-write): refetch
      // the session and send what is missing.
      current.session = await (await fetch(api + "/uploads/" + s.id)).json();
      return run();
    }
    if (!r.ok) throw new Error(done.message || "HTTP " + r.status);
    finished(done);
  }

  async function finished(meta) {
    setProgress(1, 1);
    status.textContent = fmt(msg.msgDone, { name: meta.filename });
    pauseBtn.hidden = true;
    resumeBtn.hidden = true;
    var det = meta.detection || {};
    document.getElementById("hts-up-det-format").textContent = det.format
      ? fmt(msg.msgDetected, { format: det.format })
      : msg.msgUndetected;
    document.getElementById("hts-up-det-version").textContent = det.version || "—";
    document.getElementById("hts-up-det-growth").textContent = det.growth_estimate_bytes
      ? "≈ " + bytes(det.growth_estimate_bytes)
      : "—";
    document.getElementById("hts-up-det-sha").textContent = meta.sha256 || "";
    detBox.hidden = false;
    try {
      var d = await (await fetch(api + "/uploads/" + meta.id + "/detect")).json();
      if (d.space && !d.space.ok) {
        showError(
          fmt(msg.msgSpace, {
            free: bytes(d.space.free_bytes || 0),
            need: bytes(d.space.required_bytes || 0),
          })
        );
      }
    } catch (_) {
      /* the check runs again when the job starts */
    }
    // Offer it in the "uploaded files" list, selected.
    var li = document.createElement("li");
    li.className = "query-row";
    var label = document.createElement("label");
    var radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "upload_id";
    radio.value = meta.id;
    radio.checked = true;
    var code = document.createElement("code");
    code.textContent = meta.filename;
    label.appendChild(radio);
    label.appendChild(document.createTextNode(" "));
    label.appendChild(code);
    label.appendChild(
      document.createTextNode(" · " + bytes(meta.size) + (det.format ? " · " + det.format : ""))
    );
    li.appendChild(label);
    list.insertBefore(li, list.firstChild);
    if (none) none.hidden = true;
    selectSource("upload");
    form.dataset.uploaded = meta.id;
  }

  async function start(file) {
    showError("");
    detBox.hidden = true;
    box.hidden = false;
    pauseBtn.hidden = false;
    resumeBtn.hidden = true;
    current = {
      file: file,
      session: null,
      received: 0,
      controller: new AbortController(),
      paused: false,
      samples: [],
    };
    try {
      var key = file.name + "|" + file.size + "|" + file.lastModified;
      var r = await fetch(api + "/uploads", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ filename: file.name, size: file.size, client_key: key }),
      });
      var s = await r.json().catch(function () {
        return {};
      });
      if (!r.ok) throw new Error(s.message || "HTTP " + r.status);
      current.session = s;
      if (r.status === 200 && s.received_bytes > 0) {
        status.textContent = fmt(msg.msgResumed, {
          name: file.name,
          pct: Math.floor((s.received_bytes / s.size) * 100),
        });
      }
      form.dataset.uploadId = s.id;
      await run();
    } catch (e) {
      if (current.paused) return;
      pauseBtn.hidden = true;
      showError(fmt(msg.msgFailed, { error: e.message || String(e) }));
    }
  }

  pauseBtn.addEventListener("click", function () {
    if (!current) return;
    current.paused = true;
    current.controller.abort();
    pauseBtn.hidden = true;
    resumeBtn.hidden = false;
    status.textContent = fmt(msg.msgPaused, {
      pct: Math.floor((current.received / current.file.size) * 100),
    });
  });

  resumeBtn.addEventListener("click", function () {
    if (current) start(current.file);
  });

  drop.addEventListener("click", function () {
    input.click();
  });
  input.addEventListener("change", function () {
    if (input.files && input.files[0]) start(input.files[0]);
  });
  drop.addEventListener("dragover", function (ev) {
    ev.preventDefault();
    drop.classList.add("batch-drop--over");
  });
  drop.addEventListener("dragleave", function () {
    drop.classList.remove("batch-drop--over");
  });
  drop.addEventListener("drop", function (ev) {
    ev.preventDefault();
    drop.classList.remove("batch-drop--over");
    var f = ev.dataTransfer && ev.dataTransfer.files && ev.dataTransfer.files[0];
    if (f) start(f);
  });
})();
