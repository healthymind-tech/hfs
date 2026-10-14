//! Format → importer dispatch shared by the `hts import` CLI and the admin
//! import jobs ([`crate::admin`]).
//!
//! This used to live in `main.rs`, which made it unreachable from the library:
//! the background job runner has to call *exactly* the importers the CLI calls
//! (same parsing, same batching, same finalisation contract), so the table
//! moved here and `main.rs` now calls it too. Adding a format means adding an
//! [`ImportFormat`] variant and one arm below.

use std::path::Path;

use helios_persistence::tenant::TenantContext;

use crate::config::ImportFormat;
use crate::error::HtsError;
use crate::import::{BundleImportBackend, ImportStats, LanguageFilter};

/// Run the importer for `format` on `path`.
pub async fn dispatch_import(
    format: ImportFormat,
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    path: &Path,
    batch_size: usize,
    dry_run: bool,
    languages: &LanguageFilter,
) -> Result<ImportStats, HtsError> {
    use crate::import::{
        dicom::import_dicom, hl7_v2_tables::import_hl7_v2_tables, icd9_cm::import_icd9_cm,
        icd10_cm::import_icd10_cm, icd10_pcs::import_icd10_pcs, loinc_csv::import_loinc_csv,
        mesh::import_mesh, nci_thesaurus::import_nci_thesaurus, ndc::import_ndc, nucc::import_nucc,
        rxnorm_rrf::import_rxnorm_rrf, snomed_rf2::import_snomed_rf2, tgz::import_tgz,
        ucum::import_ucum,
    };

    match format {
        ImportFormat::Hl7Npm => import_tgz(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::SnomedRf2 => {
            import_snomed_rf2(backend, ctx, path, batch_size, dry_run, languages).await
        }
        ImportFormat::Loinc => {
            import_loinc_csv(backend, ctx, path, batch_size, dry_run, languages).await
        }
        ImportFormat::Icd10Cm => import_icd10_cm(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Icd10Pcs => import_icd10_pcs(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Icd9Cm => import_icd9_cm(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Rxnorm => import_rxnorm_rrf(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Ucum => import_ucum(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::NciThesaurus => {
            import_nci_thesaurus(backend, ctx, path, batch_size, dry_run).await
        }
        ImportFormat::Mesh => import_mesh(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Dicom => import_dicom(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Hl7V2Tables => {
            import_hl7_v2_tables(backend, ctx, path, batch_size, dry_run).await
        }
        ImportFormat::Nucc => import_nucc(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::Ndc => import_ndc(backend, ctx, path, batch_size, dry_run).await,
        ImportFormat::FhirBundle => import_fhir_bundle_file(backend, ctx, path, dry_run).await,
    }
}

/// Import a FHIR Bundle JSON file (whole-file read; Bundles are small).
pub async fn import_fhir_bundle_file(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    path: &Path,
    dry_run: bool,
) -> Result<ImportStats, HtsError> {
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read '{}': {e}", path.display())))?;

    if dry_run {
        let parsed = crate::import::bundle_parser::parse_bundle(&data)?;
        let concepts: usize = parsed.code_systems.iter().map(|cs| cs.concepts.len()).sum();
        return Ok(ImportStats {
            code_systems: parsed.code_systems.len() as u32,
            value_sets: parsed.value_sets.len() as u32,
            concept_maps: parsed.concept_maps.len() as u32,
            concepts: concepts as u32,
            ..Default::default()
        });
    }

    backend.import_bundle(ctx, &data).await
}
