//! Lossless FHIR XML → JSON conversion, driven by the schema registry
//! (feature `xml`).
//!
//! The typed `helios-fhir` models parse XML leniently: unknown elements and
//! attributes vanish, an empty `url=""` disappears, a value the model cannot
//! hold fails the whole parse. Validation then judges something other than
//! what the client sent. This converter follows the FHIR JSON
//! representation rules instead and keeps everything, so the validator sees
//! the instance as written:
//!
//! - element names map to JSON keys one-to-one (choice branches such as
//!   `valueQuantity` included); repeating elements (`array` in any schema of
//!   the element) become arrays;
//! - a primitive's `value` attribute becomes the JSON value — booleans and
//!   numbers typed by the element's FHIR type, a value that does not parse
//!   kept as a string so the validator reports it — and its `id` /
//!   `extension` go to the `_name` sidecar, aligned with `null`s in arrays;
//! - attributes naming an element of the type (`Extension.url`,
//!   `Element.id`) become those elements; any other attribute is kept as
//!   `@name`, which the validator reports as an unknown element;
//! - `Narrative.div` (type `xhtml`) is the XHTML source text;
//! - resource-typed elements (`contained`, `Bundle.entry.resource`) wrap a
//!   resource element, which converts with its own `resourceType`;
//! - an element the schemas do not know is converted structurally (value
//!   attribute or children) under its own name, so it is reported unknown;
//! - non-whitespace text inside a FHIR element is kept as `#text`.
//!
//! Element order is not checked (JSON has none to compare against).

use crate::resolver::SchemaResolver;
use crate::schema::FhirSchema;
use indexmap::IndexMap;
use serde_json::{Map, Number, Value};
use std::sync::Arc;

const FHIR_NS: &str = "http://hl7.org/fhir";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
/// Depth bound for the schema base/type chains (cycles are malformed packs).
const MAX_CHAIN: usize = 16;

/// Convert a FHIR XML resource into its JSON form. Errors only on XML that
/// is not well-formed or whose root is not a FHIR element.
pub fn xml_to_json(xml: &str, resolver: &dyn SchemaResolver) -> Result<Value, String> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, options)
        .map_err(|e| format!("XML is not well-formed: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().namespace() != Some(FHIR_NS) {
        return Err(format!(
            "root element '{}' is not in the FHIR namespace ({FHIR_NS})",
            root.tag_name().name()
        ));
    }
    Ok(Converter { resolver, xml }.resource(root))
}

struct Converter<'r> {
    resolver: &'r dyn SchemaResolver,
    xml: &'r str,
}

/// The JSON representation of a FHIR primitive type.
enum Json {
    Bool,
    Integer,
    Decimal,
    String,
}

fn json_kind(fhir_type: &str) -> Json {
    match fhir_type {
        "boolean" => Json::Bool,
        "integer" | "positiveInt" | "unsignedInt" | "integer64" => Json::Integer,
        "decimal" => Json::Decimal,
        _ => Json::String,
    }
}

/// Values collected for one JSON key of an object, in document order.
#[derive(Default)]
struct Slot {
    array: bool,
    primitive: bool,
    values: Vec<Value>,
    sidecars: Vec<Option<Value>>,
}

