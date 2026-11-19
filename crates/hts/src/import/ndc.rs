//! FDA National Drug Code (NDC) Directory importer.
//!
//! Parses the FDA NDC Text Files distribution and imports, into the HTS
//! normalized schema:
//!
//! * product codes (`product.txt`, labeler-product: `0777-3105`) under a
//!   product-type hierarchy;
//! * package codes (`package.txt`, labeler-product-package: `0777-3105-02`)
//!   under their product — the codes prescriptions and claims carry;
//! * the 11-digit (5-4-2, HIPAA) form of every package code
//!   (`00777310502`), under its 10-digit package code;
//! * when the ZIP also holds FDA's "NDC excluded" files
//!   (`Products_excluded.txt`, `Packages_excluded.txt`: expired, delisted and
//!   unfinished listings, from `ndc_excluded.zip`), those products and
//!   packages too, marked `status = inactive` — historical NDCs in older
//!   records and IG examples (US Core `49281012165`) then validate as
//!   inactive instead of unknown.
//!
//! # No license required
//!
//! The NDC Directory is published by the U.S. Food and Drug Administration (FDA),
//! a US federal government work in the public domain.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use helios_persistence::tenant::TenantContext;

use crate::error::HtsError;
use crate::import::BundleImportBackend;
use crate::import::ImportStats;
use crate::import::bundle_builder::{
    BuilderConcept, BuilderProperty, CodeSystemMeta, build_native_batch,
};

// ── Constants ─────────────────────────────────────────────────────────────────

const NDC_URL: &str = "http://hl7.org/fhir/sid/ndc";
const NDC_ID: &str = "ndc";
const NDC_NAME: &str = "NDC";
const NDC_TITLE: &str = "NDC (National Drug Code Directory)";
const NDC_VERSION: &str = "current";
const ROOT_CODE: &str = "NDC";

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct NdcProduct {
    code: String,
    display: String,
    product_type: String,
    /// From the "excluded" file (no longer listed).
    inactive: bool,
}

#[derive(Debug)]
struct NdcPackage {
    /// 10-digit package code, hyphenated (`0777-3105-02`).
    code: String,
    /// Its product code (`0777-3105`).
    product: String,
    description: String,
    inactive: bool,
}

/// The 11-digit (5-4-2) form of a hyphenated 10-digit NDC package code:
/// the short segment is zero-padded (4-4-2 → `0`+labeler, 5-3-2 → `0`+product,
/// 5-4-1 → `0`+package). `None` for anything else.
pub fn ndc_11_digit(code: &str) -> Option<String> {
    let parts: Vec<&str> = code.split('-').collect();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let (l, p, k) = (parts[0], parts[1], parts[2]);
    match (l.len(), p.len(), k.len()) {
        (4, 4, 2) => Some(format!("0{l}{p}{k}")),
        (5, 3, 2) => Some(format!("{l}0{p}{k}")),
        (5, 4, 1) => Some(format!("{l}{p}0{k}")),
        (5, 4, 2) => Some(format!("{l}{p}{k}")),
        _ => None,
    }
}

