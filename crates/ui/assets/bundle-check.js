/*
 * Client-side Bundle checks shared by the Batch / Transaction page and the
 * Validate page: JSON syntax with a line:column position, and the request
 * shape a batch/transaction Bundle must have before it is worth sending.
 *
 * `window.HfsBundleCheck`:
 *   - `check(text, options)` -> report (below). `options.maxBytes` is the
 *     server's body limit (0 = unknown); `options.messages` the localized
 *     templates (`messagesFrom(element)`); `options.requireTransaction`
 *     (default true) applies the Bundle request rules, false only checks
 *     the syntax (the Validate page accepts any resource).
 *   - `format(text)` -> pretty-printed JSON (throws on a syntax error).
 *   - `locate(text, offset)` -> { line, col }, both 1-based.
 *   - `messagesFrom(element)` -> templates from its `data-msg-*` attributes.
 *
 * A report is `{ bytes, value, errors, warnings, summary }`. Every problem
 * is `{ code, message, entry, line, col }` (`entry` is the 0-based entry
 * index or null; `line`/`col` null when the position is unknown). `summary`
 * is `{ entries, byMethod, byType }` (counts), or null when the text did not
 * parse as a Bundle. Errors block submission; warnings do not.
 *
 * Positions come from a small validating scanner run over the text: it
 * reports the first syntax error the same way in every engine (JSON.parse's
 * messages differ per browser and only some carry a position) and records
 * where the first levels of the Bundle start, so an entry-level problem
 * points at the line the user must edit. Above POSITION_LIMIT characters the
 * position map is skipped (syntax errors are still located).
 *
 * Message templates use %name% placeholders, filled here.
 */
