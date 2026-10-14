//! Introspection trait for terminology backends.
//!
//! [`TerminologyMetadata`] exposes the static metadata used by the `/metadata`
//! endpoint to build a FHIR `TerminologyCapabilities` resource.
#![allow(dead_code)]

/// Introspection trait for terminology backends.
///
/// Returns static metadata used by the `/metadata` endpoint to build a
/// `TerminologyCapabilities` resource.  All methods are synchronous because
/// the metadata is expected to be available in memory.
pub trait TerminologyMetadata: Send + Sync {
    /// A short name identifying this backend (e.g. `"sqlite"`, `"postgres"`).
    fn backend_name(&self) -> &'static str;

    /// The canonical URLs of all code systems currently stored in this backend.
    ///
    /// Used to populate `TerminologyCapabilities.codeSystem[].uri`.
    fn supported_systems(&self) -> Vec<String>;

    /// Whether this backend supports the `$subsumes` operation.
    ///
    /// Returns `false` for backends that have not imported hierarchy data.
    fn supports_subsumption(&self) -> bool;

    /// Return the canonical URL for a resource identified by its FHIR `id`.
    ///
    /// `resource_type` must be one of `"ValueSet"` or `"ConceptMap"`.
    /// Returns [`None`] when no resource with that ID exists.
    ///
    /// Used by instance-level operation handlers (e.g. `GET /ValueSet/{id}/$expand`)
    /// to resolve the canonical URL before delegating to system-level logic.
    fn resource_url_by_id(&self, resource_type: &str, id: &str) -> Option<String>;

    /// Return the stored FHIR JSON for a resource identified by its FHIR `id`,
    /// read from the normalized terminology tables (`resource_json`).
    ///
    /// `resource_type` must be one of `"CodeSystem"`, `"ValueSet"` or
    /// `"ConceptMap"`. Matches the storage id, the synthetic
    /// `<fhir-id>|<version>` storage ids and the `id` captured in
    /// `resource_json`, picking the preferred row when several versions share
    /// one FHIR id. Returns [`None`] when the id is unknown.
    ///
    /// Used by the REST read/update/delete handlers so resources that reached
    /// the store only through `POST /import`, the CLI importer or bootstrap
    /// (which bypass the persistence layer) are still addressable by id.
    fn resource_json_by_id(&self, resource_type: &str, id: &str) -> Option<serde_json::Value>;
}