struct ColIdx {
    productndc: usize,
    proprietaryname: usize,
    nonproprietaryname: usize,
    producttypename: usize,
    ndc_exclude_flag: usize,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Import the FDA NDC Directory (`product.txt`) through the given backend.
pub async fn import_ndc(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    path: &Path,
    batch_size: usize,
    dry_run: bool,
) -> Result<ImportStats, HtsError> {
    let batch_size = batch_size.max(1);

    let path_owned = path.to_path_buf();
    #[allow(clippy::type_complexity)]
    let (products, packages, errors) = tokio::task::spawn_blocking(
        move || -> Result<(Vec<NdcProduct>, Vec<NdcPackage>, Vec<String>), HtsError> {
            let files = read_ndc_files(&path_owned)?;
            let product_txt = files.get("product.txt").ok_or_else(|| {
                HtsError::InvalidRequest(format!(
                    "No 'product.txt' found in '{}'",
                    path_owned.display()
                ))
            })?;
            let (mut products, mut errors) = parse_product_txt(product_txt)?;
            let mut seen: std::collections::HashSet<String> =
                products.iter().map(|p| p.code.clone()).collect();
            if let Some(text) = files.get("products_excluded.txt") {
                let (excluded, errs) = parse_products(text, true)?;
                errors.extend(errs);
                products.extend(excluded.into_iter().filter(|p| seen.insert(p.code.clone())));
            }
            let mut packages = Vec::new();
            let mut seen_pkg: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (name, inactive) in [("package.txt", false), ("packages_excluded.txt", true)] {
                if let Some(text) = files.get(name) {
                    let (pkgs, errs) = parse_package_txt(text, inactive)?;
                    errors.extend(errs);
                    packages.extend(pkgs.into_iter().filter(|p| seen_pkg.insert(p.code.clone())));
                }
            }
            Ok((products, packages, errors))
        },
    )
    .await
    .map_err(|e| HtsError::Internal(format!("NDC parser panicked: {e}")))??;

    let mut stats = ImportStats {
        errors,
        ..Default::default()
    };

    if dry_run {
        stats.code_systems = 1;
        stats.concepts = (products.len() + 2 * packages.len()) as u32;
        eprintln!(
            "[ndc] dry-run — {} products parsed, no DB writes",
            products.len()
        );
        return Ok(stats);
    }

    let meta = CodeSystemMeta {
        id: NDC_ID,
        url: NDC_URL,
        version: Some(NDC_VERSION),
        name: Some(NDC_NAME),
        title: Some(NDC_TITLE),
        status: "active",
        content: "complete",
    };

    // Collect unique product-type grouper names.
    let product_types: BTreeSet<String> = products
        .iter()
        .filter(|p| !p.product_type.is_empty())
        .map(|p| p.product_type.clone())
        .collect();

    // Seed bundle: virtual root + grouper concepts.
    let root = BuilderConcept {
        code: ROOT_CODE,
        display: Some(NDC_TITLE),
        definition: Some("header"),
        ..Default::default()
    };
    let mut seed_concepts: Vec<BuilderConcept<'_>> = Vec::with_capacity(1 + product_types.len());
    seed_concepts.push(root);
    for pt in &product_types {
        seed_concepts.push(BuilderConcept {
            code: pt.as_str(),
            display: Some(pt.as_str()),
            definition: Some("header"),
            parent_code: Some(ROOT_CODE),
            ..Default::default()
        });
    }
    let seed = build_native_batch(&meta, &seed_concepts);
    let seed_stats = backend.import_parsed(ctx, seed).await?;
    stats.code_systems = seed_stats.code_systems;
    stats.errors.extend(seed_stats.errors);

    let total = products.len();
    let total_batches = total.div_ceil(batch_size).max(1);

    const INACTIVE: &[BuilderProperty<'static>] = &[BuilderProperty {
        code: "status",
        value_key: "valueCode",
        value: "inactive",
    }];
    let status = |inactive: bool| if inactive { INACTIVE } else { &[] };

    for (batch_idx, batch) in products.chunks(batch_size).enumerate() {
        let builder: Vec<BuilderConcept<'_>> = batch
            .iter()
            .map(|p| BuilderConcept {
                code: &p.code,
                display: Some(p.display.as_str()).filter(|s| !s.is_empty()),
                parent_code: Some(if p.product_type.is_empty() {
                    ROOT_CODE
                } else {
                    p.product_type.as_str()
                }),
                extra_properties: status(p.inactive),
                ..Default::default()
            })
            .collect();

        let bytes = build_native_batch(&meta, &builder);
        let chunk = backend.import_parsed(ctx, bytes).await?;
        stats.errors.extend(chunk.errors);

        eprintln!(
            "[ndc] batch {}/{total_batches} — +{} products (total: {})",
            batch_idx + 1,
            batch.len(),
            ((batch_idx + 1) * batch_size).min(total)
        );
    }

