//! The FHIR Schema data model.
//!
//! FHIR Schema is a JSON-Schema-like, differential-by-design representation of
//! FHIR StructureDefinitions (see <https://fhir-schema.github.io/fhir-schema/>).
//! One struct serves both the schema level and the element level — the two are
//! the same shape in the format (confirmed by the upstream conformance suite).
//!
//! Everything is optional: a schema on disk is a differential fragment, and
//! the "complete picture" only exists transiently as a cooperative schema set
//! during validation (see [`crate::engine`]).
//!
//! Deserialization is tolerant: unknown keys are ignored (mirroring the
//! reference validator, which logs and skips unrecognized keywords).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Schema `kind` values with meaning to the validation engine.
///
/// Kept as plain string constants (rather than an enum) because fixtures and
/// converted packs carry open-ended kinds (`resource`, `complex-type`,
/// `logical`, `extension`, ...) and only `primitive-type` alters behavior:
/// it stops complex-`type` expansion during schema-set assembly.
pub mod kind {
    pub const PRIMITIVE_TYPE: &str = "primitive-type";
    pub const RESOURCE: &str = "resource";
    pub const COMPLEX_TYPE: &str = "complex-type";
    pub const EXTENSION: &str = "extension";
}

/// A FHIR Schema — used both as a top-level (named) schema and as an element
/// schema nested under [`FhirSchema::elements`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FhirSchema {
    // ------------------------------------------------------------------
    // Identity (top-level schemas)
    // ------------------------------------------------------------------
    /// Canonical URL this schema is published under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Business version of the definition (`StructureDefinition.version`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Machine-readable name (also usable as a resolution alias).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Schema this one layers onto; resolved into the same cooperative set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Schema kind — see [`kind`]. Only `primitive-type` alters assembly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `specialization` (custom resources) or `constraint` (profiles).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derivation: Option<String>,
    /// The type this schema (or element) constrains. On an element, naming a
    /// complex type pulls that type's schema into the set; the special value
    /// `Resource` resolves dynamically from the data's `resourceType`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,

    // ------------------------------------------------------------------
    // Shape
    // ------------------------------------------------------------------
    /// Element is a collection: only JSON arrays are accepted. Array-ness is
    /// decided cooperatively — true if *any* layer says `array: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array: Option<bool>,
    /// Element is scalar: JSON arrays are rejected. Mutually exclusive with
    /// `array` within one schema (layers may still disagree; array wins).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scalar: Option<bool>,
    /// Minimum number of items (arrays only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    /// Maximum number of items (arrays only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,

    // ------------------------------------------------------------------
    // Structure
    // ------------------------------------------------------------------
    /// Nested element schemas, keyed by element name. Order is preserved —
    /// deterministic error emission is part of the conformance contract.
    /// Values are `Arc` so cooperative sets can share them without cloning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elements: Option<IndexMap<String, Arc<FhirSchema>>>,
    /// Child keys that must be present on the node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    /// Child keys that must be absent from the node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded: Option<Vec<String>>,
    /// Reference to another element's schema by path, for recursive
    /// structures (e.g. `Questionnaire.item`):
    /// `["Questionnaire", "elements", "item"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element_reference: Option<Vec<String>>,

    // ------------------------------------------------------------------
    // Choice types (value[x])
    // ------------------------------------------------------------------
    /// On a choice-group declarer: the allowed concrete branch names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    /// On a concrete branch: the name of the choice group it belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice_of: Option<String>,

    // ------------------------------------------------------------------
    // Value constraints
    // ------------------------------------------------------------------
    /// Value must be deeply equal to this constant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<Value>,
    /// Value must partially match this constant (pattern keys present and
    /// equal; extra data keys permitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<Value>,
    /// `minValue[x]` of the element definition: the value (a number, a
    /// date/time string, or a Quantity) the data must not be below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_value: Option<Value>,
    /// `maxValue[x]`: the value the data must not exceed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_value: Option<Value>,
    /// `ElementDefinition.maxLength`: most characters a string value may
    /// have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    /// The `minLength` extension on the element definition: fewest
    /// characters a string value may have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    /// The `maxSize` extension on the element definition: most bytes an
    /// Attachment's data may hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
    /// Terminology binding. Only `required`-strength bindings are enforced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
    /// FHIRPath invariants, keyed by constraint id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraints: Option<IndexMap<String, Constraint>>,
    /// Allowed reference target types (for Reference/canonical elements).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refers: Option<Vec<String>>,
    /// `type.profile` of the element (datatype or resource profiles): the
    /// value must conform to one of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_profiles: Option<Vec<String>>,

    // ------------------------------------------------------------------
    // Slicing
    // ------------------------------------------------------------------
    /// Array slicing definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slicing: Option<Slicing>,
    /// Extension sugar: named extensions keyed by slice name, compiled into
    /// `slicing` on the `extension` element (matched by `{url}`) at
    /// validation time. Entry values reuse [`FhirSchema`] (they carry `url`,
    /// `min`, `max`, plus any extra element constraints).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<IndexMap<String, Arc<FhirSchema>>>,

    // ------------------------------------------------------------------
    // Informational (carried, never enforced)
    // ------------------------------------------------------------------
    /// Mirrors `ElementDefinition.isModifier`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modifier: Option<bool>,
    /// Mirrors `ElementDefinition.mustSupport`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub must_support: Option<bool>,
    /// Mirrors `ElementDefinition.isSummary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    /// For extension definitions: the element expressions the extension
    /// applies to (`StructureDefinition.context[].expression` where the type
    /// is `element`). Powers the editor's profiled-extension offers (#363).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    /// Mirrors `ElementDefinition.short` — the human label consumers render
    /// instead of the raw element name. The longer `definition` text is
    /// deliberately not carried (pack size); `short` is the label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,

    // ------------------------------------------------------------------
    // Helios extension
    // ------------------------------------------------------------------
    /// Primitive value regex, carried from the FHIR spec's primitive type
    /// StructureDefinitions by our converter. Not part of upstream FHIR
    /// Schema; absent from the upstream conformance fixtures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

