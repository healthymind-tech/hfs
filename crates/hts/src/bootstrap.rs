//! Terminology directory sync shared by the `hts` binary and the embedded
//! terminology service of `hfs` (design `docs/design/unified-server.md` U2):
//! `HTS_BOOTSTRAP_DIR` / `HFS_TERMINOLOGY_BOOTSTRAP_DIR` import with the
//! `bootstrap_imports` ledger, and the directory import loop of `hts import`.

use crate::config::{HtsConfig, ImportFormat, detect_format};
use crate::import::dispatch::dispatch_import;
use crate::import::{BundleImportBackend, ImportStats, LanguageFilter};
use helios_persistence::tenant::TenantContext;
use tracing::info;

#[cfg(feature = "postgres")]
use crate::backends::PostgresTerminologyBackend;
#[cfg(feature = "sqlite")]
use crate::backends::SqliteTerminologyBackend;

// ── Bootstrap ─────────────────────────────────────────────────────────────────

/// If `bootstrap_dir` is non-empty and points at an existing directory,
/// synchronize its contents into the connected database: every recognized file
/// is hashed (SHA-256) and imported only when its hash is absent from, or
/// differs from, the `bootstrap_imports` ledger. Unchanged files are skipped
/// without re-parsing. Otherwise log and no-op.
///
/// Unlike the previous first-run-only behavior, this runs on every startup, so
/// files added to the directory after the first boot — and updated terminology
/// releases that replace an existing file — are picked up automatically. The
/// underlying import is idempotent (upsert keyed on canonical `(url, version)`),
/// so re-importing a changed file refreshes it in place.
///
/// Triggered by `HTS_BOOTSTRAP_DIR`; the Docker image sets this to
/// `/app/terminology-data` so first `docker run` yields a populated server.
#[cfg(feature = "sqlite")]
pub async fn bootstrap_sync_sqlite(
    backend: &SqliteTerminologyBackend,
    config: &HtsConfig,
) -> anyhow::Result<()> {
    let Some(dir) = resolve_bootstrap_dir(&config.bootstrap_dir) else {
        return Ok(());
    };
    let tracker = BootstrapTracker::Sqlite(backend);
    bootstrap_sync(&tracker, backend, &dir, config).await
}

/// Runs the Postgres bootstrap sync under a session-scoped advisory lock
/// ([`crate::backends::postgres::schema::with_bootstrap_lock`]) so that
/// N instances cold-starting concurrently against one empty (or
/// partially-populated) database don't each redundantly run the same heavy
/// SNOMED/LOINC/RxNorm/ICD-10-CM import.
///
/// The lock covers the whole `bootstrap_sync` call (every file in the
/// directory, not per-file) — the per-file ledger check inside
/// `bootstrap_sync` still runs normally, so an instance that queues behind
/// the lock and then acquires it naturally skips any files the winner
/// already imported.
#[cfg(feature = "postgres")]
pub async fn bootstrap_sync_postgres(
    backend: &PostgresTerminologyBackend,
    config: &HtsConfig,
) -> anyhow::Result<()> {
    let Some(dir) = resolve_bootstrap_dir(&config.bootstrap_dir) else {
        return Ok(());
    };

    let tracker = BootstrapTracker::Postgres(backend);
    crate::backends::postgres::schema::with_bootstrap_lock(backend.pool(), || {
        bootstrap_sync(&tracker, backend, &dir, config)
    })
    .await
}