impl Converter<'_> {
    fn resource(&self, node: roxmltree::Node) -> Value {
        let resource_type = node.tag_name().name();
        let mut scope = Vec::new();
        if let Some(schema) = self.resolver.resolve(resource_type) {
            self.push_chain(&mut scope, schema);
        }
        let mut obj = Map::new();
        obj.insert(
            "resourceType".to_string(),
            Value::String(resource_type.to_string()),
        );
        self.fill(node, &scope, &mut obj);
        Value::Object(obj)
    }

    /// `schema` and its `base` chain.
    fn push_chain(&self, scope: &mut Vec<Arc<FhirSchema>>, schema: Arc<FhirSchema>) {
        let mut current = Some(schema);
        let mut depth = 0;
        while let Some(schema) = current.take() {
            if depth > MAX_CHAIN || scope.iter().any(|s| Arc::ptr_eq(s, &schema)) {
                break;
            }
            depth += 1;
            current = schema
                .base
                .as_deref()
                .and_then(|b| self.resolver.resolve(b));
            scope.push(schema);
        }
    }

    /// The schemas that describe the children of an element: the element
    /// itself (backbone elements carry their children inline), what its
    /// `elementReference` points at, and its type with that type's bases.
    fn element_scope(&self, element: &Arc<FhirSchema>) -> Vec<Arc<FhirSchema>> {
        let mut scope = vec![Arc::clone(element)];
        if let Some(path) = &element.element_reference
            && let Some(target) = self.element_reference(path)
        {
            scope.push(Arc::clone(&target));
            if let Some(t) = target.type_.as_deref()
                && let Some(schema) = self.resolver.resolve(t)
            {
                self.push_chain(&mut scope, schema);
            }
        }
        if let Some(t) = element.type_.as_deref()
            && let Some(schema) = self.resolver.resolve(t)
        {
            self.push_chain(&mut scope, schema);
        }
        scope
    }

    fn element_reference(&self, segments: &[String]) -> Option<Arc<FhirSchema>> {
        let (root, rest) = segments.split_first()?;
        let mut current = self.resolver.resolve(root)?;
        let mut iter = rest.iter();
        while let Some(marker) = iter.next() {
            if marker != "elements" {
                return None;
            }
            let name = iter.next()?;
            let next = Arc::clone(current.elements.as_ref()?.get(name)?);
            current = next;
        }
        Some(current)
    }

    /// Every schema in `scope` declaring the child `name`.
    fn lookup(&self, scope: &[Arc<FhirSchema>], name: &str) -> Vec<Arc<FhirSchema>> {
        scope
            .iter()
            .filter_map(|s| s.elements.as_ref()?.get(name).cloned())
            .collect()
    }

    /// The element's FHIR type: the first declared one across the schemas
    /// (a referenced element's type when only a reference is given).
    fn element_type(&self, elements: &[Arc<FhirSchema>]) -> Option<String> {
        elements.iter().find_map(|e| e.type_.clone()).or_else(|| {
            elements.iter().find_map(|e| {
                self.element_reference(e.element_reference.as_ref()?)?
                    .type_
                    .clone()
            })
        })
    }

    fn is_primitive_type(&self, fhir_type: &str) -> bool {
        if fhir_type == "xhtml" {
            return false;
        }
        self.resolver
            .resolve(fhir_type)
            .is_some_and(|s| s.is_primitive())
            || fhir_type.starts_with("http://hl7.org/fhirpath/System.")
    }

    /// Convert the attributes and children of `node` into `obj`.
    fn fill(&self, node: roxmltree::Node, scope: &[Arc<FhirSchema>], obj: &mut Map<String, Value>) {
        let mut slots: IndexMap<String, Slot> = IndexMap::new();
        for attr in node.attributes() {
            if attr.namespace().is_some() {
                continue; // xsi:schemaLocation and friends
            }
            let name = attr.name();
            if self.lookup(scope, name).is_empty() {
                obj.insert(format!("@{name}"), Value::String(attr.value().to_string()));
            } else {
                obj.insert(name.to_string(), Value::String(attr.value().to_string()));
            }
        }
        let mut text = String::new();
        for child in node.children() {
            if child.is_text() {
                text.push_str(child.text().unwrap_or_default());
                continue;
            }
            if !child.is_element() {
                continue;
            }
            let name = child.tag_name().name();
            let elements = self.lookup(scope, name);
            let slot = slots.entry(name.to_string()).or_default();
            if elements.iter().any(|e| e.array == Some(true)) {
                slot.array = true;
            }
            let fhir_type = self.element_type(&elements);
            match fhir_type.as_deref() {
                _ if elements.is_empty() => slot.values.push(self.unknown(child)),
                Some("xhtml") => slot.values.push(Value::String(self.xhtml(child))),
                Some("Resource" | "DomainResource") => {
                    let value = child
                        .children()
                        .find(|c| c.is_element())
                        .map(|r| self.resource(r))
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    slot.values.push(value);
                }
                Some(t) if self.is_primitive_type(t) => {
                    slot.primitive = true;
                    let (value, sidecar) = self.primitive(child, t);
                    slot.values.push(value);
                    slot.sidecars.push(sidecar);
                }
                _ => {
                    let mut child_scope = Vec::new();
                    for e in &elements {
                        for s in self.element_scope(e) {
                            if !child_scope.iter().any(|c| Arc::ptr_eq(c, &s)) {
                                child_scope.push(s);
                            }
                        }
                    }
                    let mut child_obj = Map::new();
                    self.fill(child, &child_scope, &mut child_obj);
                    slot.values.push(Value::Object(child_obj));
                }
            }
        }
        if !text.trim().is_empty() {
            obj.insert("#text".to_string(), Value::String(text.trim().to_string()));
        }
        for (name, slot) in slots {
            let as_array = slot.array || slot.values.len() > 1;
            if slot.primitive {
                let has_sidecar = slot.sidecars.iter().any(Option::is_some);
                if as_array {
                    // Values carried only by extensions: `_name` alone, as
                    // the published JSON writes it.
                    if slot.values.iter().any(|v| !v.is_null()) || !has_sidecar {
                        obj.insert(name.clone(), Value::Array(slot.values));
                    }
                    if has_sidecar {
                        let sidecars = slot
                            .sidecars
                            .into_iter()
                            .map(|s| s.unwrap_or(Value::Null))
                            .collect();
                        obj.insert(format!("_{name}"), Value::Array(sidecars));
                    }
                } else {
                    let value = slot.values.into_iter().next().unwrap_or(Value::Null);
                    let sidecar = slot.sidecars.into_iter().next().flatten();
                    if !value.is_null() || sidecar.is_none() {
                        obj.insert(name.clone(), value);
                    }
                    if let Some(sidecar) = sidecar {
                        obj.insert(format!("_{name}"), sidecar);
                    }
                }
            } else if as_array {
                obj.insert(name, Value::Array(slot.values));
            } else if let Some(value) = slot.values.into_iter().next() {
                obj.insert(name, value);
            }
        }
    }

    /// A primitive element: its JSON value (`Null` without a `value`
    /// attribute) and its sidecar (`id` / `extension`), if any.
    fn primitive(&self, node: roxmltree::Node, fhir_type: &str) -> (Value, Option<Value>) {
        let value = match node.attribute("value") {
            None => Value::Null,
            Some(raw) => typed(raw, fhir_type),
        };
        let mut sidecar = Map::new();
        for attr in node.attributes() {
            match attr.name() {
                "value" if attr.namespace().is_none() => {}
                "id" if attr.namespace().is_none() => {
                    sidecar.insert("id".to_string(), Value::String(attr.value().to_string()));
                }
                _ if attr.namespace().is_some() => {}
                other => {
                    sidecar.insert(format!("@{other}"), Value::String(attr.value().to_string()));
                }
            }
        }
        let mut scope = Vec::new();
        if let Some(element) = self.resolver.resolve("Element") {
            self.push_chain(&mut scope, element);
        }
        let mut children = Map::new();
        self.fill_children_only(node, &scope, &mut children);
        sidecar.extend(children);
        let sidecar = if sidecar.is_empty() && !value.is_null() {
            None
        } else {
            Some(Value::Object(sidecar))
        };
        (value, sidecar)
    }

    /// [`Self::fill`] without the node's own attributes (a primitive's
    /// attributes are its value and id).
    fn fill_children_only(
        &self,
        node: roxmltree::Node,
        scope: &[Arc<FhirSchema>],
        obj: &mut Map<String, Value>,
    ) {
        let mut tmp = Map::new();
        self.fill(node, scope, &mut tmp);
        for (k, v) in tmp {
            if k.starts_with('@') {
                continue;
            }
            obj.insert(k, v);
        }
    }

    /// An element no schema declares: kept, structurally, so the validator
    /// reports it.
    fn unknown(&self, node: roxmltree::Node) -> Value {
        if node.children().all(|c| !c.is_element())
            && let Some(value) = node.attribute("value")
        {
            return Value::String(value.to_string());
        }
        if node.tag_name().namespace() == Some(FHIR_NS)
            && node
                .tag_name()
                .name()
                .starts_with(|c: char| c.is_ascii_uppercase())
            && self.resolver.resolve(node.tag_name().name()).is_some()
        {
            return self.resource(node);
        }
        let mut obj = Map::new();
        self.fill(node, &[], &mut obj);
        Value::Object(obj)
    }

    /// The XHTML source of a `div`, with the XHTML namespace declared on it
    /// when the document declared it further up.
    fn xhtml(&self, node: roxmltree::Node) -> String {
        let source = &self.xml[node.range()];
        let declares = source
            .split('>')
            .next()
            .is_some_and(|start| start.contains("xmlns"));
        if !declares && node.tag_name().namespace() == Some(XHTML_NS) {
            let name_end = source
                .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .unwrap_or(source.len());
            let mut out = String::with_capacity(source.len() + 40);
            out.push_str(&source[..name_end]);
            out.push_str(&format!(" xmlns=\"{XHTML_NS}\""));
            out.push_str(&source[name_end..]);
            return out;
        }
        source.to_string()
    }
}