impl FhirSchema {
    /// Whether this schema is a primitive-type leaf: registered in the set
    /// but never expanded as a complex type.
    pub fn is_primitive(&self) -> bool {
        self.kind.as_deref() == Some(kind::PRIMITIVE_TYPE)
    }

    /// Return a clone with all cardinality fields cleared.
    ///
    /// Used when merging an `elementReference` target into a schema set: the
    /// target contributes structure (elements, type, constraints) but never
    /// its own array/min/max, which belong to the referring element.
    pub fn without_cardinality(&self) -> Self {
        Self {
            array: None,
            scalar: None,
            min: None,
            max: None,
            ..self.clone()
        }
    }
}

/// Terminology binding: the ValueSet a coded value must belong to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    /// Canonical URL of the bound ValueSet.
    pub value_set: String,
    /// FHIR binding strength (`required`, `extensible`, `preferred`,
    /// `example`). Only `required` is validated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<String>,
}

/// A FHIRPath invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    /// FHIRPath expression that must evaluate truthy on the node.
    pub expression: String,
    /// `error` | `warning` | `guideline`. Only `error` fails validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Human-readable description, used in the emitted message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human: Option<String>,
}

/// Array slicing: partitions a repeating element into named slices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slicing {
    /// Named slices. Order preserved for deterministic sweep/error order.
    pub slices: IndexMap<String, Slice>,
    /// `open` (default) | `closed` | `openAtEnd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    /// When true, matched items must appear in slice `order`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordered: Option<bool>,
}

/// One slice of a sliced array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slice {
    /// How items are matched into this slice. Optional: constraining slices
    /// (`sliceIsConstraining`) reference a parent slice instead.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_: Option<Match>,
    /// Minimum number of matched items across the whole array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    /// Maximum number of matched items across the whole array.
    /// `Some(0)` means the slice is prohibited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
    /// Position for `ordered` slicing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<u64>,
    /// Name of the parent slice being re-sliced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reslice: Option<String>,
    /// Further constrains a slice of the same name from a parent schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice_is_constraining: Option<bool>,
    /// Element schema applied cooperatively to items matched into the slice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Arc<FhirSchema>>,
}