/// Backend-agnostic bootstrap sync: import the directory, gating each file on
/// its content hash via `tracker`.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub async fn bootstrap_sync(
    tracker: &BootstrapTracker<'_>,
    backend: &dyn BundleImportBackend,
    dir: &std::path::Path,
    config: &HtsConfig,
) -> anyhow::Result<()> {
    let batch_size = config.bootstrap_batch_size;
    let languages = LanguageFilter::parse(&config.import_languages);
    info!(
        bootstrap_dir = %dir.display(),
        batch_size,
        import_languages = %languages.canonical_spec(),
        "bootstrap: syncing terminology directory"
    );
    let ctx = TenantContext::system();
    let (stats, imported, skipped) = import_directory_files(
        backend,
        &ctx,
        dir,
        batch_size,
        false,
        &languages,
        Some(tracker),
    )
    .await
    .map_err(|e| anyhow::anyhow!("bootstrap import failed: {e}"))?;
    info!(
        imported_files = imported,
        skipped_files = skipped,
        code_systems = stats.code_systems,
        value_sets = stats.value_sets,
        concept_maps = stats.concept_maps,
        concepts = stats.concepts,
        "bootstrap sync complete"
    );
    // Mirrors what print_import_summary already does for the CLI path (see
    // below): a bootstrap file that partially fails still reports overall
    // success above, so surface the non-fatal errors here too instead of
    // discarding them — that silence is exactly how issue #802 went
    // unnoticed (14,567 "no separator" errors, zero concepts, clean logs).
    if !stats.errors.is_empty() {
        tracing::warn!(
            error_count = stats.errors.len(),
            "bootstrap sync completed with non-fatal errors"
        );
        for e in stats.errors.iter().take(10) {
            tracing::warn!(error = %e, "bootstrap import error");
        }
    }
    Ok(())
}

/// Parse `HTS_BOOTSTRAP_DIR`: empty string → no bootstrap, non-existent path
/// → warn + no bootstrap, otherwise return the resolved path.
pub fn resolve_bootstrap_dir(bootstrap_dir: &str) -> Option<std::path::PathBuf> {
    if bootstrap_dir.is_empty() {
        return None;
    }
    let dir = std::path::PathBuf::from(bootstrap_dir);
    if !dir.is_dir() {
        tracing::warn!(
            bootstrap_dir,
            "HTS_BOOTSTRAP_DIR is set but does not point at an existing directory; skipping"
        );
        return None;
    }
    Some(dir)
}

