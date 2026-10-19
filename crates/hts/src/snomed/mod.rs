//! SNOMED CT specifics: the in-memory concept model, the compositional
//! grammar (post-coordinated expressions) and the concept model rules (MRCM).

/// The SNOMED CT code system URI.
pub const SNOMED_URL: &str = "http://snomed.info/sct";
/// Description types.
pub const FSN: &str = "900000000000003001";
pub const SYNONYM: &str = "900000000000013009";
pub const DEFINITION: &str = "900000000000550004";

/// The name of a description type, for `designation.use.display`.
pub fn description_type_name(code: &str) -> Option<&'static str> {
    match code {
        FSN => Some("Fully specified name"),
        SYNONYM => Some("Synonym"),
        DEFINITION => Some("Definition"),
        _ => None,
    }
}

pub mod ecl;
pub mod model;
pub mod mrcm;
pub mod nf;
pub mod ops;
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

#[cfg(test)]
mod tests {
    use super::ecl::{Evaluator, Virtual, parse};
    use super::*;

    const ROOT: &str = "138875005";

    /// A toy edition: diseases of the liver, defined by finding site and
    /// morphology, with MRCM rules for both attributes.
    fn model() -> SnomedModel {
        let c = |code: &str, primitive: bool, name: &str| {
            (
                code.to_string(),
                true,
                primitive,
                Some(name.to_string()),
                Some(name.to_string()),
            )
        };
        let concepts = vec![
            c(ROOT, true, "SNOMED CT Concept"),
            c("404684003", true, "Clinical finding"),
            c("64572001", true, "Disease"),
            c("123037004", true, "Body structure"),
            c("10200004", true, "Liver"),
            c("181268008", true, "Entire liver"),
            c("39607008", true, "Lung"),
            c("410662002", true, "Concept model attribute"),
            c("363698007", true, "Finding site"),
            c("116676008", true, "Associated morphology"),
            c("49755003", true, "Morphologic abnormality"),
            c("23583003", true, "Inflammation"),
            c("235856003", false, "Disorder of liver"),
            c("128241005", false, "Inflammatory disease of liver"),
            c("197321007", true, "Some hepatitis"),
        ];
        let isa = |a: &str, b: &str| {
            (
                a.to_string(),
                0,
                "116680003".to_string(),
                b.to_string(),
                false,
            )
        };
        let rel = |a: &str, g: i32, t: &str, v: &str| {
            (a.to_string(), g, t.to_string(), v.to_string(), false)
        };
        let relationships = vec![
            isa("404684003", ROOT),
            isa("64572001", "404684003"),
            isa("123037004", ROOT),
            isa("10200004", "123037004"),
            isa("181268008", "10200004"),
            isa("39607008", "123037004"),
            isa("410662002", ROOT),
            isa("363698007", "410662002"),
            isa("116676008", "410662002"),
            isa("49755003", ROOT),
            isa("23583003", "49755003"),
            isa("235856003", "64572001"),
            rel("235856003", 1, "363698007", "10200004"),
            isa("128241005", "235856003"),
            rel("128241005", 1, "363698007", "10200004"),
            rel("128241005", 1, "116676008", "23583003"),
            isa("197321007", "128241005"),
            rel("197321007", 1, "363698007", "10200004"),
            rel("197321007", 1, "116676008", "23583003"),
        ];
        let mrcm = |k: &str, a: &str, c1: &str, c2: &str| {
            let (c3, c4) = if k == "domain" {
                ("0..*", "0..1")
            } else {
                ("", "")
            };
            (
                k.to_string(),
                a.to_string(),
                c1.to_string(),
                c2.to_string(),
                c3.to_string(),
                c4.to_string(),
            )
        };
        SnomedModel::build(ModelRows {
            concepts,
            relationships,
            refset_members: vec![],
            mrcm: vec![
                mrcm("domain", "363698007", "404684003", "1"),
                mrcm("domain", "116676008", "404684003", "1"),
                mrcm("range", "363698007", "<< 123037004 |Body structure|", ""),
                mrcm(
                    "range",
                    "116676008",
                    "<< 49755003 |Morphologic abnormality|",
                    "",
                ),
            ],
        })
    }