/// A primitive `value` attribute in its JSON form.
fn typed(raw: &str, fhir_type: &str) -> Value {
    match json_kind(fhir_type) {
        Json::Bool => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.to_string()),
        },
        // JSON's number grammar is FHIR's integer/decimal grammar: a value
        // it rejects (`01`, `+1`, `1.`) stays a string for the validator.
        Json::Integer => serde_json::from_str::<Number>(raw)
            .ok()
            .filter(|n| n.is_i64() || n.is_u64())
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.to_string())),
        Json::Decimal => serde_json::from_str::<Number>(raw)
            .map(Value::Number)
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        Json::String => Value::String(raw.to_string()),
    }
}

#[cfg(all(test, feature = "R4"))]
mod tests {
    use super::*;
    use crate::packs::core_registry;
    use helios_fhir::FhirVersion;
    use serde_json::json;

    fn convert(xml: &str) -> Value {
        xml_to_json(xml, core_registry(FhirVersion::R4).as_ref()).unwrap()
    }

    #[test]
    fn primitives_arrays_and_sidecars() {
        let v = convert(
            r#"<Patient xmlns="http://hl7.org/fhir">
                 <id value="p1"/>
                 <active value="true"/>
                 <name><given value="A"/><given id="g2"><extension url="http://x"><valueString value="y"/></extension></given></name>
                 <birthDate value="2000-01-01"/>
                 <multipleBirthInteger value="2"/>
               </Patient>"#,
        );
        assert_eq!(
            v,
            json!({"resourceType": "Patient", "id": "p1", "active": true,
                   "name": [{"given": ["A", null],
                             "_given": [null, {"id": "g2", "extension": [{"url": "http://x", "valueString": "y"}]}]}],
                   "birthDate": "2000-01-01", "multipleBirthInteger": 2})
        );
    }