/// Ledger over the `bootstrap_imports` table, used by [`import_directory_files`]
/// to skip files that have already been imported with identical content.
///
/// Backend-specific because the SQLite pool is blocking (run on a
/// `spawn_blocking` worker) while the PostgreSQL pool is async.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub enum BootstrapTracker<'a> {
    #[cfg(feature = "sqlite")]
    Sqlite(&'a SqliteTerminologyBackend),
    #[cfg(feature = "postgres")]
    Postgres(&'a PostgresTerminologyBackend),
}

/// A previously-recorded bootstrap ledger row.
///
/// `mtime_unix` is `None` for rows written by an older HTS that predates the
/// column, forcing a content-hash comparison on first boot after upgrade.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub struct LedgerRecord {
    content_hash: String,
    size_bytes: i64,
    mtime_unix: Option<i64>,
    languages: String,
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
impl BootstrapTracker<'_> {
    /// The full ledger row previously recorded for `path` (file name), if any.
    async fn imported_record(&self, path: &str) -> anyhow::Result<Option<LedgerRecord>> {
        match self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite(backend) => {
                use rusqlite::OptionalExtension;
                let pool = backend.pool().clone();
                let key = path.to_string();
                let rec =
                    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<LedgerRecord>> {
                        let conn = pool.get()?;
                        let r = conn
                            .query_row(
                                "SELECT content_hash, size_bytes, mtime_unix, languages \
                                 FROM bootstrap_imports WHERE path = ?1",
                                [&key],
                                |r| {
                                    Ok(LedgerRecord {
                                        content_hash: r.get(0)?,
                                        size_bytes: r.get(1)?,
                                        mtime_unix: r.get(2)?,
                                        languages: r.get(3)?,
                                    })
                                },
                            )
                            .optional()?;
                        Ok(r)
                    })
                    .await??;
                Ok(rec)
            }
            #[cfg(feature = "postgres")]
            Self::Postgres(backend) => {
                let client = backend.pool().get().await?;
                let row = client
                    .query_opt(
                        "SELECT content_hash, size_bytes, mtime_unix, languages \
                         FROM bootstrap_imports WHERE path = $1",
                        &[&path],
                    )
                    .await?;
                Ok(row.map(|r| LedgerRecord {
                    content_hash: r.get(0),
                    size_bytes: r.get(1),
                    mtime_unix: r.get(2),
                    languages: r.get(3),
                }))
            }
        }
    }

    /// Record (insert or update) the content hash, size, mtime and applied
    /// language filter for `path`.
    async fn record(
        &self,
        path: &str,
        hash: &str,
        size: i64,
        mtime_unix: Option<i64>,
        languages: &str,
    ) -> anyhow::Result<()> {
        match self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite(backend) => {
                let pool = backend.pool().clone();
                let (key, hash, languages) =
                    (path.to_string(), hash.to_string(), languages.to_string());
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = pool.get()?;
                    conn.execute(
                        "INSERT INTO bootstrap_imports \
                             (path, content_hash, size_bytes, mtime_unix, languages) \
                         VALUES (?1, ?2, ?3, ?4, ?5) \
                         ON CONFLICT(path) DO UPDATE SET \
                             content_hash = excluded.content_hash, \
                             size_bytes   = excluded.size_bytes, \
                             mtime_unix   = excluded.mtime_unix, \
                             languages    = excluded.languages, \
                             imported_at  = datetime('now')",
                        rusqlite::params![key, hash, size, mtime_unix, languages],
                    )?;
                    Ok(())
                })
                .await??;
                Ok(())
            }
            #[cfg(feature = "postgres")]
            Self::Postgres(backend) => {
                let client = backend.pool().get().await?;
                client
                    .execute(
                        "INSERT INTO bootstrap_imports \
                             (path, content_hash, size_bytes, mtime_unix, languages) \
                         VALUES ($1, $2, $3, $4, $5) \
                         ON CONFLICT (path) DO UPDATE SET \
                             content_hash = EXCLUDED.content_hash, \
                             size_bytes   = EXCLUDED.size_bytes, \
                             mtime_unix   = EXCLUDED.mtime_unix, \
                             languages    = EXCLUDED.languages, \
                             imported_at  = now()",
                        &[&path, &hash, &size, &mtime_unix, &languages],
                    )
                    .await?;
                Ok(())
            }
        }
    }
}

/// Cheaply stat a regular file for its `(size, mtime as unix nanoseconds)`
/// without reading its contents. Returns `None` for anything that is not a
/// regular file (e.g. an RxNorm RRF directory) or when the metadata/mtime is
/// unavailable, in which case callers fall back to the full content-hash
/// comparison.
///
/// Nanosecond precision (not seconds) is used so that a same-size content
/// rewrite is reliably detected on filesystems with sub-second mtime
/// granularity (ext4, tmpfs, APFS, NTFS) — a seconds-resolution mtime could
/// collide for two writes in the same wall-clock second.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub fn cheap_file_stat(path: &std::path::Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((meta.len() as i64, mtime))
}

