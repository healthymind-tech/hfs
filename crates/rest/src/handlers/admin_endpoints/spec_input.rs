//! Request bodies: an [`EndpointSpec`] plus write-only secret inputs.
//!
//! The body of `POST /admin/endpoints` and `PUT /admin/endpoints/{name}` is
//! the spec JSON (camelCase, design §4.1) with one extra member:
//!
//! ```json
//! { "name": "twcore", "displayName": "TW Core",
//!   "terminology": {"serverUrl": "https://tx.example.org/fhir"},
//!   "secrets": {"terminology.token": {"set": "s3cr3t"}} }
//! ```
//!
//! * Secret fields ([`SECRET_FIELDS`]) are **write-only**: inside the spec they
//!   may only be absent, `null` or the redacted `{"set": true}` a GET returns
//!   (all meaning "keep"), so a client that PUTs back what it read never
//!   clears or overwrites a secret by accident. Changes go through
//!   `secrets: {"<field path>": {"set": "…"} | "keep" | "clear"}`.
//! * Server-managed members of a GET response (`id`, `revision`,
//!   `createdAt`, `updatedAt`, `createdBy`, `updatedBy`, `publicBaseUrl`,
//!   `warnings`) are ignored, so a GET
//!   body can be edited and PUT back as is.
//! * Unknown members are refused (`422`), so a typo such as `rateLimits`
//!   is not silently dropped.
//! * On create, `path` defaults to the name, `storage` to the tenant named
//!   like the endpoint, and `displayName` to the name.
//! * Name and path are checked against the segment grammar and the reserved
//!   list ([`helios_endpoints::RESERVED_SEGMENTS`]) with one issue per field,
//!   together with every other problem [`EndpointSpec::validate`] finds.

use std::collections::{BTreeMap, BTreeSet};

use helios_endpoints::model::EndpointSpec;
use helios_endpoints::name::{EndpointName, check_segment};
use helios_endpoints::{FhirRelease, SecretInput};
use serde_json::{Map, Value, json};

use super::error::{ApiError, ApiResult, Issue};

/// Fields of [`EndpointSpec`] that hold a sealed secret, as JSON paths.
pub const SECRET_FIELDS: &[&str] = &["terminology.token"];

/// Members of an `Endpoint` response that are not part of the spec.
const SERVER_FIELDS: &[&str] = &[
    "id",
    "revision",
    "createdAt",
    "updatedAt",
    "createdBy",
    "updatedBy",
    "publicBaseUrl",
    "warnings",
];

/// Secret inputs keyed by field path.
pub type SecretInputs = BTreeMap<String, SecretInput>;

/// A parsed body.
#[derive(Debug)]
pub struct SpecInput {
    /// The spec (secret fields not yet applied: they hold whatever the
    /// caller's base had — `None` on create, the stored value on replace).
    pub spec: EndpointSpec,
    /// Requested secret changes.
    pub secrets: SecretInputs,
}

/// Parses raw body bytes as a JSON object.
pub fn json_object(body: &[u8]) -> ApiResult<Map<String, Value>> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiError::bad_request("a JSON object body is required"));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(ApiError::bad_request("the body must be a JSON object")),
        Err(e) => Err(ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "structure",
            format!("malformed JSON: {e}"),
        )),
    }
}

/// The top-level members an `EndpointSpec` accepts.
fn spec_members() -> BTreeSet<String> {
    let sample = EndpointSpec::default_endpoint("default", FhirRelease::R4);
    match serde_json::to_value(sample) {
        Ok(Value::Object(m)) => m.keys().cloned().collect(),
        _ => BTreeSet::new(),
    }
}

fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |cur, seg| cur.get(seg))
}