(function () {
  "use strict";

  var METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];
  var POSITION_LIMIT = 8 * 1024 * 1024;
  var TYPE_RE = /^[A-Z][A-Za-z]{1,63}$/;
  var ID_RE = /^[A-Za-z0-9\-.]{1,64}$/;

  function fill(template, values) {
    return String(template || "").replace(/%([a-z_]+)%/g, function (all, name) {
      return Object.prototype.hasOwnProperty.call(values, name) ? String(values[name]) : all;
    });
  }

  function messagesFrom(element) {
    var out = {};
    if (!element) return out;
    var data = element.dataset;
    for (var key in data) {
      if (key.indexOf("msg") === 0) out[key.charAt(3).toLowerCase() + key.slice(4)] = data[key];
    }
    return out;
  }

  function locate(text, offset) {
    var line = 1;
    var last = -1;
    var end = Math.min(offset, text.length);
    for (var i = 0; i < end; i++) {
      if (text.charCodeAt(i) === 10) {
        line++;
        last = i;
      }
    }
    return { line: line, col: end - last };
  }

  /* ---- the scanner --------------------------------------------------- */

  var NUMBER = /-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/y;

  function SyntaxProblem(offset, kind, detail) {
    this.offset = offset;
    this.kind = kind;
    this.detail = detail;
  }

  /* Walks the whole text. `record(path, offset)` is called for every value
     no deeper than four levels (`entry[3].request.method`), when given. */
  function scan(text, record) {
    var i = 0;
    var n = text.length;

    function fail(kind, detail) {
      throw new SyntaxProblem(i, kind, detail);
    }

    function ws() {
      while (i < n) {
        var c = text.charCodeAt(i);
        if (c === 32 || c === 9 || c === 10 || c === 13) i++;
        else break;
      }
    }

    function string(capture) {
      var start = ++i;
      var escaped = false;
      while (i < n) {
        var c = text.charCodeAt(i);
        if (c === 34) {
          var raw = capture ? text.slice(start, i) : null;
          i++;
          return escaped && raw ? JSON.parse('"' + raw + '"') : raw;
        }
        if (c === 92) {
          escaped = true;
          var next = text.charAt(i + 1);
          if (next === "u") {
            if (!/^[0-9a-fA-F]{4}$/.test(text.substr(i + 2, 4))) {
              i++;
              fail("unexpected", next);
            }
            i += 6;
            continue;
          }
          if ('"\\/bfnrt'.indexOf(next) < 0 || next === "") {
            i++;
            if (i >= n) fail("eof");
            fail("unexpected", next);
          }
          i += 2;
          continue;
        }
        if (c < 32) fail("unexpected", "U+" + ("000" + c.toString(16).toUpperCase()).slice(-4));
        i++;
      }
      fail("eof");
    }

    function value(path, depth) {
      ws();
      if (i >= n) fail("eof");
      if (record && depth <= 4) record(path, i);
      var c = text.charAt(i);
      if (c === "{") return object(path, depth);
      if (c === "[") return array(path, depth);
      if (c === '"') return string(false);
      if (c === "-" || (c >= "0" && c <= "9")) {
        NUMBER.lastIndex = i;
        var m = NUMBER.exec(text);
        if (!m || m.index !== i || m[0] === "-") {
          i++;
          fail(i >= n ? "eof" : "unexpected", text.charAt(i));
        }
        i += m[0].length;
        return;
      }
      var words = ["true", "false", "null"];
      for (var w = 0; w < words.length; w++) {
        if (text.substr(i, words[w].length) === words[w]) {
          i += words[w].length;
          return;
        }
      }
      fail("unexpected", c);
    }

    function object(path, depth) {
      i++;
      ws();
      if (text.charAt(i) === "}") {
        i++;
        return;
      }
      for (;;) {
        ws();
        if (i >= n) fail("eof");
        if (text.charAt(i) !== '"') fail("expected", '"');
        var key = string(record && depth < 4);
        ws();
        if (i >= n) fail("eof");
        if (text.charAt(i) !== ":") fail("expected", ":");
        i++;
        value(key === null ? path : path ? path + "." + key : key, depth + 1);
        ws();
        if (i >= n) fail("eof");
        var c = text.charAt(i);
        if (c === ",") {
          i++;
          continue;
        }
        if (c === "}") {
          i++;
          return;
        }
        fail("expected", ", }");
      }
    }

    function array(path, depth) {
      i++;
      ws();
      if (text.charAt(i) === "]") {
        i++;
        return;
      }
      for (var index = 0; ; index++) {
        value(path + "[" + index + "]", depth + 1);
        ws();
        if (i >= n) fail("eof");
        var c = text.charAt(i);
        if (c === ",") {
          i++;
          continue;
        }
        if (c === "]") {
          i++;
          return;
        }
        fail("expected", ", ]");
      }
    }

    value("", 0);
    ws();
    if (i < n) fail("unexpected", text.charAt(i));
  }

  /* ---- the report ---------------------------------------------------- */

  function humanBytes(n) {
    if (n >= 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + " MiB";
    if (n >= 1024) return (n / 1024).toFixed(1) + " KiB";
    return n + " B";
  }

  function byteLength(text) {
    if (window.TextEncoder) return new TextEncoder().encode(text).length;
    return new Blob([text]).size;
  }

  function check(text, options) {
    options = options || {};
    var m = options.messages || {};
    var requireTransaction = options.requireTransaction !== false;
    var report = { bytes: 0, value: undefined, errors: [], warnings: [], summary: null };
    text = String(text == null ? "" : text);
    report.bytes = byteLength(text);

    var positions = null;

    function problem(list, code, values, entry, path) {
      var at = null;
      if (positions && path != null) {
        var probe = path;
        // Fall back from the exact field to the nearest recorded ancestor.
        while (probe && positions[probe] === undefined) {
          var cut = Math.max(probe.lastIndexOf("."), probe.lastIndexOf("["));
          probe = cut > 0 ? probe.slice(0, cut) : "";
        }
        if (positions[probe] !== undefined) at = locate(text, positions[probe]);
      }
      var message = fill(m[code] || code, values || {});
      list.push({
        code: code,
        message: message,
        entry: entry == null ? null : entry,
        line: at ? at.line : null,
        col: at ? at.col : null,
      });
    }

    if (options.maxBytes && report.bytes > options.maxBytes) {
      problem(report.errors, "tooLarge", {
        size: humanBytes(report.bytes),
        limit: humanBytes(options.maxBytes),
      });
    }

    if (!text.trim()) {
      problem(report.errors, "empty", {});
      return report;
    }

    try {
      report.value = JSON.parse(text);
    } catch (native) {
      var where = null;
      var detail = native && native.message;
      try {
        scan(text, null);
      } catch (e) {
        if (!(e instanceof SyntaxProblem)) throw e;
        where = locate(text, e.offset);
        detail =
          e.kind === "eof"
            ? m.jsonEof
            : fill(e.kind === "expected" ? m.jsonExpected : m.jsonUnexpected, {
                char: e.detail,
                expected: e.detail,
              });
      }
      report.errors.push({
        code: "jsonSyntax",
        message: fill(m.jsonSyntax || "%detail%", { detail: detail || "" }),
        entry: null,
        line: where ? where.line : null,
        col: where ? where.col : null,
      });
      return report;
    }

    if (text.length <= POSITION_LIMIT) {
      positions = {};
      try {
        scan(text, function (path, offset) {
          positions[path] = offset;
        });
      } catch (e) {
        positions = null;
      }
    }

    var value = report.value;
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      problem(report.errors, "notObject", {}, null, "");
      return report;
    }
    if (!requireTransaction) return report;

    if (value.resourceType !== "Bundle") {
      problem(report.errors, "notBundle", { found: value.resourceType || "—" }, null, "resourceType");
      return report;
    }
    if (!value.type) {
      problem(report.errors, "missingType", {}, null, "");
    } else if (value.type !== "batch" && value.type !== "transaction") {
      problem(report.errors, "badType", { found: value.type }, null, "type");
    }
    var transaction = value.type === "transaction";

    var summary = { entries: 0, byMethod: {}, byType: {} };
    report.summary = summary;
    if (value.entry !== undefined && !Array.isArray(value.entry)) {
      problem(report.errors, "entryNotArray", {}, null, "entry");
      return report;
    }
    var entries = value.entry || [];
    summary.entries = entries.length;
    if (!entries.length) problem(report.warnings, "noEntries", {}, null, "");

    var fullUrls = {};
    var references = [];

    entries.forEach(function (entry, index) {
      var n = index + 1;
      var path = "entry[" + index + "]";
      if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
        problem(report.errors, "entryNotObject", { entry: n }, index, path);
        return;
      }
      var request = entry.request;
      var resource = entry.resource;
      var method = request && typeof request.method === "string" ? request.method : null;
      var methodUpper = method ? method.toUpperCase() : null;
      var url = request && typeof request.url === "string" ? request.url.trim() : "";

      var type = resource && typeof resource === "object" ? resource.resourceType : null;
      if (!type && url) {
        var head = url.split(/[/?]/)[0];
        if (TYPE_RE.test(head)) type = head;
      }
      var methodKey = methodUpper || "?";
      summary.byMethod[methodKey] = (summary.byMethod[methodKey] || 0) + 1;
      var typeKey = type || "?";
      summary.byType[typeKey] = (summary.byType[typeKey] || 0) + 1;

      if (!request || typeof request !== "object") {
        problem(report.errors, "noRequest", { entry: n }, index, path);
      } else {
        if (!method) {
          problem(report.errors, "noMethod", { entry: n }, index, path + ".request");
        } else if (METHODS.indexOf(method) < 0) {
          problem(report.errors, "badMethod", { entry: n, method: method }, index, path + ".request.method");
        }
        if (!url) {
          problem(report.errors, "noUrl", { entry: n }, index, path + ".request");
        } else if (methodUpper === "PUT" || methodUpper === "DELETE" || methodUpper === "PATCH") {
          var instance = /^([A-Za-z]+)\/([^/?#]+)$/.exec(url);
          var conditional = /^([A-Za-z]+)\?.+/.exec(url);
          if (!(instance && TYPE_RE.test(instance[1]) && ID_RE.test(instance[2])) && !(conditional && TYPE_RE.test(conditional[1]))) {
            problem(report.errors, "badUrl", { entry: n, method: methodUpper, url: url }, index, path + ".request.url");
          } else if (instance && resource && resource.resourceType) {
            if (instance[1] !== resource.resourceType && methodUpper === "PUT") {
              problem(report.warnings, "urlTypeMismatch", { entry: n, url_type: instance[1], type: resource.resourceType }, index, path + ".request.url");
            } else if (methodUpper === "PUT" && resource.id && resource.id !== instance[2]) {
              problem(report.warnings, "idMismatch", { entry: n, url_id: instance[2], id: resource.id }, index, path + ".request.url");
            }
          }
        } else if (methodUpper === "POST" && resource && resource.resourceType) {
          var target = url.split(/[/?]/)[0];
          if (TYPE_RE.test(target) && target !== resource.resourceType) {
            problem(report.warnings, "urlTypeMismatch", { entry: n, url_type: target, type: resource.resourceType }, index, path + ".request.url");
          }
        }
      }

      var needsBody = methodUpper === "POST" || methodUpper === "PUT" || methodUpper === "PATCH";
      if (resource !== undefined && resource !== null) {
        if (typeof resource !== "object" || Array.isArray(resource) || typeof resource.resourceType !== "string" || !resource.resourceType) {
          problem(report.errors, "noResourceType", { entry: n }, index, path + ".resource");
        }
      } else if (needsBody) {
        problem(report.errors, "noResource", { entry: n, method: methodUpper }, index, path);
      }

      if (typeof entry.fullUrl === "string" && entry.fullUrl) {
        if (fullUrls[entry.fullUrl] !== undefined) {
          if (transaction) {
            problem(report.warnings, "duplicateFullUrl", { entry: n, fullurl: entry.fullUrl, other: fullUrls[entry.fullUrl] + 1 }, index, path + ".fullUrl");
          }
        } else {
          fullUrls[entry.fullUrl] = index;
        }
      }
      if (transaction && resource && typeof resource === "object") collectUrnReferences(resource, index, references);
    });

    if (transaction) {
      var reported = {};
      references.forEach(function (ref) {
        if (fullUrls[ref.value] !== undefined) return;
        var key = ref.entry + "|" + ref.value;
        if (reported[key]) return;
        reported[key] = true;
        problem(report.warnings, "unresolvedUrn", { entry: ref.entry + 1, ref: ref.value }, ref.entry, "entry[" + ref.entry + "].resource");
      });
    }
    return report;
  }

  /* Every `reference: "urn:uuid:…"` inside one entry's resource. */
  function collectUrnReferences(node, entry, out) {
    var stack = [node];
    var guard = 0;
    while (stack.length && guard++ < 200000) {
      var current = stack.pop();
      if (Array.isArray(current)) {
        for (var i = 0; i < current.length; i++) {
          if (current[i] && typeof current[i] === "object") stack.push(current[i]);
        }
        continue;
      }
      for (var key in current) {
        var v = current[key];
        if (key === "reference" && typeof v === "string" && v.indexOf("urn:uuid:") === 0) {
          out.push({ entry: entry, value: v });
        } else if (v && typeof v === "object") {
          stack.push(v);
        }
      }
    }
  }

  function format(text) {
    return JSON.stringify(JSON.parse(text), null, 2) + "\n";
  }

  window.HfsBundleCheck = {
    check: check,
    format: format,
    locate: locate,
    messagesFrom: messagesFrom,
    humanBytes: humanBytes,
    METHODS: METHODS,
  };
})();
