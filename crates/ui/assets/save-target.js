/* The one save-target rule for "create" (#1751), shared by the Resources
 * modal (resources.js) and the full-page editor (editor.js): a document that
 * carries an `id` is written with `PUT /{type}/{id}`, one without with
 * `POST /{type}`. `notice` is the "will be saved as" text the header shows
 * before saving.
 *
 * UMD wrapper — same shape as `unsaved.js` — so `e2e/unit/save-target.test.cjs`
 * can `require()` this file under plain Node: nothing here touches `window`
 * or `document`.
 */
(function (root, factory) {
  "use strict";

  var api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.HfsSaveTarget = api;
})(typeof window !== "undefined" ? window : null, function () {
  "use strict";

  /* A FHIR id as the server accepts it. */
  function isValidId(id) {
    return typeof id === "string" && /^[A-Za-z0-9.-]{1,64}$/.test(id);
  }

  /* -> { method, url, id }. An id with invalid characters still goes by PUT:
   * the server is the one that rejects it. */
  function forCreate(type, doc) {
    var id = doc && typeof doc.id === "string" && doc.id !== "" ? doc.id : null;
    if (id === null) return { method: "POST", url: "/" + type, id: null };
    return { method: "PUT", url: "/" + type + "/" + encodeURIComponent(id), id: id };
  }

  /* -> the notice text, or "" when the document has no usable id. */
  function notice(type, doc, template) {
    if (!doc || !isValidId(doc.id)) return "";
    return String(template).replace("{target}", type + "/" + doc.id);
  }

  /* The status of the "does this id exist" probe -> whether it does. Only a
   * 200 means yes; anything else (404, 410, 401, 5xx...) lets the save go on,
   * the server has the last word. */
  function existsFromStatus(status) {
    return status === 200;
  }

  /* The `If-Match` of an update of `doc` (`W/"<meta.versionId>"`), or "" when
   * the document carries no version (a new resource, a pasted document).
   * Saving an existing resource with the version it was loaded at turns a
   * concurrent edit (another tab, another operator) into a 412 instead of
   * silently overwriting it. */
  function ifMatch(doc) {
    var v = doc && doc.meta && doc.meta.versionId;
    if (typeof v !== "string" || !/^[A-Za-z0-9.-]{1,64}$/.test(v)) return "";
    return 'W/"' + v + '"';
  }

  return {
    ifMatch: ifMatch,
    isValidId: isValidId,
    forCreate: forCreate,
    notice: notice,
    existsFromStatus: existsFromStatus,
  };
});