fn remove_path(v: &mut Value, path: &str) {
    let mut segs: Vec<&str> = path.split('.').collect();
    let Some(last) = segs.pop() else { return };
    let mut cur = v;
    for s in segs {
        match cur.get_mut(s) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Some(o) = cur.as_object_mut() {
        o.remove(last);
    }
}

fn set_path(v: &mut Value, path: &str, value: Value) {
    let mut segs: Vec<&str> = path.split('.').collect();
    let Some(last) = segs.pop() else { return };
    let mut cur = v;
    for s in segs {
        match cur.get_mut(s) {
            Some(next) if next.is_object() => cur = next,
            _ => return,
        }
    }
    if let Some(o) = cur.as_object_mut() {
        o.insert(last.to_string(), value);
    }
}

/// Splits `secrets` out of the body and checks the field paths.
fn take_secrets(obj: &mut Map<String, Value>, issues: &mut Vec<Issue>) -> SecretInputs {
    let mut out = SecretInputs::new();
    let Some(raw) = obj.remove("secrets") else {
        return out;
    };
    let Value::Object(map) = raw else {
        if !raw.is_null() {
            issues.push(Issue::field(
                "invalid",
                "secrets",
                "must be an object of field path → {\"set\": \"…\"} | \"keep\" | \"clear\"",
            ));
        }
        return out;
    };
    for (path, input) in map {
        let field = format!("secrets.{path}");
        if !SECRET_FIELDS.contains(&path.as_str()) {
            issues.push(Issue::field(
                "invalid",
                field,
                format!(
                    "not a secret field (secret fields: {})",
                    SECRET_FIELDS.join(", ")
                ),
            ));
            continue;
        }
        match serde_json::from_value::<SecretInput>(input) {
            Ok(SecretInput::Set(v)) if v.is_empty() => issues.push(Issue::field(
                "invalid",
                field,
                "an empty secret is not allowed; use \"clear\" to remove it",
            )),
            Ok(i) => {
                out.insert(path, i);
            }
            Err(_) => issues.push(Issue::field(
                "invalid",
                field,
                "must be {\"set\": \"<value>\"}, \"keep\" or \"clear\"",
            )),
        }
    }
    out
}

/// Removes the (redacted) secret fields from the spec JSON; anything but
/// absent / `null` / `{"set": bool}` is refused.
fn strip_secret_fields(v: &mut Value, issues: &mut Vec<Issue>) {
    for path in SECRET_FIELDS {
        match get_path(v, path) {
            None | Some(Value::Null) => {}
            Some(Value::Object(o)) if o.len() == 1 && o.get("set").is_some_and(Value::is_boolean) => {}
            Some(_) => issues.push(Issue::field(
                "security",
                *path,
                format!("secrets are write-only: send them as \"secrets\": {{\"{path}\": {{\"set\": \"…\"}}}}"),
            )),
        }
        remove_path(v, path);
    }
}

fn check_name_and_path(obj: &Map<String, Value>, issues: &mut Vec<Issue>) -> (bool, bool) {
    let mut name_ok = false;
    match obj.get("name") {
        Some(Value::String(n)) => match check_segment(n) {
            Ok(()) => name_ok = true,
            Err(e) => issues.push(Issue::field("invalid", "name", format!("name: {e}"))),
        },
        None | Some(Value::Null) => {
            issues.push(Issue::field("required", "name", "name is required"))
        }
        Some(_) => issues.push(Issue::field("invalid", "name", "name must be a string")),
    }
    let mut path_ok = false;
    match obj.get("path") {
        Some(Value::String(p)) if p.is_empty() => path_ok = true,
        Some(Value::String(p)) => match check_segment(p) {
            Ok(()) => path_ok = true,
            Err(e) => issues.push(Issue::field("invalid", "path", format!("path: {e}"))),
        },
        None | Some(Value::Null) => {
            issues.push(Issue::field("required", "path", "path is required"))
        }
        Some(_) => issues.push(Issue::field("invalid", "path", "path must be a string")),
    }
    (name_ok, path_ok)
}

/// Turns a spec JSON object (defaults already filled) into a validated spec.
fn finish(mut obj: Map<String, Value>, mut issues: Vec<Issue>) -> ApiResult<SpecInput> {
    let secrets = take_secrets(&mut obj, &mut issues);
    for f in SERVER_FIELDS {
        obj.remove(*f);
    }
    let members = spec_members();
    for k in obj.keys() {
        if !members.contains(k) {
            issues.push(Issue::field(
                "invalid",
                k.clone(),
                format!("unknown member {k:?}"),
            ));
        }
    }
    let (name_ok, path_ok) = check_name_and_path(&obj, &mut issues);
    let mut v = Value::Object(obj);
    strip_secret_fields(&mut v, &mut issues);
    // Substitute placeholders for a bad name/path so the rest of the spec is
    // still checked and every problem is reported at once.
    let mut probe = v.clone();
    if !name_ok {
        set_path(&mut probe, "name", json!("placeholder-name"));
    }
    if !path_ok {
        let name = probe["name"].clone();
        set_path(&mut probe, "path", name);
    }
    if probe.get("storage").is_none_or(Value::is_null) {
        issues.push(Issue::field("required", "storage", "storage is required"));
        let name = probe["name"].clone();
        set_path(
            &mut probe,
            "storage",
            json!({"kind": "tenant", "tenantId": name}),
        );
    }
    match serde_json::from_value::<EndpointSpec>(probe) {
        Ok(spec) => {
            if let Err(errs) = spec.validate() {
                for f in errs.0 {
                    // Path errors that only exist because of the placeholder.
                    if (f.field == "path" || f.field == "name") && !(name_ok && path_ok) {
                        continue;
                    }
                    issues.push(Issue::field("invalid", f.field, f.message));
                }
            }
            if issues.is_empty() {
                return Ok(SpecInput { spec, secrets });
            }
        }
        Err(e) => issues.push(Issue {
            code: "invalid",
            message: format!("invalid endpoint spec: {e}"),
            field: None,
        }),
    }
    Err(ApiError::unprocessable(issues))
}

/// Body of `POST /admin/endpoints`.
pub fn parse_create(mut obj: Map<String, Value>) -> ApiResult<SpecInput> {
    if let Some(Value::String(name)) = obj.get("name").cloned()
        && EndpointName::parse(&name).is_ok()
    {
        obj.entry("path").or_insert_with(|| json!(name));
        obj.entry("displayName").or_insert_with(|| json!(name));
        obj.entry("storage")
            .or_insert_with(|| json!({"kind": "tenant", "tenantId": name}));
    }
    finish(obj, Vec::new())
}

/// Body of `PUT /admin/endpoints/{name}`: a full replacement. The returned
/// spec's secret fields carry the stored values (so "keep" is the default).
pub fn parse_replace(obj: Map<String, Value>, current: &EndpointSpec) -> ApiResult<SpecInput> {
    let mut input = finish(obj, Vec::new())?;
    carry_secrets(&mut input.spec, current);
    Ok(input)
}

/// Body of `PATCH /admin/endpoints/{name}` (RFC 7396 JSON merge patch over
/// the current spec).
pub fn parse_patch(patch: Map<String, Value>, current: &EndpointSpec) -> ApiResult<SpecInput> {
    let mut base = serde_json::to_value(current)
        .map_err(|e| ApiError::internal(format!("spec does not serialize: {e}")))?;
    // Secrets are carried over below, never through the JSON.
    for p in SECRET_FIELDS {
        remove_path(&mut base, p);
    }
    let mut patch = Value::Object(patch);
    // `secrets` is not part of the document: keep it out of the merge.
    let secrets = patch.as_object_mut().and_then(|o| o.remove("secrets"));
    merge_patch(&mut base, &patch);
    let Value::Object(mut obj) = base else {
        return Err(ApiError::internal("merged spec is not an object"));
    };
    if let Some(s) = secrets {
        obj.insert("secrets".into(), s);
    }
    let mut input = finish(obj, Vec::new())?;
    carry_secrets(&mut input.spec, current);
    Ok(input)
}

/// Copies the stored sealed values into `spec` (for sections that still exist).
fn carry_secrets(spec: &mut EndpointSpec, current: &EndpointSpec) {
    if let (Some(t), Some(cur)) = (spec.terminology.as_mut(), current.terminology.as_ref()) {
        t.token = cur.token.clone();
    }
}

/// RFC 7396 JSON merge patch.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let t = target.as_object_mut().expect("object");
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        other => *target = other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::model::TerminologyBinding;
    use helios_endpoints::{EndpointName, SecretKeyRing};

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn fields(e: &ApiError) -> Vec<String> {
        e.issues.iter().filter_map(|i| i.field.clone()).collect()
    }

    #[test]
    fn create_fills_defaults() {
        let s = parse_create(obj(json!({"name": "lab"}))).unwrap();
        assert_eq!(s.spec.path.as_str(), "lab");
        assert_eq!(s.spec.display_name, "lab");
        assert_eq!(s.spec.storage.tenant_id(), "lab");
        assert!(s.secrets.is_empty());
    }

    #[test]
    fn reserved_and_bad_segments_are_reported_per_field() {
        let e = parse_create(obj(
            json!({"name": "admin", "path": "Bad", "displayName": " "}),
        ))
        .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        let f = fields(&e);
        assert!(f.contains(&"name".to_string()), "{f:?}");
        assert!(f.contains(&"path".to_string()), "{f:?}");
        assert!(f.contains(&"displayName".to_string()), "{f:?}");
        assert!(e.issues.iter().any(|i| i.message.contains("reserved")));
    }

    #[test]
    fn missing_name_and_unknown_members() {
        let e = parse_create(obj(json!({"rateLimits": {}}))).unwrap_err();
        let f = fields(&e);
        assert!(f.contains(&"name".to_string()));
        assert!(f.contains(&"rateLimits".to_string()));
        let e = parse_create(obj(json!({"name": 3, "path": 4}))).unwrap_err();
        assert_eq!(fields(&e), vec!["name", "path", "storage"]);
    }

    #[test]
    fn secret_values_in_the_spec_are_refused() {
        let e = parse_create(obj(json!({
            "name": "lab",
            "terminology": {"serverUrl": "https://tx.example.org", "token": "plain"}
        })))
        .unwrap_err();
        assert_eq!(fields(&e), vec!["terminology.token"]);
        assert_eq!(e.issues[0].code, "security");
        // The redacted form a GET returns is accepted (= keep).
        let s = parse_create(obj(json!({
            "name": "lab",
            "terminology": {"serverUrl": "https://tx.example.org", "token": {"set": true}}
        })))
        .unwrap();
        assert!(s.spec.terminology.unwrap().token.is_none());
    }

    #[test]
    fn secrets_member_is_parsed_and_checked() {
        let s = parse_create(obj(json!({
            "name": "lab",
            "terminology": {"serverUrl": "https://tx.example.org"},
            "secrets": {"terminology.token": {"set": "abc"}}
        })))
        .unwrap();
        assert!(matches!(
            s.secrets["terminology.token"],
            SecretInput::Set(_)
        ));
        let e = parse_create(obj(json!({
            "name": "lab",
            "secrets": {"auth.password": "clear", "terminology.token": {"set": ""}}
        })))
        .unwrap_err();
        assert_eq!(
            fields(&e),
            vec!["secrets.auth.password", "secrets.terminology.token"]
        );
        let e = parse_create(obj(
            json!({"name": "lab", "secrets": {"terminology.token": 5}}),
        ))
        .unwrap_err();
        assert_eq!(fields(&e), vec!["secrets.terminology.token"]);
        let e = parse_create(obj(json!({"name": "lab", "secrets": []}))).unwrap_err();
        assert_eq!(fields(&e), vec!["secrets"]);
    }

    #[test]
    fn replace_ignores_server_members_and_keeps_secrets() {
        let ring = SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap();
        let mut current = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        current.terminology = Some(TerminologyBinding {
            server_url: Some("https://tx.example.org".into()),
            token: Some(ring.seal_str("t", "endpoint:x:terminology.token")),
            ..Default::default()
        });
        let mut body = serde_json::to_value(&current).unwrap();
        helios_endpoints::secret::redact_sealed(&mut body);
        let mut body = body.as_object().unwrap().clone();
        body.insert("id".into(), json!("x"));
        body.insert("revision".into(), json!(3));
        body.insert("displayName".into(), json!("Lab 2"));
        let s = parse_replace(body, &current).unwrap();
        assert_eq!(s.spec.display_name, "Lab 2");
        assert_eq!(
            s.spec.terminology.as_ref().unwrap().token,
            current.terminology.unwrap().token
        );
    }

    #[test]
    fn patch_merges_and_null_removes() {
        let mut current = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        current.labels.insert("env".into(), "test".into());
        let s = parse_patch(
            obj(
                json!({"displayName": "Lab X", "labels": {"env": null, "team": "a"},
                       "cors": {"enabled": true, "allowedOrigins": ["https://a.example"]}}),
            ),
            &current,
        )
        .unwrap();
        assert_eq!(s.spec.display_name, "Lab X");
        assert_eq!(s.spec.labels.len(), 1);
        assert_eq!(s.spec.labels["team"], "a");
        assert_eq!(
            s.spec.cors.unwrap().allowed_origins,
            vec!["https://a.example"]
        );
        let e =
            parse_patch(obj(json!({"cors": {"allowedOrigins": ["nope"]}})), &current).unwrap_err();
        assert_eq!(fields(&e), vec!["cors.allowedOrigins"]);
    }

    #[test]
    fn merge_patch_rfc7396_examples() {
        let mut t = json!({"a": "b", "c": {"d": "e", "f": "g"}});
        merge_patch(&mut t, &json!({"a": "z", "c": {"f": null}}));
        assert_eq!(t, json!({"a": "z", "c": {"d": "e"}}));
        let mut t = json!({"a": [1]});
        merge_patch(&mut t, &json!({"a": {"b": 1}}));
        assert_eq!(t, json!({"a": {"b": 1}}));
        let mut t = json!("x");
        merge_patch(&mut t, &json!({"a": 1}));
        assert_eq!(t, json!({"a": 1}));
    }

    #[test]
    fn json_object_errors() {
        assert_eq!(
            json_object(b"").unwrap_err().status,
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            json_object(b"[]").unwrap_err().status,
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(json_object(b"{").unwrap_err().issues[0].code, "structure");
        assert!(json_object(b"{}").unwrap().is_empty());
    }

    #[test]
    fn missing_storage_and_type_errors() {
        // Replace bodies get no defaults.
        let current = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        let e = parse_replace(
            obj(json!({"name": "lab", "path": "lab", "displayName": "x"})),
            &current,
        )
        .unwrap_err();
        assert_eq!(fields(&e), vec!["storage"]);
        let e = parse_replace(
            obj(json!({"name": "lab", "path": "lab", "displayName": "x",
                       "storage": {"kind": "tenant", "tenantId": "lab"}, "fhirVersion": "R9"})),
            &current,
        )
        .unwrap_err();
        assert!(e.issues[0].message.contains("invalid endpoint spec"));
    }
}