    // Packages under their product (or the root when the product is not
    // listed), then the 11-digit form of each under its package.
    let product_display: std::collections::HashMap<&str, &str> = products
        .iter()
        .map(|p| (p.code.as_str(), p.display.as_str()))
        .collect();
    struct Row {
        code: String,
        display: String,
        parent: String,
        inactive: bool,
    }
    let mut rows: Vec<Row> = Vec::with_capacity(packages.len() * 2);
    for pkg in &packages {
        let parent = if product_display.contains_key(pkg.product.as_str()) {
            pkg.product.clone()
        } else {
            ROOT_CODE.to_string()
        };
        let display = match product_display.get(pkg.product.as_str()) {
            Some(d) if !d.is_empty() => format!("{d}, {}", pkg.description),
            _ => pkg.description.clone(),
        };
        if let Some(eleven) = ndc_11_digit(&pkg.code) {
            rows.push(Row {
                code: pkg.code.clone(),
                display: display.clone(),
                parent,
                inactive: pkg.inactive,
            });
            rows.push(Row {
                code: eleven,
                display,
                parent: pkg.code.clone(),
                inactive: pkg.inactive,
            });
        } else {
            rows.push(Row {
                code: pkg.code.clone(),
                display,
                parent,
                inactive: pkg.inactive,
            });
        }
    }
    let total_rows = rows.len();
    for batch in rows.chunks(batch_size) {
        let builder: Vec<BuilderConcept<'_>> = batch
            .iter()
            .map(|r| BuilderConcept {
                code: &r.code,
                display: Some(r.display.as_str()).filter(|s| !s.is_empty()),
                parent_code: Some(r.parent.as_str()),
                extra_properties: status(r.inactive),
                ..Default::default()
            })
            .collect();
        let parsed = build_native_batch(&meta, &builder);
        let chunk = backend.import_parsed(ctx, parsed).await?;
        stats.errors.extend(chunk.errors);
    }
    eprintln!(
        "[ndc] {} package codes (+ 11-digit forms): {total_rows} concepts",
        packages.len()
    );

    stats.concepts = (total + total_rows) as u32;
    Ok(stats)
}

/// The NDC text files in `path`: a ZIP with `product.txt` (and optionally
/// `package.txt`, `Products_excluded.txt`, `Packages_excluded.txt`), or a
/// bare `product.txt`. Keys are lower-case file names.
fn read_ndc_files(path: &Path) -> Result<std::collections::HashMap<String, String>, HtsError> {
    let mut out = std::collections::HashMap::new();
    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        out.insert("product.txt".to_string(), read_product_txt(path)?);
        return Ok(out);
    }
    let file = std::fs::File::open(path).map_err(|e| {
        HtsError::InvalidRequest(format!("Cannot open ZIP '{}': {e}", path.display()))
    })?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| HtsError::InvalidRequest(format!("Invalid ZIP '{}': {e}", path.display())))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| HtsError::InvalidRequest(format!("Cannot read ZIP entry: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| HtsError::InvalidRequest(format!("Cannot read ZIP entry name: {e}")))?
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "product.txt" | "package.txt" | "products_excluded.txt" | "packages_excluded.txt"
        ) {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| HtsError::InvalidRequest(format!("Cannot read {name} from ZIP: {e}")))?;
        out.insert(name, decode_ndc_text(bytes));
    }
    Ok(out)
}

/// Parse `package.txt` / `Packages_excluded.txt`.
fn parse_package_txt(
    text: &str,
    inactive: bool,
) -> Result<(Vec<NdcPackage>, Vec<String>), HtsError> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| HtsError::InvalidRequest("package file is empty".to_string()))?;
    let cols: Vec<&str> = header.split('\t').map(str::trim).collect();
    let find = |name: &str| {
        cols.iter()
            .position(|h| h.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                HtsError::InvalidRequest(format!("package file header is missing column '{name}'"))
            })
    };
    let (i_product, i_package, i_desc) = (
        find("PRODUCTNDC")?,
        find("NDCPACKAGECODE")?,
        find("PACKAGEDESCRIPTION")?,
    );
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (n, line) in lines.enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let get = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
        let code = get(i_package);
        if code.is_empty() {
            errors.push(format!(
                "package line {}: missing NDCPACKAGECODE — skipped",
                n + 2
            ));
            continue;
        }
        out.push(NdcPackage {
            code: code.to_string(),
            product: get(i_product).to_string(),
            description: get(i_desc).to_string(),
            inactive,
        });
    }
    Ok((out, errors))
}

// ── File reader ───────────────────────────────────────────────────────────────

fn read_product_txt(path: &Path) -> Result<String, HtsError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "zip" {
        read_product_txt_from_zip(path)
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| HtsError::InvalidRequest(format!("Cannot read '{}': {e}", path.display())))
    }
}

