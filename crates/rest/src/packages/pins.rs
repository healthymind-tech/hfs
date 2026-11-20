//! Binding version pins: which version of a value set an installed profile
//! means.
//!
//! A StructureDefinition binds `…/ValueSet/x` without a version. Its IG means
//! the version in its own package or in the packages it depends on — the IG
//! publisher and the HL7 validator resolve canonicals in that dependency
//! context. The terminology server may hold several versions of the same
//! value set (IPS 1.1.0 from an installed IG next to IPS 2.0.0 from a bundled
//! package) and answers an unversioned request from its newest, so the
//! validator sends `url|version` when the profile's package context names
//! one (see [`helios_fhir_validator::schema::pin_binding_versions`]).
//!
//! Pins are computed from the package registry: for every installed package,
//! the value sets it wrote itself, then those of its resolved dependencies
//! (breadth first, nearest wins). Every StructureDefinition the package wrote
//! is keyed by `url|version`. A value set no package in the context carries
//! is not pinned (the binding stays unversioned, as before). The same pins
//! carry the versions of the StructureDefinitions in the context, which fix
//! the version of a profile's (unversioned) base definition.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use helios_persistence::core::{PackageRecord, PackageStore, TARGET_HFS};
use serde_json::Value;

/// Value set (or StructureDefinition) url → version.
pub type Pins = HashMap<String, String>;

/// `StructureDefinition url|version` (or `url` when it has no version) →
/// the pins of the package that wrote it.
pub type ProfilePins = HashMap<String, Arc<Pins>>;

/// The key a profile's pins are filed under.
pub fn profile_key(url: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("{url}|{v}"),
        None => url.to_string(),
    }
}

/// Compute the pins of every installed package of `tenant`.
pub async fn compute(store: &dyn PackageStore, tenant: &str) -> Result<ProfilePins, String> {
    let packages = store
        .list_packages(tenant)
        .await
        .map_err(|e| e.to_string())?;
    let rows = store
        .package_resources(tenant, None)
        .await
        .map_err(|e| e.to_string())?;
    let mut value_sets: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut profiles: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    for row in rows {
        let key = format!("{}#{}", row.package_name, row.package_version);
        let r = row.resource;
        let Some(url) = r.url else { continue };
        match r.resource_type.as_str() {
            "ValueSet" => {
                if let Some(v) = r.version {
                    value_sets.entry(key).or_default().push((url, v));
                }
            }
            "StructureDefinition" if r.target == TARGET_HFS => {
                // A profile's base is pinned like a binding: EMR 0.2.0
                // builds on TW Core 0.3.2 while TW Core 1.0.0 is installed
                // next to it, and the HL7 validator layers the 0.3.2
                // definition (its snapshot was generated from it).
                if let Some(v) = &r.version {
                    value_sets
                        .entry(key.clone())
                        .or_default()
                        .push((url.clone(), v.clone()));
                }
                profiles.entry(key).or_default().push((url, r.version));
            }
            _ => {}
        }
    }
    Ok(assemble(&packages, &value_sets, &profiles))
}

