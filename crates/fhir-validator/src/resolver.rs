//! Schema resolution: how the engine turns a name or canonical URL into a
//! [`FhirSchema`].
//!
//! Mirrors the SearchParameter registry pattern in `crates/fhir/src/search/`:
//! an `Arc`-backed in-memory registry, composable so tenant-uploaded profiles
//! can overlay the embedded core packs.

use crate::schema::FhirSchema;
use std::collections::HashMap;
use std::sync::Arc;

/// Resolves a schema reference — a bare name (`"Patient"`) or a canonical URL
/// (`"http://hl7.org/fhir/StructureDefinition/Patient"`) — to a schema.
pub trait SchemaResolver: Send + Sync {
    /// Returns the schema, or `None` if the reference is unknown.
    fn resolve(&self, reference: &str) -> Option<Arc<FhirSchema>>;
}

/// In-memory schema registry.
///
/// Schemas are indexed under every identity they carry: the explicit key they
/// were inserted with, their `url`, and their `name`. All aliases map to the
/// same `Arc`, so identity-based deduplication in the engine treats them as
/// one schema.
#[derive(Debug, Default)]
pub struct SchemaRegistry {
    map: HashMap<String, Arc<FhirSchema>>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extension definitions applicable at any of the given element
    /// contexts (#363): schemas of kind `extension` whose declared context
    /// expressions intersect `contexts`. Deduplicated by canonical URL and
    /// sorted by name for a stable picker order.
    pub fn extensions_applicable(&self, contexts: &[&str]) -> Vec<Arc<FhirSchema>> {
        let mut out: Vec<Arc<FhirSchema>> = self
            .map
            .values()
            .filter(|schema| schema.kind.as_deref() == Some(crate::schema::kind::EXTENSION))
            .filter(|schema| {
                schema
                    .context
                    .as_deref()
                    .is_some_and(|cs| cs.iter().any(|c| contexts.contains(&c.as_str())))
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.url == b.url);
        out
    }

    /// Number of distinct keys (aliases included).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert a schema under an explicit key, plus its `url`/`name` aliases.
    ///
    /// The explicit key wins on collision (later inserts overwrite earlier
    /// ones for the same key — last write wins, like the JS fixture maps).
    pub fn insert_named(&mut self, key: impl Into<String>, schema: FhirSchema) {
        let schema = Arc::new(schema);
        let key = key.into();
        if let Some(url) = schema.url.clone()
            && url != key
        {
            self.map.insert(url, Arc::clone(&schema));
        }
        if let Some(name) = schema.name.clone()
            && name != key
        {
            self.map.insert(name, Arc::clone(&schema));
        }
        self.map.insert(key, schema);
    }

    /// Insert a schema under its own `url` and `name`.
    ///
    /// Returns `false` (and stores nothing) if the schema carries neither.
    pub fn insert(&mut self, schema: FhirSchema) -> bool {
        match (schema.url.clone(), schema.name.clone()) {
            (Some(url), _) => {
                self.insert_named(url, schema);
                true
            }
            (None, Some(name)) => {
                self.insert_named(name, schema);
                true
            }
            (None, None) => false,
        }
    }

    /// Insert a schema under its canonical `url` **only** — no `name` alias.
    ///
    /// Profiles and extensions (`derivation: constraint`) are referenced by
    /// canonical URL; element `type` references resolve base types by *name*.
    /// Indexing a constraint under its name lets an extension such as
    /// `rendering-xhtml` (`name: "xhtml"`) shadow the primitive `xhtml`, so
    /// every `Narrative.div` fails as "expected object, got string". Stored
    /// (tenant-uploaded) profiles use this entry point; the embedded packs
    /// keep their two-pass `insert` because there the core schema is
    /// guaranteed to win the name alias.
    ///
    /// Returns `false` (and stores nothing) if the schema has no `url`.
    pub fn insert_by_url(&mut self, schema: FhirSchema) -> bool {
        match schema.url.clone() {
            Some(url) => {
                self.map.insert(url, Arc::new(schema));
                true
            }
            None => false,
        }
    }

    /// Remove the schema reachable under `reference`, together with every
    /// other key (url, name, explicit key) that aliases the same schema.
    ///
    /// Returns `true` if something was removed. Removing an alias that
    /// pointed at a schema also registered under a *different* schema's key
    /// only drops the aliases of the schema actually found.
    pub fn remove(&mut self, reference: &str) -> bool {
        let Some(target) = self.map.get(reference).cloned() else {
            return false;
        };
        self.map.retain(|_, schema| !Arc::ptr_eq(schema, &target));
        true
    }

    /// Build a registry from `(key, schema)` pairs.
    pub fn from_named(iter: impl IntoIterator<Item = (String, FhirSchema)>) -> Self {
        let mut reg = Self::new();
        for (k, s) in iter {
            reg.insert_named(k, s);
        }
        reg
    }
}

impl SchemaResolver for SchemaRegistry {
    /// A versioned canonical (`url|version`, as instance data, `meta.profile`
    /// and IG profiles write them — e.g. an extension URL
    /// `…/patient-nationality|5.3.0`) resolves to the schema registered under
    /// the bare `url`: the registry holds one schema per canonical, so the
    /// version is advisory here (the HL7 validator likewise falls back to the
    /// loaded version of a canonical).
    fn resolve(&self, reference: &str) -> Option<Arc<FhirSchema>> {
        if let Some(schema) = self.map.get(reference) {
            return Some(Arc::clone(schema));
        }
        match reference.split_once('|') {
            Some((url, _)) if !url.is_empty() => self.map.get(url).cloned(),
            _ => None,
        }
    }
}

/// Layered resolver: earlier layers win. Used to overlay tenant-uploaded
/// profiles on top of an embedded core pack.
#[derive(Clone, Default)]
pub struct CompositeResolver {
    layers: Vec<Arc<dyn SchemaResolver>>,
}

impl CompositeResolver {
    pub fn new(layers: Vec<Arc<dyn SchemaResolver>>) -> Self {
        Self { layers }
    }

