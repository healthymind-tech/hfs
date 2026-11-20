// Console CSRF token (design §7.3, crates/admin-auth console sign-in).
//
// With console sign-in on, every state-changing request made with the
// session cookie must carry the session's synchronizer token. The server
// sets it in the `hfs_csrf` cookie (SameSite=Strict, readable by this
// page only) and checks it against the session it stored — the cookie is
// just how the token reaches this script. This file attaches it to:
//   * htmx requests        → X-CSRF-Token header
//   * fetch() to this site → X-CSRF-Token header (non-GET/HEAD)
//   * plain <form method=post> submissions → hidden `_csrf` field
// Without console sign-in there is no cookie and nothing is added.
(function () {
  "use strict";
  function token() {
    var m = document.cookie.match(/(?:^|;\s*)hfs_csrf=([^;]+)/);
    return m ? decodeURIComponent(m[1]) : "";
  }
  function sameOrigin(url) {
    try {
      return new URL(url, window.location.href).origin === window.location.origin;
    } catch (e) {
      return false;
    }
  }
  document.addEventListener("htmx:configRequest", function (event) {
    var t = token();
    if (t && event.detail && event.detail.headers) {
      event.detail.headers["X-CSRF-Token"] = t;
    }
  });
  if (window.fetch) {
    var original = window.fetch;
    window.fetch = function (input, init) {
      var t = token();
      var method = ((init && init.method) || (input && input.method) || "GET").toUpperCase();
      var url = typeof input === "string" ? input : input && input.url;
      if (t && method !== "GET" && method !== "HEAD" && sameOrigin(url)) {
        init = Object.assign({}, init || {});
        var headers = new Headers(init.headers || (typeof input !== "string" && input.headers) || {});
        if (!headers.has("X-CSRF-Token")) headers.set("X-CSRF-Token", t);
        init.headers = headers;
      }
      return original.call(this, input, init);
    };
  }
  document.addEventListener(
    "submit",
    function (event) {
      var form = event.target;
      if (!(form instanceof HTMLFormElement)) return;
      if ((form.getAttribute("method") || "get").toLowerCase() !== "post") return;
      var t = token();
      if (!t || !sameOrigin(form.action)) return;
      var field = form.querySelector('input[name="_csrf"]');
      if (!field) {
        field = document.createElement("input");
        field.type = "hidden";
        field.name = "_csrf";
        form.appendChild(field);
      }
      field.value = t;
    },
    true
  );
})();