/// A slice match rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Match {
    /// `pattern` | `binding` | `profile` | `type` (the reference validator
    /// only implements `pattern`; we start there too).
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    /// The pattern / binding / profile / type payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Match against the resolved reference target instead of the reference
    /// element itself.
    #[serde(rename = "resolve-ref", skip_serializing_if = "Option::is_none")]
    pub resolve_ref: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_primitive_schema() {
        let s: FhirSchema = serde_json::from_str(r#"{ "kind": "primitive-type" }"#).unwrap();
        assert!(s.is_primitive());
        assert!(s.elements.is_none());
    }

    #[test]
    fn parses_elements_preserving_order() {
        let s: FhirSchema = serde_json::from_str(
            r#"{ "elements": { "z": {"type": "string"}, "a": {"type": "string"} } }"#,
        )
        .unwrap();
        let keys: Vec<&str> = s
            .elements
            .as_ref()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["z", "a"],
            "element declaration order must be preserved"
        );
    }

    #[test]
    fn tolerates_unknown_keys() {
        let s: FhirSchema =
            serde_json::from_str(r#"{ "type": "string", "somethingNew": 42 }"#).unwrap();
        assert_eq!(s.type_.as_deref(), Some("string"));
    }

    #[test]
    fn parses_slicing_shape() {
        let s: FhirSchema = serde_json::from_str(
            r#"{
              "slicing": {
                "slices": {
                  "home": {
                    "match": { "type": "pattern", "value": { "use": "home" } },
                    "min": 1, "max": 1,
                    "schema": { "required": ["city"] }
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let slicing = s.slicing.unwrap();
        let home = &slicing.slices["home"];
        assert_eq!(home.min, Some(1));
        let m = home.match_.as_ref().unwrap();
        assert_eq!(m.type_.as_deref(), Some("pattern"));
        assert_eq!(m.value.as_ref().unwrap()["use"], "home");
        assert_eq!(
            home.schema.as_ref().unwrap().required.as_ref().unwrap(),
            &vec!["city".to_string()]
        );
    }
}

/// Pin unversioned binding value sets to the versions in `pins`
/// (value set url → version).
///
/// A profile binds `…/ValueSet/x` without a version, meaning "the version in
/// my package's dependency context" — the IG publisher and the HL7 validator
/// resolve it that way. A terminology server holding several versions (IPS
/// 1.1.0 from an installed IG and IPS 2.0.0 from a bundled package) would
/// otherwise answer from its newest one, which for IPS replaced the LOINC
/// answers with SNOMED codes. Bindings that already name a version, and value
/// sets without a pin, are left alone. Returns `None` when nothing changed.
pub fn pin_binding_versions(
    schema: &FhirSchema,
    pins: &std::collections::HashMap<String, String>,
) -> Option<FhirSchema> {
    fn walk(s: &mut FhirSchema, pins: &std::collections::HashMap<String, String>) -> bool {
        let mut changed = false;
        if let Some(b) = s.binding.as_mut()
            && !b.value_set.contains('|')
            && let Some(v) = pins.get(&b.value_set)
        {
            b.value_set = format!("{}|{v}", b.value_set);
            changed = true;
        }
        let mut each = |map: &mut IndexMap<String, Arc<FhirSchema>>| {
            for child in map.values_mut() {
                let mut c = (**child).clone();
                if walk(&mut c, pins) {
                    *child = Arc::new(c);
                    changed = true;
                }
            }
        };
        if let Some(m) = s.elements.as_mut() {
            each(m);
        }
        if let Some(m) = s.extensions.as_mut() {
            each(m);
        }
        if let Some(slicing) = s.slicing.as_mut() {
            for slice in slicing.slices.values_mut() {
                if let Some(child) = slice.schema.as_mut() {
                    let mut c = (**child).clone();
                    if walk(&mut c, pins) {
                        *child = Arc::new(c);
                        changed = true;
                    }
                }
            }
        }
        changed
    }
    if pins.is_empty() {
        return None;
    }
    let mut out = schema.clone();
    let mut changed = walk(&mut out, pins);
    // The base definition, when the package context names its version (a
    // profile of an IG built on an older release of another installed IG).
    if let Some(base) = out.base.as_mut()
        && !base.contains('|')
        && base.contains("://")
        && let Some(v) = pins.get(base.as_str())
    {
        *base = format!("{base}|{v}");
        changed = true;
    }
    changed.then_some(out)
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn an_unversioned_base_is_pinned_to_its_package_context() {
        let schema = FhirSchema {
            url: Some("https://emr/Encounter".into()),
            base: Some("https://twcore/StructureDefinition/Encounter-twcore".into()),
            ..Default::default()
        };
        let pins: std::collections::HashMap<String, String> = [(
            "https://twcore/StructureDefinition/Encounter-twcore".to_string(),
            "0.3.2".to_string(),
        )]
        .into();
        let pinned = pin_binding_versions(&schema, &pins).expect("changed");
        assert_eq!(
            pinned.base.as_deref(),
            Some("https://twcore/StructureDefinition/Encounter-twcore|0.3.2")
        );
        let versioned = FhirSchema {
            base: Some("https://twcore/StructureDefinition/Encounter-twcore|1.0.0".into()),
            ..schema
        };
        assert!(pin_binding_versions(&versioned, &pins).is_none());
    }

    #[test]
    fn unversioned_bindings_are_pinned_everywhere_and_versioned_ones_kept() {
        let leaf = |vs: &str| {
            Arc::new(FhirSchema {
                binding: Some(Binding {
                    value_set: vs.to_string(),
                    strength: Some("required".into()),
                }),
                ..Default::default()
            })
        };
        let mut elements = IndexMap::new();
        elements.insert("smoking".to_string(), leaf("http://ips/ValueSet/smoking"));
        elements.insert("status".to_string(), leaf("http://hl7.org/fhir/ValueSet/obs|4.0.1"));
        elements.insert("other".to_string(), leaf("http://x/ValueSet/unpinned"));
        let mut slices = IndexMap::new();
        slices.insert(
            "s".to_string(),
            Slice {
                match_: None,
                min: None,
                max: None,
                order: None,
                reslice: None,
                slice_is_constraining: None,
                schema: Some(leaf("http://ips/ValueSet/smoking")),
            },
        );
        let schema = FhirSchema {
            elements: Some(elements),
            slicing: Some(Slicing {
                slices,
                rules: None,
                ordered: None,
            }),
            ..Default::default()
        };
        let mut pins = std::collections::HashMap::new();
        pins.insert("http://ips/ValueSet/smoking".to_string(), "1.1.0".to_string());
        pins.insert("http://hl7.org/fhir/ValueSet/obs".to_string(), "9".to_string());
        let pinned = pin_binding_versions(&schema, &pins).expect("changed");
        let vs = |s: &FhirSchema, k: &str| {
            s.elements.as_ref().unwrap()[k].binding.as_ref().unwrap().value_set.clone()
        };
        assert_eq!(vs(&pinned, "smoking"), "http://ips/ValueSet/smoking|1.1.0");
        assert_eq!(vs(&pinned, "status"), "http://hl7.org/fhir/ValueSet/obs|4.0.1");
        assert_eq!(vs(&pinned, "other"), "http://x/ValueSet/unpinned");
        let slice = pinned.slicing.as_ref().unwrap().slices["s"].schema.clone().unwrap();
        assert_eq!(
            slice.binding.as_ref().unwrap().value_set,
            "http://ips/ValueSet/smoking|1.1.0"
        );
        assert!(pin_binding_versions(&schema, &Default::default()).is_none());
    }
}