/// Compute a `(SHA-256 hex, size in bytes)` signature for a bootstrap entry.
///
/// For a regular file, this is the SHA-256 of its contents. For a directory
/// (e.g. an RxNorm RRF folder), hashing every byte on every startup would
/// dominate boot time, so the signature is derived from the sorted
/// `(name, size)` of its immediate children instead — a new release dropped in
/// (which changes file sizes) still re-triggers import, while an unchanged
/// folder is cheaply recognized.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub fn compute_signature(path: &std::path::Path) -> std::io::Result<(String, i64)> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let size: u64 = if path.is_dir() {
        let mut entries: Vec<(String, u64)> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let len = e.metadata().ok()?.len();
                Some((name, len))
            })
            .collect();
        entries.sort();
        let mut total: u64 = 0;
        for (name, len) in &entries {
            hasher.update(name.as_bytes());
            hasher.update(b":");
            hasher.update(len.to_le_bytes());
            hasher.update(b"\n");
            total += len;
        }
        total
    } else {
        let mut file = std::fs::File::open(path)?;
        std::io::copy(&mut file, &mut hasher)?
    };
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((hex, size as i64))
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub async fn import_directory_files(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    dir: &std::path::Path,
    batch_size: usize,
    dry_run: bool,
    languages: &LanguageFilter,
    tracker: Option<&BootstrapTracker<'_>>,
) -> Result<(ImportStats, usize, usize), crate::error::HtsError> {
    use crate::error::HtsError;

    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| {
            HtsError::InvalidRequest(format!("Cannot read directory '{}': {e}", dir.display()))
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let hidden = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            !hidden
        })
        .collect();
    entries.sort();

    let mut stats = ImportStats::default();
    let mut imported_count = 0usize;
    let mut skipped_count = 0usize;
    let total = entries.len();

    for (idx, entry) in entries.iter().enumerate() {
        let fname = entry
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<non-utf8>");
        let is_dir = entry.is_dir();
        let suffix = if is_dir { "/" } else { "" };

        // Resolve the import format for this entry, skipping entries we don't
        // recognize (non-rxnorm directories, undetectable file formats).
        let format = if is_dir {
            if contains_rxnorm_rrf(entry) {
                ImportFormat::Rxnorm
            } else {
                eprintln!(
                    "[skip] {}/{total} {fname}/: nested directory ignored",
                    idx + 1
                );
                skipped_count += 1;
                continue;
            }
        } else {
            match detect_format(entry) {
                Some(f) => f,
                None => {
                    eprintln!(
                        "[skip] {}/{total} {fname}: format not auto-detected",
                        idx + 1
                    );
                    skipped_count += 1;
                    continue;
                }
            }
        };

        // Bootstrap-sync gating: skip files already imported with identical
        // content; otherwise remember the signature to record after a
        // successful import. A hashing or ledger-lookup error is non-fatal —
        // we log it and fall through to import the file unconditionally.
        //
        // Skip decision, cheapest-first:
        //   1. stat the file (no read) — if size + mtime + applied language
        //      filter all match the ledger, the file is unchanged → skip without
        //      ever reading it. This is what keeps restarts fast for multi-GB
        //      SNOMED/LOINC archives.
        //   2. only when the cheap stat disagrees (or the ledger predates the
        //      mtime column) fall back to a full content hash, so a file whose
        //      mtime was merely touched is still recognized as unchanged.
        let mut pending_signature: Option<(String, i64, Option<i64>, String)> = None;
        if let Some(tracker) = tracker {
            // The language filter applied to this file, stored separately from
            // the content hash so the cheap fast-path can detect a changed
            // HTS_IMPORT_LANGUAGES without re-hashing. Empty = no filtering
            // (non-language-sensitive format, or the allow-all default).
            let lang_sensitive = matches!(format, ImportFormat::SnomedRf2 | ImportFormat::Loinc);
            let applied_langs = if lang_sensitive && !languages.allows_all() {
                languages.canonical_spec()
            } else {
                String::new()
            };
            // Directories (RxNorm RRF) return None and fall through to the
            // metadata-based compute_signature, which is already cheap for them.
            let stat = cheap_file_stat(entry);

            match tracker.imported_record(fname).await {
                Ok(Some(rec)) => {
                    let fast_match = matches!((stat, rec.mtime_unix), (Some((sz, mt)), Some(rmt))
                        if sz == rec.size_bytes && mt == rmt)
                        && rec.languages == applied_langs;
                    if fast_match {
                        eprintln!(
                            "[skip] {}/{total} {fname}{suffix}: unchanged since last bootstrap (size+mtime)",
                            idx + 1
                        );
                        skipped_count += 1;
                        continue;
                    }
                    // Stat disagreed (or legacy row without mtime): hash to see
                    // whether the contents actually changed.
                    match compute_signature(entry) {
                        Ok((hash, size)) => {
                            let mtime = stat.map(|(_, m)| m);
                            if hash == rec.content_hash && rec.languages == applied_langs {
                                // Content identical — only the mtime was touched.
                                // Refresh the ledger so the next boot fast-paths,
                                // then skip.
                                if !dry_run
                                    && let Err(e) = tracker
                                        .record(fname, &hash, size, mtime, &applied_langs)
                                        .await
                                {
                                    tracing::warn!(
                                        file = fname,
                                        error = %e,
                                        "failed to refresh bootstrap ledger mtime"
                                    );
                                }
                                eprintln!(
                                    "[skip] {}/{total} {fname}{suffix}: unchanged since last bootstrap (content hash)",
                                    idx + 1
                                );
                                skipped_count += 1;
                                continue;
                            }
                            pending_signature = Some((hash, size, mtime, applied_langs));
                        }
                        Err(e) => tracing::warn!(
                            file = fname,
                            error = %e,
                            "could not hash bootstrap file; importing unconditionally"
                        ),
                    }
                }
                Ok(None) => match compute_signature(entry) {
                    Ok((hash, size)) => {
                        let mtime = stat.map(|(_, m)| m);
                        pending_signature = Some((hash, size, mtime, applied_langs));
                    }
                    Err(e) => tracing::warn!(
                        file = fname,
                        error = %e,
                        "could not hash bootstrap file; importing unconditionally"
                    ),
                },
                Err(e) => tracing::warn!(
                    file = fname,
                    error = %e,
                    "bootstrap ledger lookup failed; importing unconditionally"
                ),
            }
        }

        eprintln!(
            "[import] file {}/{total} ({format}): {fname}{suffix}",
            idx + 1
        );
        match dispatch_import(format, backend, ctx, entry, batch_size, dry_run, languages).await {
            Ok(file_stats) => {
                stats.merge(file_stats);
                imported_count += 1;
                if let (Some(tracker), Some((hash, size, mtime, langs))) =
                    (tracker, pending_signature.as_ref())
                    && !dry_run
                    && let Err(e) = tracker.record(fname, hash, *size, *mtime, langs).await
                {
                    tracing::warn!(
                        file = fname,
                        error = %e,
                        "failed to record bootstrap import; file will be re-imported next startup"
                    );
                }
            }
            Err(e) => {
                // In directory-iteration mode we deliberately keep going so
                // one broken file doesn't prevent the rest of the bundle
                // from loading. The failure is still reported via
                // ImportStats.errors, which drives exit code 2.
                let msg = format!("[{format}] '{fname}{suffix}' failed: {e}");
                eprintln!("[error] {msg}");
                stats.errors.push(msg);
                skipped_count += 1;
            }
        }
    }

    Ok((stats, imported_count, skipped_count))
}

