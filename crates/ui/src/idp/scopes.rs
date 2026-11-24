//! The SMART scope builder of an app (add-app form on the Clients page and
//! in wizard step 4, the app page): presets, the sign-in / launch scopes as
//! toggles, a searchable grouped list of every resource type the realm's
//! endpoints serve with c/r/u/d/s per context, live analysis, and a raw
//! text view kept in sync (`scopes_text`, one scope per line — the only
//! field the form posts; without JavaScript it is the editor).
//!
//! | route | |
//! |---|---|
//! | `GET /ui/idp/{realm}/scopes/check?scopes=&kind=&launch=&accept_v1=` | analysis as JSON (localised) |
//!
//! The stored format is unchanged: `ClientSpec.scopes.allowed` strings.

use std::collections::BTreeMap;

use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use helios_idp::client::{ClientAuth, ClientSpec, LaunchType};
use helios_idp::realm::RealmFeatures;
use helios_idp::scope::{Scope, ScopeContext, parse_scope, split_scopes};
use helios_idp::scope_catalog::{self, Analysis, ClientShape, Level, NON_RESOURCE, PRESETS};
use helios_idp::store::RealmRecord;
use helios_idp_admin::scopes::Catalog;
use helios_idp_admin::service::IdpConsole;
use serde::Deserialize;
use serde_json::{Value, json};

use super::console;
use crate::i18n::{I18n, RequestLocale};

/// A preset button.
#[derive(Debug, Clone)]
pub(crate) struct PresetView {
    pub id: &'static str,
    pub label: String,
    pub desc: String,
    pub scopes: String,
    pub backend: bool,
}

/// A sign-in / launch toggle.
#[derive(Debug, Clone)]
pub(crate) struct ToggleView {
    pub scope: &'static str,
    pub label: String,
    pub hint: String,
    pub checked: bool,
    /// The realm does not offer it.
    pub off: bool,
}

/// One finding as the builder shows it.
#[derive(Debug, Clone)]
pub(crate) struct FindingView {
    pub warn: bool,
    pub message: String,
}

/// Everything `partials/idp/scope-builder.html` needs.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeBuilder {
    /// One scope per line (the `scopes_text` field).
    pub text: String,
    pub accept_v1: bool,
    /// The catalog for the script (groups with labels, contexts, perms).
    pub catalog_json: String,
    pub check_url: String,
    /// `public|secret|key|backend` for a saved app; empty on the add form
    /// (the script reads the kind radios).
    pub kind: String,
    pub presets: Vec<PresetView>,
    pub toggles: Vec<ToggleView>,
    /// "Resource types of sandbox (R4)…" line.
    pub source_note: String,
    pub findings: Vec<FindingView>,
    pub error: String,
    pub users_href: String,
}

/// Kind of a saved client, as the add form names it.
pub(crate) fn client_kind(c: &ClientSpec) -> &'static str {
    if c.launch_types.contains(&LaunchType::Backend) {
        return "backend";
    }
    match c.auth {
        ClientAuth::None => "public",
        ClientAuth::ClientSecret { .. } | ClientAuth::TlsClientAuth { .. } => "secret",
        ClientAuth::PrivateKeyJwt { .. } => "key",
    }
}

/// The client the add form would create (see `wizard::client_from_form`).
pub(crate) fn shape_for(kind: &str, launch: &[String], accept_v1: bool) -> ClientShape {
    let backend = kind == "backend";
    ClientShape {
        public: !matches!(kind, "secret" | "key" | "backend"),
        authorization_code: !backend,
        client_credentials: backend,
        standalone: !backend && launch.iter().any(|l| l == "standalone"),
        ehr: !backend && launch.iter().any(|l| l == "ehr"),
        accept_v1,
    }
}

/// Default scopes of a new app: the patient-facing preset, minus what the
/// realm does not offer.
pub(crate) fn default_scopes(f: &RealmFeatures) -> Vec<String> {
    scope_catalog::preset("patient-read")
        .map(|p| p.scopes)
        .unwrap_or_default()
        .iter()
        .filter(|s| scope_catalog::realm_offers(f, s, false))
        .map(|s| s.to_string())
        .collect()
}