fn read_product_txt_from_zip(path: &Path) -> Result<String, HtsError> {
    let file = std::fs::File::open(path).map_err(|e| {
        HtsError::InvalidRequest(format!("Cannot open ZIP '{}': {e}", path.display()))
    })?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| HtsError::InvalidRequest(format!("Invalid ZIP '{}': {e}", path.display())))?;

    let idx = (0..archive.len())
        .find(|&i| {
            archive
                .by_index(i)
                .ok()
                .map(|e| {
                    let n = e.name().map(|n| n.to_ascii_lowercase()).unwrap_or_default();
                    n == "product.txt" || n.ends_with("/product.txt")
                })
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            HtsError::InvalidRequest(format!(
                "No 'product.txt' found inside ZIP '{}'",
                path.display()
            ))
        })?;

    let mut entry = archive
        .by_index(idx)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read ZIP entry: {e}")))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read product.txt from ZIP: {e}")))?;
    Ok(decode_ndc_text(bytes))
}

/// The FDA NDC distribution ships `product.txt` in Windows-1252 / Latin-1, not
/// UTF-8 (drug names contain bytes such as `0xBF`), so a strict UTF-8 read
/// fails outright. Try UTF-8 first (for any re-encoded copy) and fall back to a
/// byte-wise Latin-1 → UTF-8 decode, which is lossless for every Latin-1
/// codepoint. Mirrors `icd9_cm::decode_cms_text`.
fn decode_ndc_text(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => e.into_bytes().into_iter().map(char::from).collect(),
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

fn resolve_columns(header: &str) -> Result<ColIdx, HtsError> {
    let cols: Vec<&str> = header.split('\t').collect();
    let find = |name: &str| -> Result<usize, HtsError> {
        cols.iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                HtsError::InvalidRequest(format!(
                    "product.txt header is missing required column '{name}'"
                ))
            })
    };

    Ok(ColIdx {
        productndc: find("PRODUCTNDC")?,
        proprietaryname: find("PROPRIETARYNAME")?,
        nonproprietaryname: find("NONPROPRIETARYNAME")?,
        producttypename: find("PRODUCTTYPENAME")?,
        ndc_exclude_flag: find("NDC_EXCLUDE_FLAG")?,
    })
}

fn parse_product_txt(text: &str) -> Result<(Vec<NdcProduct>, Vec<String>), HtsError> {
    parse_products(text, false)
}