    fn codes(m: &SnomedModel, ecl: &str) -> Vec<String> {
        let e = parse(ecl).unwrap();
        let mut v: Vec<String> = Evaluator::new(m)
            .eval(&e)
            .unwrap()
            .iter()
            .map(|&i| m.code(i))
            .collect();
        v.sort();
        v
    }

    fn virt(m: &SnomedModel, code: &str) -> Virtual {
        let e = scg::parse(code).unwrap();
        Virtual::from_sub(m, &e.body).unwrap()
    }

    #[test]
    fn ecl_refinements_children_and_dotted_attributes() {
        let m = model();
        assert_eq!(
            codes(&m, "< 64572001 : 363698007 = << 10200004"),
            ["128241005", "197321007", "235856003"]
        );
        assert_eq!(
            codes(&m, "< 64572001 : { 363698007 = *, 116676008 = 23583003 }"),
            ["128241005", "197321007"]
        );
        assert_eq!(
            codes(&m, "< 64572001 : [0..0] 116676008 = *"),
            ["235856003"]
        );
        assert_eq!(codes(&m, "<! 64572001"), ["235856003"]);
        assert_eq!(codes(&m, ">>! 235856003"), ["235856003", "64572001"]);
        assert_eq!(codes(&m, "< 64572001 . 363698007"), ["10200004"]);
        assert_eq!(
            codes(&m, "<< 123037004 : R 363698007 = << 128241005"),
            ["10200004"]
        );
        assert_eq!(
            codes(&m, "<< 64572001 {{ C definitionStatus = primitive }}"),
            ["197321007", "64572001"]
        );
    }

    #[test]
    fn expressions_satisfy_constraints_as_refinements_of_their_focus() {
        let m = model();
        let v = virt(&m, "64572001:{363698007=181268008}");
        let mut ev = Evaluator::new(&m);
        let t = |ev: &mut Evaluator, ecl: &str| {
            ev.expression_matches(&v, &parse(ecl).unwrap()).unwrap()
        };
        assert!(t(&mut ev, "< 64572001 : 363698007 = << 10200004"));
        assert!(t(&mut ev, "<< 404684003"));
        assert!(!t(&mut ev, "<! 64572001"));
        assert!(!t(&mut ev, "< 64572001 : 116676008 = *"));
        assert!(!t(&mut ev, "<< 64572001 MINUS << 404684003"));
    }

    #[test]
    fn normal_forms_decide_subsumption() {
        let m = model();
        let o = |a: &str, b: &str| nf::outcome(&m, &virt(&m, a), &virt(&m, b));
        assert_eq!(
            o("235856003", "64572001:{363698007=10200004}"),
            Some("equivalent")
        );
        assert_eq!(
            o("64572001:{363698007=10200004}", "197321007"),
            Some("subsumes")
        );
        assert_eq!(
            o(
                "128241005",
                "235856003:{363698007=10200004,116676008=23583003}"
            ),
            Some("equivalent")
        );
        assert_eq!(
            o(
                "64572001:{363698007=181268008}",
                "64572001:{363698007=10200004}"
            ),
            Some("subsumed-by")
        );
        assert_eq!(
            o(
                "64572001:{363698007=39607008}",
                "64572001:{363698007=10200004}"
            ),
            None
        );
    }

    #[test]
    fn concept_model_rules() {
        let m = model();
        let check = |code: &str| mrcm::check(&m, &scg::parse(code).unwrap().body);
        assert!(check("64572001:{363698007=181268008,116676008=23583003}").is_ok());
        let domain = check("10200004:{363698007=39607008}").unwrap_err();
        assert!(
            domain.contains("only valid in the domain 404684003 |Clinical finding|"),
            "{domain}"
        );
        let range = check("64572001:{363698007=23583003}").unwrap_err();
        assert!(
            range.contains("the value must satisfy \"<< 123037004 |Body structure|\""),
            "{range}"
        );
        assert!(
            check("64572001:363698007=10200004")
                .unwrap_err()
                .contains("relationship group")
        );
        assert!(
            check("64572001:{10200004=39607008}")
                .unwrap_err()
                .contains("not valid in this context")
        );
        assert!(
            check("64572001:{363698007=10200004,363698007=181268008}")
                .unwrap_err()
                .contains("at most 1")
        );
    }
}
