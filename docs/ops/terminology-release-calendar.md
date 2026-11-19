# Terminology release calendar and change SOP

The **how** of each import (commands, verification queries, per-version
rollback) is in [terminology-updates.md](terminology-updates.md). This page is
the **when and who**: the yearly calendar, the change-control wrapper every
terminology update goes through on the PostgreSQL production stack, and the
records to keep.

## 1. Calendar

| Terminology | Publisher release | Hospital deadline | Notes |
|---|---|---|---|
| ICD-10-CM (US, CMS) | June–August for the fiscal year starting **1 October** | live by **1 October** | the importer stores version `current` and **replaces** the previous year in place; claims/reporting systems must switch on the same day |
| ICD-10-PCS (CMS) | same | live by **1 October** | new version row (e.g. `2026`) beside the previous one |
| 衛福部 ICD-10-CM/PCS 中文版 (zh-Hant supplement) | irregular (MOHW) | within 1 month of publication | regenerate with `tools/terminology/make_zh_hant_supplements.py --version …` |
| 健保 ICD-10 申報版本 | per NHI announcement | NHI effective date | confirm which CM year NHI requires before 1 October — it can lag CMS |
| LOINC | **February** and **August** | within 6 weeks | new version row; keep the previous version until every IG ValueSet pinned to it is updated |
| SNOMED CT International | monthly (1st of the month) | quarterly is enough unless a clinical project needs a new concept | ~1.4 GB per version; keep at most two versions, drop the older after a month |
| THO / bundled terminologies (in the HTS image) | with HTS image upgrades | with the next upgrade | `bootstrap_imports` re-imports changed bundled files at startup |
| UCUM | rarely | with HTS upgrades | built into HTS |
| CVX, ISO 4217 | CDC monthly / ISO irregular | yearly | `eval/scripts/p25-z/cvx_codesystem.py`, `iso4217_codesystem.py` |

Suggested yearly plan:

| Month | Work |
|---|---|
| Jan | SNOMED (Jan release) if due |
| Feb–Mar | **LOINC Feb release** |
| Apr | SNOMED quarterly |
| Jul | SNOMED quarterly; download CMS ICD-10-CM/PCS for next FY; check NHI announcement |
| Aug–Sep | **ICD-10-CM/PCS** rehearsal on a production copy (§2 step 2); **LOINC Aug release** |
| **1 Oct** | **ICD-10-CM/PCS go-live** |
| Oct | SNOMED quarterly; MOHW zh-Hant supplement if published |
| Dec | annual review of loaded versions; drop superseded LOINC/SNOMED versions |

## 2. Change SOP (every terminology update)

1. **Ticket** with: terminology, old → new version, source file and its
   sha256, license confirmation (SNOMED affiliate licence for Taiwan member
   country, LOINC terms of use), requester, planned window.
2. **Rehearse** on a production copy for ICD-10 (in-place replacement) and any
   release with importer changes:
   restore the nightly `hts` dump into a scratch PostgreSQL
   (`scripts/ops/pg-restore.sh … hts`), run the import there with the same
   image, run the verification block of terminology-updates.md §5, note the
   import time and DB growth.
3. **Backup** right before the production import (PostgreSQL replaces the
   SQLite file copy described in terminology-updates.md §2):
   ```bash
   PG_CONTAINER=hfs-db PGUSER=postgres BACKUP_DIR=/backup/hfs/dumps scripts/ops/pg-backup.sh -k 0 hts
   ```
   (~1–4 min, 400 MB dump for today's 5 GB `hts`; see [backup-restore.md](backup-restore.md)).
4. **Import** per terminology-updates.md §3–4 — while HTS keeps serving.
5. **Verify** (terminology-updates.md §5) plus: `$expand` of three ValueSets
   the hospital's forms use, one `$validate-code` per system, an HFS
   `$validate` of a TW Core resource that binds to the updated system.
6. **Downstream**: reindex HFS search data that depends on terminology only if
   a SearchParameter uses `:in`/`:below` against it (none today); re-run IG
   example validation (`eval/scripts/p25-z/matrix.py`) after ICD-10/LOINC
   changes; notify integrating systems.
7. **Record** in the ticket: versions now loaded
   (`GET /tx/CodeSystem?url=…&_elements=url,version`), import duration,
   `pg_database_size('hts')` before/after, verification output.
8. **Rollback** if verification fails: per-version removal
   (terminology-updates.md §6.2) or the whole-database restore:
   `pg-restore.sh <hts dump> hts --replace --owner hts` with HTS stopped.

## 3. Disk budget

Each LOINC version adds ~1.4 GB and each SNOMED International version ~1.4 GB
to `hts` (plus WAL during the import — `max_wal_size` 8 GB, archived and
compressed). Keep ≥ 15 GB free on the data disk before an import and check the
WAL archive disk too: an import generates roughly 2× its table size in WAL.