/// Parse a product file; `excluded` = FDA's `Products_excluded.txt` (every
/// row kept, marked inactive) instead of `product.txt` (flag `E` skipped).
fn parse_products(text: &str, excluded: bool) -> Result<(Vec<NdcProduct>, Vec<String>), HtsError> {
    let mut lines = BufReader::new(text.as_bytes()).lines().enumerate();

    let (_, first) = lines
        .next()
        .ok_or_else(|| HtsError::InvalidRequest("product.txt is empty".to_string()))?;
    let header =
        first.map_err(|e| HtsError::InvalidRequest(format!("Cannot read header row: {e}")))?;
    let idx = resolve_columns(&header)?;

    let mut products = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (line_num, line_result) in lines {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }

        let cols: Vec<&str> = line.split('\t').collect();

        if !excluded
            && cols
                .get(idx.ndc_exclude_flag)
                .map(|s| s.trim())
                .unwrap_or("")
                == "E"
        {
            continue;
        }

        let ndc = match cols.get(idx.productndc).map(|s| s.trim()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                errors.push(format!(
                    "line {}: missing PRODUCTNDC — skipped",
                    line_num + 1
                ));
                continue;
            }
        };

        let brand = cols
            .get(idx.proprietaryname)
            .map(|s| s.trim())
            .unwrap_or("")
            .to_string();
        let generic = cols
            .get(idx.nonproprietaryname)
            .map(|s| s.trim())
            .unwrap_or("")
            .to_string();

        let display = match (brand.is_empty(), generic.is_empty()) {
            (false, false) => format!("{brand} ({generic})"),
            (false, true) => brand,
            (true, false) => generic.clone(),
            (true, true) => {
                errors.push(format!(
                    "line {}: NDC {ndc} has no brand or generic name — imported with empty display",
                    line_num + 1
                ));
                String::new()
            }
        };

        let product_type = cols
            .get(idx.producttypename)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        products.push(NdcProduct {
            code: ndc,
            display,
            product_type,
            inactive: excluded,
        });
    }

    Ok((products, errors))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> &'static str {
        "PRODUCTID\tPRODUCTNDC\tPRODUCTTYPENAME\tPROPRIETARYNAME\tPROPRIETARYNAMESUFFIX\t\
         NONPROPRIETARYNAME\tDOSAGEFORMNAME\tROUTENAME\tSTARTMARKETINGDATE\tENDMARKETINGDATE\t\
         MARKETINGCATEGORYNAME\tAPPLICATIONNUMBER\tLABELERNAME\tSUBSTANCENAME\t\
         ACTIVE_NUMERATOR_STRENGTH\tACTIVE_INGRED_UNIT\tPHARM_CLASSES\tDEASCHEDULE\t\
         NDC_EXCLUDE_FLAG\tLISTINGRECORDCERTIFIEDTHROUGH"
    }

    #[test]
    fn resolve_columns_finds_standard_header() {
        let idx = resolve_columns(sample_header()).unwrap();
        assert_eq!(idx.productndc, 1);
        assert_eq!(idx.producttypename, 2);
        assert_eq!(idx.proprietaryname, 3);
        assert_eq!(idx.nonproprietaryname, 5);
        assert_eq!(idx.ndc_exclude_flag, 18);
    }

    #[test]
    fn resolve_columns_is_case_insensitive() {
        let header = "productid\tproductndc\tproducttypename\tproprietaryname\tproprietarynamesuffix\t\
                      nonproprietaryname\tdosageformname\troutename\tstartmarketingdate\tendmarketingdate\t\
                      marketingcategoryname\tapplicationnumber\tlabelername\tsubstancename\t\
                      active_numerator_strength\tactive_ingred_unit\tpharm_classes\tdeaschedule\t\
                      ndc_exclude_flag\tlistingrecordcertifiedthrough";
        let idx = resolve_columns(header).unwrap();
        assert_eq!(idx.productndc, 1);
    }

    #[test]
    fn resolve_columns_missing_required_column_returns_error() {
        let header = "PRODUCTID\tPRODUCTNDC\tPROPRIETARYNAME";
        let result = resolve_columns(header);
        assert!(result.is_err());
    }

    #[test]
    fn decode_ndc_text_handles_latin1_bytes() {
        // 0xBF is the exact byte that breaks a strict UTF-8 read of the real
        // FDA product.txt (it precedes "Baryta muriatica"). Latin-1 maps the
        // byte 0xBF to U+00BF ('¿'), losslessly.
        let bytes = b"DRUG\t\xbfBaryta muriatica".to_vec();
        assert_eq!(decode_ndc_text(bytes), "DRUG\t\u{00BF}Baryta muriatica");
    }

    #[test]
    fn decode_ndc_text_passes_through_valid_utf8() {
        let bytes = "Prozac® 20mg".as_bytes().to_vec();
        assert_eq!(decode_ndc_text(bytes), "Prozac® 20mg");
    }

    fn sample_product_txt() -> String {
        format!(
            "{header}\n\
             0_0002-1200_001\t0002-1200\tHUMAN PRESCRIPTION DRUG\tProzac\t\tfluoxetine hydrochloride\tCAPSULE\tORAL\t19870101\t\tNDA\tNDA018936\tEli Lilly\tfluoxetine hydrochloride\t10\tmg/1\t\t\t\t\n\
             0_0002-3233_001\t0002-3233\tHUMAN PRESCRIPTION DRUG\tHumulin N\t\tinsulin isophane\tINJECTION\tSUBCUTANEOUS\t19821001\t\tNDA\tNDA018781\tEli Lilly\tinsulin isophane, human\t100\t[iU]/mL\t\t\t\t\n\
             0_0067-2000_001\t0067-2000\tHUMAN OTC DRUG\tTylenol\t\tacetaminophen\tTABLET\tORAL\t19800101\t\tOTC monograph final\t\tJohnson & Johnson\tacetaminophen\t500\tmg/1\t\t\t\t\n\
             0_9999-0001_001\t9999-0001\tHUMAN PRESCRIPTION DRUG\tExcluded Drug\t\texcluded\tCAPSULE\tORAL\t20000101\t\tNDA\tNDA999999\tAcme\texcluded\t10\tmg/1\t\t\tE\t\n",
            header = sample_header()
        )
    }

    #[test]
    fn parse_skips_excluded_products() {
        let (products, errors) = parse_product_txt(&sample_product_txt()).unwrap();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(products.len(), 3);
        assert!(!products.iter().any(|p| p.code == "9999-0001"));
    }

    #[test]
    fn parse_display_combines_brand_and_generic() {
        let (products, _) = parse_product_txt(&sample_product_txt()).unwrap();
        let prozac = products.iter().find(|p| p.code == "0002-1200").unwrap();
        assert_eq!(prozac.display, "Prozac (fluoxetine hydrochloride)");
    }

    #[test]
    fn parse_product_type_grouper_is_populated() {
        let (products, _) = parse_product_txt(&sample_product_txt()).unwrap();
        let otc = products.iter().find(|p| p.code == "0067-2000").unwrap();
        assert_eq!(otc.product_type, "HUMAN OTC DRUG");
    }

    #[test]
    fn eleven_digit_forms_pad_the_short_segment() {
        assert_eq!(ndc_11_digit("0777-3105-02").as_deref(), Some("00777310502"));
        assert_eq!(ndc_11_digit("49281-121-65").as_deref(), Some("49281012165"));
        assert_eq!(ndc_11_digit("12345-6789-1").as_deref(), Some("12345678901"));
        assert_eq!(
            ndc_11_digit("0777-3105"),
            None,
            "product codes have no 11-digit form"
        );
        assert_eq!(ndc_11_digit("07773105-02"), None);
    }

    #[test]
    fn package_file_parses_codes_products_and_descriptions() {
        let text = "PRODUCTID\tPRODUCTNDC\tNDCPACKAGECODE\tPACKAGEDESCRIPTION\tSTARTMARKETINGDATE\tENDMARKETINGDATE\tNDC_EXCLUDE_FLAG\tSAMPLE_PACKAGE\n\
                    0777-3105_x\t0777-3105\t0777-3105-02\t100 CAPSULE in 1 BOTTLE (0777-3105-02) \t19880201\t\tN\tN\n";
        let (pkgs, errors) = parse_package_txt(text, true).unwrap();
        assert!(errors.is_empty());
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].code, "0777-3105-02");
        assert_eq!(pkgs[0].product, "0777-3105");
        assert!(pkgs[0].inactive);
    }

    #[test]
    fn parse_empty_input_returns_error() {
        let result = parse_product_txt("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_required_column_returns_error() {
        let bad = "PRODUCTID\tPRODUCTNDC\n0001\t1234-5678\n";
        let result = parse_product_txt(bad);
        assert!(result.is_err());
    }

    #[cfg(feature = "sqlite")]
    mod integration {
        use super::*;
        use crate::backends::SqliteTerminologyBackend;
        use std::io::Write;

        fn count(backend: &SqliteTerminologyBackend, table: &str) -> i64 {
            backend
                .pool()
                .get()
                .unwrap()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        }

        fn make_txt_file() -> tempfile::NamedTempFile {
            let mut f = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
            f.write_all(sample_product_txt().as_bytes()).unwrap();
            f
        }

        fn make_zip_file() -> tempfile::NamedTempFile {
            let tmp = tempfile::NamedTempFile::with_suffix(".zip").unwrap();
            {
                let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
                zip.start_file("product.txt", zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(sample_product_txt().as_bytes()).unwrap();
                zip.finish().unwrap();
            }
            tmp
        }

        #[tokio::test]
        async fn dry_run_does_not_write() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_txt_file();
            let stats = import_ndc(&backend, &ctx, f.path(), 500, true)
                .await
                .unwrap();
            assert_eq!(stats.code_systems, 1);
            assert_eq!(stats.concepts, 3);
            assert_eq!(count(&backend, "code_systems"), 0);
            assert_eq!(count(&backend, "concepts"), 0);
        }

        #[tokio::test]
        async fn live_import_writes_concepts_and_hierarchy() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_txt_file();
            let stats = import_ndc(&backend, &ctx, f.path(), 500, false)
                .await
                .unwrap();
            assert_eq!(stats.code_systems, 1);
            assert_eq!(stats.concepts, 3);
            assert_eq!(count(&backend, "concepts"), 6);
            assert_eq!(count(&backend, "concept_hierarchy"), 5);
        }

        #[tokio::test]
        async fn idempotent_reimport() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_txt_file();
            import_ndc(&backend, &ctx, f.path(), 500, false)
                .await
                .unwrap();
            import_ndc(&backend, &ctx, f.path(), 500, false)
                .await
                .unwrap();
            assert_eq!(count(&backend, "code_systems"), 1);
            assert_eq!(count(&backend, "concepts"), 6);
            assert_eq!(count(&backend, "concept_hierarchy"), 5);
        }

        #[tokio::test]
        async fn import_from_zip() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_zip_file();
            let stats = import_ndc(&backend, &ctx, f.path(), 500, false)
                .await
                .unwrap();
            assert_eq!(stats.concepts, 3);
            assert_eq!(count(&backend, "concepts"), 6);
        }

        #[tokio::test]
        async fn batching_preserves_all_concepts() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_txt_file();
            let stats = import_ndc(&backend, &ctx, f.path(), 1, false)
                .await
                .unwrap();
            assert_eq!(stats.concepts, 3);
            assert_eq!(count(&backend, "concepts"), 6);
        }

        #[tokio::test]
        async fn missing_file_returns_error() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let result = import_ndc(
                &backend,
                &ctx,
                Path::new("/nonexistent/product.txt"),
                500,
                false,
            )
            .await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn lookup_known_code() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_txt_file();
            import_ndc(&backend, &ctx, f.path(), 500, false)
                .await
                .unwrap();

            let conn = backend.pool().get().unwrap();
            let display: String = conn
                .query_row(
                    "SELECT display FROM concepts WHERE code = ?1",
                    rusqlite::params!["0002-1200"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(display, "Prozac (fluoxetine hydrochloride)");
        }

        #[tokio::test]
        async fn excluded_product_not_in_db() {
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            let f = make_txt_file();
            import_ndc(&backend, &ctx, f.path(), 500, false)
                .await
                .unwrap();

            let conn = backend.pool().get().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM concepts WHERE code = ?1",
                    rusqlite::params!["9999-0001"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
        }

        /// Packages, their 11-digit forms and FDA's excluded listings
        /// (inactive) come in with the products.
        #[tokio::test]
        async fn packages_eleven_digit_forms_and_excluded_listings() {
            use crate::traits::CodeSystemOperations;
            use crate::types::ValidateCodeRequest;
            let pkg_header = "PRODUCTID\tPRODUCTNDC\tNDCPACKAGECODE\tPACKAGEDESCRIPTION\tSTARTMARKETINGDATE\tENDMARKETINGDATE\tNDC_EXCLUDE_FLAG\tSAMPLE_PACKAGE";
            let package = format!(
                "{pkg_header}\n0_0002-1200_001\t0002-1200\t0002-1200-30\t30 CAPSULE in 1 BOTTLE (0002-1200-30) \t19870101\t\tN\tN\n"
            );
            let products_excluded = format!(
                "{header}\n49281-121_x\t49281-121\tVACCINE\tFluzone\t\tinfluenza vaccine\tINJECTION\tIM\t20210702\t20231206\tBLA\tBLA1\tSanofi\tflu\t\t\t\t\tD\t\n",
                header = sample_header()
            );
            let packages_excluded = format!(
                "{pkg_header}\n49281-121_x\t49281-121\t49281-121-65\t10 SYRINGE, GLASS in 1 PACKAGE (49281-121-65) \t20210702\t20231206\tD\tN\n"
            );
            let tmp = tempfile::NamedTempFile::with_suffix(".zip").unwrap();
            {
                let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
                for (name, text) in [
                    ("product.txt", sample_product_txt()),
                    ("package.txt", package),
                    ("Products_excluded.txt", products_excluded),
                    ("Packages_excluded.txt", packages_excluded),
                ] {
                    zip.start_file(name, zip::write::SimpleFileOptions::default())
                        .unwrap();
                    zip.write_all(text.as_bytes()).unwrap();
                }
                zip.finish().unwrap();
            }
            let backend = SqliteTerminologyBackend::in_memory().unwrap();
            let ctx = TenantContext::system();
            import_ndc(&backend, &ctx, tmp.path(), 2, false)
                .await
                .unwrap();
            for (code, active) in [
                ("0002-1200-30", true),
                ("00002120030", true),
                ("49281-121-65", false),
                ("49281012165", false),
                ("49281-121", false),
            ] {
                let r = CodeSystemOperations::validate_code(
                    &backend,
                    &ctx,
                    ValidateCodeRequest {
                        system: Some(NDC_URL.into()),
                        code: code.into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                assert!(r.result, "{code}: {r:?}");
                let conn = backend.pool().get().unwrap();
                let flagged: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM concept_properties cp JOIN concepts c ON c.id = cp.concept_id
                          WHERE c.code = ?1 AND cp.property = 'status' AND cp.value = 'inactive'",
                        [code],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(flagged == 0, active, "{code}");
            }
            let conn = backend.pool().get().unwrap();
            let parent: String = conn
                .query_row(
                    "SELECT parent_code FROM concept_hierarchy WHERE child_code = '00002120030'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(parent, "0002-1200-30");
        }
    }
}