/// Scopes posted by the form: `scopes_text` (builder / raw view; spaces,
/// commas or new lines) when present, else the older checkbox + "More
/// scopes" fields. Order kept, duplicates dropped.
pub(crate) fn scopes_from_form(body: &[u8]) -> Vec<String> {
    let text: Vec<String> = super::values(body, "scopes_text");
    let raw: Vec<String> = if form_has(body, "scopes_text") {
        text
    } else {
        let mut v = super::values(body, "scope");
        v.extend(super::values(body, "scopes_custom"));
        v
    };
    split_scopes(&raw.join(" ").replace([',', '\n', '\r', '\t'], " "))
}

/// Whether the form carried `name` at all (an empty textarea counts).
pub(crate) fn form_has(body: &[u8], name: &str) -> bool {
    form_urlencoded::parse(body).any(|(k, _)| k == name)
}

/// Fluent key suffix of a non-resource scope (`launch/patient` →
/// `launch-patient`, `offline_access` → `offline-access`).
fn nr_key(s: &str) -> String {
    s.replace(['/', '_'], "-").to_ascii_lowercase()
}

fn feature_label(i18n: &I18n, feature: &str) -> String {
    i18n.t(&format!("idps-feature-{feature}"))
}

/// Localised analysis.
pub(crate) fn localise(i18n: &I18n, a: &Analysis) -> Value {
    let invalid: Vec<Value> = a
        .invalid
        .iter()
        .map(|(s, e)| {
            json!({
                "scope": s,
                "message": i18n.t_arg2(
                    "idps-invalid", "scope", s.clone(), "reason",
                    i18n.t(&format!("idps-syntax-{}", e.code())),
                ),
            })
        })
        .collect();
    let redundant: Vec<Value> = a
        .redundant
        .iter()
        .map(|r| {
            json!({
                "scope": r.scope,
                "coveredBy": r.covered_by,
                "message": i18n.t_arg2("idps-redundant", "scope", r.scope.clone(), "by", r.covered_by.clone()),
            })
        })
        .collect();
    let findings: Vec<Value> = a
        .findings
        .iter()
        .map(|f| {
            let mut args = BTreeMap::new();
            args.insert("scopes".to_string(), f.scopes.join(" "));
            let detail = if f.code == "realm-off" {
                feature_label(i18n, &f.detail)
            } else {
                f.detail.clone()
            };
            args.insert("detail".to_string(), detail);
            json!({
                "code": f.code,
                "level": if f.level == Level::Warn { "warn" } else { "info" },
                "scopes": f.scopes,
                "message": i18n.t_args(&format!("idps-f-{}", f.code), &args),
            })
        })
        .collect();
    json!({"invalid": invalid, "redundant": redundant, "findings": findings})
}