    #[test]
    fn unknown_content_is_kept() {
        let v = convert(
            r#"<Patient xmlns="http://hl7.org/fhir" foo="1">
                 <gender value="xx"/><bogus value="1"/>
                 <extension url=""><valueString value="nothing"/></extension>
               </Patient>"#,
        );
        assert_eq!(v["@foo"], "1");
        assert_eq!(v["bogus"], "1");
        assert_eq!(v["gender"], "xx");
        assert_eq!(
            v["extension"],
            json!([{"url": "", "valueString": "nothing"}])
        );
    }

    #[test]
    fn narrative_contained_and_bundles() {
        let v = convert(
            r#"<Bundle xmlns="http://hl7.org/fhir"><type value="collection"/>
                 <entry><fullUrl value="urn:uuid:1"/><resource>
                   <Observation><text><status value="generated"/><div xmlns="http://www.w3.org/1999/xhtml"><p>x &amp; y</p></div></text>
                     <contained><Device><id value="d"/></Device></contained>
                     <status value="final"/><code><text value="t"/></code>
                     <valueQuantity><value value="1.50"/><unit value="mg"/></valueQuantity>
                   </Observation></resource></entry>
               </Bundle>"#,
        );
        let obs = &v["entry"][0]["resource"];
        assert_eq!(obs["resourceType"], "Observation");
        assert_eq!(
            obs["text"]["div"],
            r#"<div xmlns="http://www.w3.org/1999/xhtml"><p>x &amp; y</p></div>"#
        );
        assert_eq!(
            obs["contained"],
            json!([{"resourceType": "Device", "id": "d"}])
        );
        assert_eq!(obs["valueQuantity"]["value"].as_f64(), Some(1.5));
        assert_eq!(obs["valueQuantity"]["unit"], "mg");
    }