    /// Add a layer with lower precedence than all existing layers.
    pub fn push(&mut self, layer: Arc<dyn SchemaResolver>) {
        self.layers.push(layer);
    }
}

impl SchemaResolver for CompositeResolver {
    fn resolve(&self, reference: &str) -> Option<Arc<FhirSchema>> {
        self.layers.iter().find_map(|l| l.resolve(reference))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> FhirSchema {
        FhirSchema {
            name: Some(name.to_string()),
            url: Some(format!("http://example.org/{name}")),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_by_key_url_and_name() {
        let mut reg = SchemaRegistry::new();
        reg.insert_named("PatKey", named("Patient"));
        let by_key = reg.resolve("PatKey").unwrap();
        let by_url = reg.resolve("http://example.org/Patient").unwrap();
        let by_name = reg.resolve("Patient").unwrap();
        assert!(Arc::ptr_eq(&by_key, &by_url));
        assert!(Arc::ptr_eq(&by_key, &by_name));
        assert!(reg.resolve("nope").is_none());
    }

    #[test]
    fn versioned_canonical_resolves_by_bare_url() {
        let mut reg = SchemaRegistry::new();
        reg.insert(named("Patient"));
        let bare = reg.resolve("http://example.org/Patient").unwrap();
        let versioned = reg.resolve("http://example.org/Patient|5.3.0").unwrap();
        assert!(Arc::ptr_eq(&bare, &versioned));
        assert!(
            reg.resolve("http://example.org/Patient|").is_some(),
            "an empty version is the bare canonical"
        );
        assert!(reg.resolve("http://example.org/Other|1.0").is_none());
        assert!(reg.resolve("|1.0").is_none());

        // Through a composite: an overlay layer and the core layer both apply.
        let composite =
            CompositeResolver::new(vec![Arc::new(SchemaRegistry::new()), Arc::new(reg)]);
        assert!(
            composite
                .resolve("http://example.org/Patient|4.0.1")
                .is_some()
        );
    }

    #[test]
    fn insert_by_url_does_not_claim_the_name_alias() {
        let mut reg = SchemaRegistry::new();
        // A base type registered the usual way (url + name).
        reg.insert(FhirSchema {
            name: Some("xhtml".into()),
            url: Some("http://hl7.org/fhir/StructureDefinition/xhtml".into()),
            kind: Some("primitive-type".into()),
            ..Default::default()
        });
        // An extension whose *name* collides with the base type (the real
        // `rendering-xhtml` extension in hl7.fhir.r4.core).
        assert!(reg.insert_by_url(FhirSchema {
            name: Some("xhtml".into()),
            url: Some("http://hl7.org/fhir/StructureDefinition/rendering-xhtml".into()),
            kind: Some("extension".into()),
            derivation: Some("constraint".into()),
            ..Default::default()
        }));

        assert_eq!(
            reg.resolve("xhtml").unwrap().kind.as_deref(),
            Some("primitive-type"),
            "the name alias must still resolve the base type"
        );
        assert_eq!(
            reg.resolve("http://hl7.org/fhir/StructureDefinition/rendering-xhtml")
                .unwrap()
                .kind
                .as_deref(),
            Some("extension")
        );
        assert!(
            !reg.insert_by_url(FhirSchema {
                name: Some("no-url".into()),
                ..Default::default()
            }),
            "a schema without a url cannot be registered by url"
        );
    }

    #[test]
    fn remove_drops_every_alias_of_the_schema() {
        let mut reg = SchemaRegistry::new();
        reg.insert_named("PatKey", named("Patient"));
        reg.insert(named("Observation"));
        assert_eq!(reg.len(), 5);

        assert!(reg.remove("http://example.org/Patient"));
        assert!(reg.resolve("PatKey").is_none());
        assert!(reg.resolve("Patient").is_none());
        assert!(reg.resolve("http://example.org/Patient").is_none());
        assert!(
            reg.resolve("http://example.org/Observation").is_some(),
            "unrelated schemas are untouched"
        );
        assert_eq!(reg.len(), 2);

        assert!(!reg.remove("http://example.org/Patient"), "already gone");
        assert!(!reg.remove("never-there"));
    }

    #[test]
    fn composite_earlier_layer_wins() {
        let mut base = SchemaRegistry::new();
        base.insert_named(
            "Patient",
            FhirSchema {
                kind: Some("resource".into()),
                ..Default::default()
            },
        );
        let mut overlay = SchemaRegistry::new();
        overlay.insert_named(
            "Patient",
            FhirSchema {
                kind: Some("constraint".into()),
                ..Default::default()
            },
        );

        let composite = CompositeResolver::new(vec![Arc::new(overlay), Arc::new(base)]);
        assert_eq!(
            composite.resolve("Patient").unwrap().kind.as_deref(),
            Some("constraint")
        );
    }
}

#[cfg(test)]
mod extension_catalogue_tests {
    use crate::packs::core_registry;
    use helios_fhir::FhirVersion;

    /// #363: the R4 pack now embeds the core extension definitions with
    /// their applicability contexts, queryable by element context.
    #[test]
    fn r4_pack_offers_profiled_extensions_by_context() {
        let registry = core_registry(FhirVersion::R4);
        let for_patient = registry.extensions_applicable(&["Patient"]);
        assert!(
            for_patient.iter().any(|s| s.url.as_deref()
                == Some("http://hl7.org/fhir/StructureDefinition/patient-birthPlace")),
            "birthPlace applies to Patient: {:?}",
            for_patient
                .iter()
                .filter_map(|s| s.url.clone())
                .take(8)
                .collect::<Vec<_>>()
        );
        let for_name = registry.extensions_applicable(&["HumanName.family"]);
        assert!(
            for_name.iter().any(|s| s.url.as_deref()
                == Some("http://hl7.org/fhir/StructureDefinition/humanname-own-name")),
            "own-name applies to HumanName"
        );
        assert!(
            !for_name.iter().any(|s| s.url.as_deref()
                == Some("http://hl7.org/fhir/StructureDefinition/patient-birthPlace")),
            "contexts scope the offer"
        );
    }

    /// #488: the R4B pack carries its own core extension catalogue, same
    /// contract as R4. (R5/R6 stay empty until the Extensions Pack IG is
    /// sourced — the decision is recorded on the issue.)
    #[cfg(feature = "R4B")]
    #[test]
    fn r4b_pack_offers_profiled_extensions_by_context() {
        let registry = core_registry(FhirVersion::R4B);
        let for_patient = registry.extensions_applicable(&["Patient"]);
        assert!(
            for_patient.iter().any(|s| s.url.as_deref()
                == Some("http://hl7.org/fhir/StructureDefinition/patient-birthPlace")),
            "birthPlace applies to Patient in R4B"
        );
        assert!(
            !registry
                .extensions_applicable(&["HumanName.family"])
                .is_empty(),
            "element-scoped contexts populated"
        );
    }
}
