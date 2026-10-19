//! SNOMED CT specifics: the in-memory concept model, the compositional
//! grammar (post-coordinated expressions) and the concept model rules (MRCM).

/// The SNOMED CT code system URI.
pub const SNOMED_URL: &str = "http://snomed.info/sct";
pub mod ecl;
pub mod model;
pub mod scg;
pub mod store;

pub use model::{ModelRows, SnomedModel};

/// One concept's row in `snomed_concepts`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConceptRow {
    pub code: String,
    pub active: bool,
    pub primitive: bool,
    /// `YYYYMMDD`.
    pub effective_time: Option<String>,
    pub module: Option<String>,
    /// Active description terms, in description id order.
    pub terms: Vec<String>,
    /// Inactive description terms (not also active), in id order.
    pub inactive_terms: Vec<String>,
}

/// What the RF2 importer stores for the concept model of one edition.
#[derive(Debug, Clone, Default)]
pub struct ModelImport {
    pub concepts: Vec<ConceptRow>,
    /// `(source, group, type, value, concrete)`, is-a included.
    pub relationships: Vec<(String, i32, String, String, bool)>,
    /// MRCM rows, see [`ModelRows::mrcm`].
    pub mrcm: Vec<(String, String, String, String, String, String)>,
}

/// A stored concept as `$lookup` and `$validate-code` need it, with its
/// display (preferred term).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConceptInfo {
    pub row: ConceptRow,
    pub display: Option<String>,
}

/// Whether `url` is SNOMED CT.
pub fn is_snomed(url: &str) -> bool {
    url == SNOMED_URL
}