    #[test]
    fn recursive_elements_and_bad_values() {
        let v = convert(
            r#"<Questionnaire xmlns="http://hl7.org/fhir"><status value="active"/>
                 <item><linkId value="1"/><type value="group"/>
                   <item><linkId value="1.1"/><type value="boolean"/><required value="yes"/></item>
                 </item></Questionnaire>"#,
        );
        assert_eq!(v["item"][0]["item"][0]["linkId"], "1.1");
        // A value the type cannot hold stays a string for the validator.
        assert_eq!(v["item"][0]["item"][0]["required"], "yes");
    }

    /// Differences between two JSON trees, with numbers compared as f64;
    /// `meta` (rewritten in the JSON copies by the corpus' build script) and
    /// XHTML source text are not compared.
    fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                for (k, v) in x.iter().filter(|(k, _)| *k != "meta") {
                    match y.get(k) {
                        Some(w) => diff(&format!("{path}.{k}"), v, w, out),
                        None => out.push(format!("{path}.{k}: only in XML")),
                    }
                }
                for k in y.keys().filter(|k| *k != "meta" && !x.contains_key(*k)) {
                    out.push(format!("{path}.{k}: only in JSON"));
                }
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
                for (i, (v, w)) in x.iter().zip(y).enumerate() {
                    diff(&format!("{path}[{i}]"), v, w, out);
                }
            }
            (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => {}
            // XHTML is carried as source text; the JSON copies were
            // re-serialised (entities, quoting, empty tags).
            (Value::String(_), Value::String(_)) if path.ends_with(".div") => {}
            _ if a == b => {}
            _ => out.push(format!("{path}: {a} vs {b}")),
        }
    }

    /// The R4 spec examples ship in both formats: converting the XML must
    /// give the published JSON (modulo number formatting and XHTML
    /// whitespace).
    #[test]
    fn r4_spec_examples_convert_to_their_json() {
        let data = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fhir/tests/data");
        let Ok(dir) = std::fs::read_dir(data.join("xml/R4")) else {
            return; // corpus not checked out
        };
        let registry = core_registry(FhirVersion::R4);
        let (mut compared, mut differing) = (0, Vec::new());
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let stem = name.trim_end_matches(".xml");
            let stem = stem.split('(').next().unwrap_or(stem);
            let Ok(json) = std::fs::read_to_string(data.join(format!("json/R4/{stem}.json")))
            else {
                continue;
            };
            let Ok(expected) = serde_json::from_str::<Value>(&json) else {
                continue;
            };
            let xml = std::fs::read_to_string(entry.path()).unwrap();
            let Ok(actual) = xml_to_json(&xml, registry.as_ref()) else {
                differing.push(format!("{name}: conversion failed"));
                continue;
            };
            compared += 1;
            let mut out = Vec::new();
            diff("", &actual, &expected, &mut out);
            if !out.is_empty() {
                differing.push(format!(
                    "{name}: {}",
                    out.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
                ));
            }
        }
        assert!(compared > 500, "only {compared} pairs compared");
        assert!(
            differing.is_empty(),
            "{} of {compared} differ:\n{}",
            differing.len(),
            differing
                .iter()
                .take(60)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn non_fhir_roots_are_rejected() {
        assert!(xml_to_json("<Patient/>", core_registry(FhirVersion::R4).as_ref()).is_err());
        assert!(xml_to_json("<Patient", core_registry(FhirVersion::R4).as_ref()).is_err());
    }
}