/// Returns `true` if `path` is a directory containing an `RXNCONSO.RRF`
/// file (case-insensitive) — the hallmark of an extracted RxNorm RRF
/// distribution. Used to keep the RxNorm importer reachable when the user
/// points `hts import` at a flat RRF directory.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub fn contains_rxnorm_rrf(path: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        e.file_name()
            .to_str()
            .map(|n| n.eq_ignore_ascii_case("RXNCONSO.RRF"))
            .unwrap_or(false)
    })
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// Write a minimal FHIR Bundle wrapping a single CodeSystem to `dir/name`.
    fn write_bundle(dir: &std::path::Path, name: &str, cs_url: &str, version: &str) {
        let bundle = serde_json::json!({
            "resourceType": "Bundle",
            "type": "collection",
            "entry": [{
                "resource": {
                    "resourceType": "CodeSystem",
                    "url": cs_url,
                    "version": version,
                    "status": "active",
                    "content": "complete",
                    "concept": [{ "code": "A", "display": "Alpha" }]
                }
            }]
        });
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();

        // Stamp a deterministic mtime keyed on (name, version) so the bootstrap
        // size+mtime fast-path reliably sees a content change as a change,
        // independent of the host filesystem's mtime granularity (some
        // platforms, e.g. the CI/WSL temp dirs, resolve mtime to whole
        // seconds, which would otherwise collide for two same-size rewrites in
        // the same wall-clock second). A real terminology release replaces the
        // file and so always lands a fresh mtime — this mirrors that.
        let key: u64 = name.bytes().chain(version.bytes()).map(u64::from).sum();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + key);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn compute_signature_is_deterministic_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.json");

        std::fs::write(&file, b"hello").unwrap();
        let (h1, s1) = compute_signature(&file).unwrap();
        let (h2, s2) = compute_signature(&file).unwrap();
        assert_eq!(h1, h2, "same content hashes identically");
        assert_eq!(s1, 5);
        assert_eq!(s2, 5);

        std::fs::write(&file, b"hello world").unwrap();
        let (h3, s3) = compute_signature(&file).unwrap();
        assert_ne!(h1, h3, "changed content yields a different hash");
        assert_eq!(s3, 11);
    }

    /// The bootstrap sync must import a file the first time, skip it while
    /// unchanged, re-import it when its content changes (e.g. a version bump),
    /// and pick up newly added files — without re-importing the unchanged ones.
    #[tokio::test]
    async fn bootstrap_sync_imports_new_and_changed_files_only() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let backend = SqliteTerminologyBackend::new(db.path().to_str().unwrap()).unwrap();
        let ctx = TenantContext::system();
        let tracker = BootstrapTracker::Sqlite(&backend);
        let boot = tempfile::tempdir().unwrap();
        write_bundle(boot.path(), "cs.json", "http://example.org/cs", "1.0.0");

        // First run: imports the file.
        let (stats, imported, skipped) = import_directory_files(
            &backend,
            &ctx,
            boot.path(),
            500,
            false,
            &LanguageFilter::default(),
            Some(&tracker),
        )
        .await
        .unwrap();
        assert_eq!((imported, skipped), (1, 0));
        assert_eq!(stats.code_systems, 1);

        // Second run, file unchanged: skipped, nothing imported.
        let (stats, imported, skipped) = import_directory_files(
            &backend,
            &ctx,
            boot.path(),
            500,
            false,
            &LanguageFilter::default(),
            Some(&tracker),
        )
        .await
        .unwrap();
        assert_eq!((imported, skipped), (0, 1));
        assert_eq!(stats.code_systems, 0);

        // Bump the version: content changes, so the file is re-imported.
        write_bundle(boot.path(), "cs.json", "http://example.org/cs", "2.0.0");
        let (_stats, imported, skipped) = import_directory_files(
            &backend,
            &ctx,
            boot.path(),
            500,
            false,
            &LanguageFilter::default(),
            Some(&tracker),
        )
        .await
        .unwrap();
        assert_eq!((imported, skipped), (1, 0));

        // Add a brand-new file: only it is imported; cs.json stays skipped.
        write_bundle(boot.path(), "cs2.json", "http://example.org/cs2", "1.0.0");
        let (_stats, imported, skipped) = import_directory_files(
            &backend,
            &ctx,
            boot.path(),
            500,
            false,
            &LanguageFilter::default(),
            Some(&tracker),
        )
        .await
        .unwrap();
        assert_eq!((imported, skipped), (1, 1));
    }

    /// Without a tracker (the `hts import <dir>` CLI path) every recognized file
    /// is imported unconditionally on each invocation.
    #[tokio::test]
    async fn import_without_tracker_always_imports() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let backend = SqliteTerminologyBackend::new(db.path().to_str().unwrap()).unwrap();
        let ctx = TenantContext::system();
        let boot = tempfile::tempdir().unwrap();
        write_bundle(boot.path(), "cs.json", "http://example.org/cs", "1.0.0");

        for _ in 0..2 {
            let (_stats, imported, skipped) = import_directory_files(
                &backend,
                &ctx,
                boot.path(),
                500,
                false,
                &LanguageFilter::default(),
                None,
            )
            .await
            .unwrap();
            assert_eq!((imported, skipped), (1, 0));
        }
    }
}