/// [`compute`] over already-read rows (pure; tested directly).
pub fn assemble(
    packages: &[PackageRecord],
    value_sets: &HashMap<String, Vec<(String, String)>>,
    profiles: &HashMap<String, Vec<(String, Option<String>)>>,
) -> ProfilePins {
    let installed: HashSet<String> = packages
        .iter()
        .map(|p| format!("{}#{}", p.name, p.version))
        .collect();
    let deps: HashMap<String, Vec<String>> = packages
        .iter()
        .map(|p| {
            let declared = p.document.get("dependencies").and_then(Value::as_object);
            let resolved = p
                .document
                .get("resolved")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(name, v)| {
                            // The version the package declares, when it is
                            // installed, is the one the IG was built on —
                            // even if the install resolved the dependency
                            // elsewhere (older installers satisfied DTR's
                            // SDC 3.0.0 with 4.0.0) or not at all (mCODE's
                            // genomics-reporting 2.0.0, installed later).
                            if let Some(want) =
                                declared.and_then(|d| d.get(name)).and_then(Value::as_str)
                            {
                                let key = format!("{name}#{want}");
                                if installed.contains(&key) {
                                    return Some(key);
                                }
                            }
                            let v = v.as_str()?;
                            v.contains('#').then(|| v.to_string())
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (format!("{}#{}", p.name, p.version), resolved)
        })
        .collect();
    let mut out = ProfilePins::new();
    for (package, sds) in profiles {
        let mut pins = Pins::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::from([package.clone()]);
        while let Some(p) = queue.pop_front() {
            if !seen.insert(p.clone()) {
                continue;
            }
            // Within one package, the highest version of a url (a package
            // rarely ships two).
            let mut own: Vec<&(String, String)> = value_sets
                .get(&p)
                .map(|v| v.iter().collect())
                .unwrap_or_default();
            own.sort_by(|a, b| crate::packages::version::compare(&b.1, &a.1));
            for (url, version) in own {
                pins.entry(url.clone()).or_insert_with(|| version.clone());
            }
            for d in deps.get(&p).into_iter().flatten() {
                queue.push_back(d.clone());
            }
        }
        if pins.is_empty() {
            continue;
        }
        let pins = Arc::new(pins);
        for (url, version) in sds {
            out.insert(profile_key(url, version.as_deref()), pins.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(name: &str, version: &str, resolved: Value) -> PackageRecord {
        PackageRecord {
            name: name.into(),
            version: version.into(),
            status: "installed".into(),
            installed_at: chrono::Utc::now(),
            document: json!({ "resolved": resolved }),
        }
    }

    #[test]
    fn profiles_pin_their_own_then_their_dependencies_value_sets() {
        let packages = vec![
            package(
                "hl7.fhir.uv.ips",
                "1.1.0",
                json!({"hl7.fhir.r4.core": "embedded"}),
            ),
            package(
                "tw.gov.mohw.twcore",
                "1.0.0",
                json!({"hl7.fhir.uv.ips": "hl7.fhir.uv.ips#1.1.0", "x": "missing"}),
            ),
        ];
        let smoking = "http://hl7.org/fhir/uv/ips/ValueSet/current-smoking-status-uv-ips";
        let mut value_sets = HashMap::new();
        value_sets.insert(
            "hl7.fhir.uv.ips#1.1.0".to_string(),
            vec![(smoking.to_string(), "1.1.0".to_string())],
        );
        value_sets.insert(
            "tw.gov.mohw.twcore#1.0.0".to_string(),
            vec![("https://twcore/ValueSet/a".to_string(), "1.0.0".to_string())],
        );
        let mut profiles = HashMap::new();
        profiles.insert(
            "hl7.fhir.uv.ips#1.1.0".to_string(),
            vec![(
                "http://ips/Observation-tobacco".to_string(),
                Some("1.1.0".to_string()),
            )],
        );
        profiles.insert(
            "tw.gov.mohw.twcore#1.0.0".to_string(),
            vec![(
                "https://twcore/Patient".to_string(),
                Some("1.0.0".to_string()),
            )],
        );
        let pins = assemble(&packages, &value_sets, &profiles);
        let ips = &pins["http://ips/Observation-tobacco|1.1.0"];
        assert_eq!(ips.get(smoking).map(String::as_str), Some("1.1.0"));
        assert!(!ips.contains_key("https://twcore/ValueSet/a"));
        let tw = &pins["https://twcore/Patient|1.0.0"];
        assert_eq!(
            tw.get(smoking).map(String::as_str),
            Some("1.1.0"),
            "via the dependency"
        );
        assert_eq!(
            tw.get("https://twcore/ValueSet/a").map(String::as_str),
            Some("1.0.0")
        );
    }

    #[test]
    fn profiles_pin_their_base_definitions_also_through_a_declared_dependency() {
        let mut mcode = package(
            "hl7.fhir.us.mcode",
            "3.0.0",
            json!({"hl7.fhir.uv.genomics-reporting": "missing", "hl7.fhir.r4.core": "embedded"}),
        );
        mcode.document["dependencies"] = json!({"hl7.fhir.uv.genomics-reporting": "2.0.0"});
        let packages = vec![
            mcode,
            package("hl7.fhir.uv.genomics-reporting", "2.0.0", json!({})),
            package("hl7.fhir.uv.genomics-reporting", "3.0.0", json!({})),
        ];
        let variant = "http://hl7.org/fhir/uv/genomics-reporting/StructureDefinition/variant";
        let mut canonicals = HashMap::new();
        for v in ["2.0.0", "3.0.0"] {
            canonicals.insert(
                format!("hl7.fhir.uv.genomics-reporting#{v}"),
                vec![(variant.to_string(), v.to_string())],
            );
        }
        let mut profiles = HashMap::new();
        profiles.insert(
            "hl7.fhir.us.mcode#3.0.0".to_string(),
            vec![(
                "http://mcode/genomic-variant".to_string(),
                Some("3.0.0".to_string()),
            )],
        );
        let pins = assemble(&packages, &canonicals, &profiles);
        assert_eq!(
            pins["http://mcode/genomic-variant|3.0.0"]
                .get(variant)
                .map(String::as_str),
            Some("2.0.0"),
            "the declared 2.0.0 is installed, so mCODE means it"
        );
    }
}