fn findings_view(i18n: &I18n, a: &Analysis) -> Vec<FindingView> {
    let v = localise(i18n, a);
    let mut out = vec![];
    for (k, warn) in [("invalid", true), ("findings", false), ("redundant", false)] {
        for f in v[k].as_array().into_iter().flatten() {
            out.push(FindingView {
                warn: warn || f["level"] == "warn",
                message: f["message"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    out
}

/// The catalog as the script reads it.
fn catalog_json(i18n: &I18n, cat: &Catalog) -> String {
    let groups: Vec<Value> = cat
        .groups
        .iter()
        .map(|g| json!({"id": g.id, "label": i18n.t(&format!("idps-group-{}", g.id)), "types": g.types}))
        .collect();
    let contexts: Vec<Value> = cat
        .contexts
        .iter()
        .map(|c| json!({"id": c, "label": format!("{c}/"), "name": i18n.t(&format!("idps-ctx-{c}"))}))
        .collect();
    let perms: Vec<Value> = cat
        .permissions
        .iter()
        .map(|p| json!({"l": p, "label": i18n.t(&format!("idps-perm-{p}"))}))
        .collect();
    json!({"groups": groups, "contexts": contexts, "perms": perms})
        .to_string()
        // Safe inside <script type="application/json">.
        .replace("</", "<\\/")
}

fn source_note(i18n: &I18n, cat: &Catalog, total: usize) -> String {
    use helios_idp_admin::scopes::TypeSource;
    let eps: Vec<String> = cat
        .endpoints
        .iter()
        .filter(|e| match cat.source {
            TypeSource::Bound => e.bound,
            _ => true,
        })
        .map(|e| {
            if e.fhir_version.is_empty() {
                e.name.clone()
            } else {
                format!("{} ({})", e.name, e.fhir_version)
            }
        })
        .collect();
    let key = match cat.source {
        TypeSource::Bound => "idps-source-bound",
        TypeSource::Chosen => "idps-source-chosen",
        TypeSource::All | TypeSource::Server => "idps-source-all",
    };
    if eps.is_empty() {
        return i18n.t_arg("idps-source-none", "count", total as i64);
    }
    i18n.t_arg2(key, "endpoints", eps.join(", "), "count", total as i64)
}

/// Builds the builder for `scopes` of a client of `shape` (`kind` empty:
/// read from the add form's radios).
pub(crate) async fn builder(
    c: &IdpConsole,
    rec: &RealmRecord,
    i18n: &I18n,
    scopes: &[String],
    accept_v1: bool,
    kind: &str,
    shape: &ClientShape,
    error: String,
) -> ScopeBuilder {
    let realm = rec.spec.name.as_str();
    let f = &rec.spec.features;
    let cat = c.scope_catalog(Some(realm)).await.ok();
    let total = cat
        .as_ref()
        .map(|c| c.groups.iter().map(|g| g.types.len()).sum())
        .unwrap_or(0);
    let analysis = c
        .analyze_scopes(realm, scopes, shape)
        .await
        .unwrap_or_default();
    ScopeBuilder {
        text: scopes.join("\n"),
        accept_v1,
        catalog_json: cat
            .as_ref()
            .map(|c| catalog_json(i18n, c))
            .unwrap_or_else(|| "{}".into()),
        check_url: format!("/ui/idp/{realm}/scopes/check"),
        kind: kind.to_string(),
        presets: PRESETS
            .iter()
            .map(|p| PresetView {
                id: p.id,
                label: i18n.t(&format!("idps-preset-{}", p.id)),
                desc: i18n.t(&format!("idps-preset-{}-desc", p.id)),
                scopes: p.scopes.join(" "),
                backend: p.backend,
            })
            .collect(),
        toggles: NON_RESOURCE
            .iter()
            .map(|s| ToggleView {
                scope: s,
                label: i18n.t(&format!("idps-nr-{}", nr_key(s))),
                hint: i18n.t(&format!("idps-nr-{}-hint", nr_key(s))),
                checked: scopes.iter().any(|x| x == s),
                off: !scope_catalog::realm_offers(f, s, false),
            })
            .collect(),
        source_note: cat
            .as_ref()
            .map(|c| source_note(i18n, c, total))
            .unwrap_or_default(),
        findings: findings_view(i18n, &analysis),
        error,
        users_href: format!("/ui/idp/{realm}/users"),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CheckQuery {
    #[serde(default)]
    scopes: String,
    #[serde(default)]
    kind: String,
    /// `standalone ehr` (space separated).
    #[serde(default)]
    launch: String,
    #[serde(default)]
    accept_v1: String,
}

/// `GET /ui/idp/{realm}/scopes/check` — the live analysis.
pub(crate) async fn check(
    locale: RequestLocale,
    Path(realm): Path<String>,
    Query(q): Query<CheckQuery>,
) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let scopes = split_scopes(&q.scopes.replace([',', '\n', '\r', '\t'], " "));
    let launch: Vec<String> = q.launch.split_whitespace().map(String::from).collect();
    let shape = shape_for(
        &q.kind,
        &launch,
        matches!(q.accept_v1.as_str(), "1" | "on" | "true"),
    );
    match c.analyze_scopes(&realm, &scopes, &shape).await {
        Ok(a) => {
            let i18n = I18n::new(locale);
            axum::Json(localise(&i18n, &a)).into_response()
        }
        Err(e) => (super::op_status(&e), e.message).into_response(),
    }
}

// ---------------------------------------------------------- detail chips

/// A chip of the app page's "Effective access" summary.
#[derive(Debug, Clone)]
pub(crate) struct Chip {
    pub label: String,
    pub scope: String,
    /// The realm does not offer it (shown struck through).
    pub off: bool,
}

/// A group of chips (sign-in & launch, patient/, user/, system/, other).
#[derive(Debug, Clone)]
pub(crate) struct ChipGroup {
    pub title: String,
    pub hint: String,
    pub chips: Vec<Chip>,
}

fn perms_label(i18n: &I18n, letters: &str) -> String {
    match letters {
        "rs" => i18n.t("idps-perms-read-only"),
        "cruds" => i18n.t("idps-perms-full"),
        _ => letters
            .chars()
            .map(|c| i18n.t(&format!("idps-perm-{c}")))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The saved scopes as readable chips grouped by context.
pub(crate) fn effective_chips(i18n: &I18n, spec: &ClientSpec, f: &RealmFeatures) -> Vec<ChipGroup> {
    let cc = spec
        .grant_types
        .contains(&helios_idp::client::GrantType::ClientCredentials);
    let mut groups: Vec<(&str, Vec<Chip>)> = ["signin", "patient", "user", "system", "other"]
        .iter()
        .map(|g| (*g, vec![]))
        .collect();
    for s in &spec.scopes.allowed {
        let off = !scope_catalog::realm_offers(f, s, cc);
        let (g, label) = match parse_scope(s) {
            Scope::Resource(r) => {
                let what = if r.resource_type == "*" {
                    i18n.t("idps-all-types")
                } else {
                    r.resource_type.clone()
                };
                let mut label = format!("{what} · {}", perms_label(i18n, &r.perms.letters()));
                if let Some(q) = &r.query {
                    label.push_str(&format!(" · {q}"));
                }
                let g = match r.context {
                    ScopeContext::Patient => "patient",
                    ScopeContext::User => "user",
                    ScopeContext::System => "system",
                };
                (g, label)
            }
            Scope::Other(_) => ("other", s.clone()),
            _ => {
                let label = i18n.t(&format!("idps-nr-{}", nr_key(s)));
                let label = if label.starts_with("idps-") {
                    s.clone()
                } else {
                    label
                };
                ("signin", label)
            }
        };
        if let Some((_, v)) = groups.iter_mut().find(|(x, _)| *x == g) {
            v.push(Chip {
                label,
                scope: s.clone(),
                off,
            });
        }
    }
    groups
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(g, chips)| ChipGroup {
            title: i18n.t(&format!("idps-chips-{g}")),
            hint: i18n.t(&format!("idps-chips-{g}-hint")),
            chips,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_scopes_prefer_the_text_field() {
        let body = b"scope=openid&scopes_text=patient%2F*.rs%0Aopenid%0A%0Alaunch%2Fpatient+offline_access%2Copenid";
        assert_eq!(
            scopes_from_form(body),
            vec!["patient/*.rs", "openid", "launch/patient", "offline_access"]
        );
        // An emptied text field means "no scopes", not "fall back".
        assert!(scopes_from_form(b"scope=openid&scopes_text=").is_empty());
        // Older forms (checkboxes + More scopes) still work.
        assert_eq!(
            scopes_from_form(
                b"scope=openid&scope=patient%2F*.rs&scopes_custom=user%2FObservation.rs+x"
            ),
            vec!["openid", "patient/*.rs", "user/Observation.rs", "x"]
        );
    }

    #[test]
    fn shapes() {
        let s = shape_for("public", &["standalone".into()], false);
        assert!(s.public && s.standalone && s.authorization_code && !s.client_credentials);
        let b = shape_for("backend", &["standalone".into()], true);
        assert!(!b.public && b.client_credentials && !b.authorization_code && !b.standalone);
        assert!(b.accept_v1);
        assert!(!shape_for("key", &[], false).public);
    }

    #[test]
    fn defaults_follow_the_realm() {
        let mut f = RealmFeatures::default();
        assert_eq!(
            default_scopes(&f),
            vec![
                "openid",
                "fhirUser",
                "launch/patient",
                "offline_access",
                "patient/*.rs"
            ]
        );
        f.offline_access = false;
        assert!(!default_scopes(&f).contains(&"offline_access".to_string()));
    }
}
