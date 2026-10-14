# Terminology updates — operations SOP (HTS)

How the production Helios Terminology Server (HTS) is kept current: annual
ICD-10-CM / ICD-10-PCS (October), semi-annual LOINC (February / August), monthly
SNOMED CT International, plus the Taiwanese zh-Hant supplements and the lab
ObservationDefinitions that depend on them.

Reference deployment: compose project `hfs-eval` (`/home/brian/projects/hfs-eval`),
service `hts`, container `hfs-eval-hts`, SQLite file `/data/hts.db` on the
`hts-data` volume, public base `https://hfs.gugulu.tw/tx/`. Source files are
bind-mounted read-only at `/import` from `/home/brian/projects/test_datas`.

## 1. What is loaded and how versions coexist

| Terminology | Canonical URL | `CodeSystem.version` | Where the version comes from |
|---|---|---|---|
| SNOMED CT International | `http://snomed.info/sct` | `YYYYMMDD` (e.g. `20250601`) | RF2 file-name release date |
| LOINC | `http://loinc.org` | `2.80` | `Loinc_<ver>/` folder inside the ZIP, else the ZIP file name (`Loinc_2.80.zip`) |
| ICD-10-CM | `http://hl7.org/fhir/sid/icd-10-cm` | `current` (fixed) | hard-coded by the importer — a new fiscal year **replaces** the previous one in place |
| ICD-10-PCS | `http://www.cms.gov/Medicare/Coding/ICD10` | `2025` | `<version>` in the CMS tables XML, else the fiscal year in the file name (`icd10pcs_codes_2025.txt`) |
| zh-Hant supplements | `https://terminology.healthymind.tw/CodeSystem/{icd-10-cm,icd-10-pcs,loinc}-zh-hant` | `2024-11-18` (MOHW file 1131118) / `2026-10` | `tools/terminology/make_zh_hant_supplements.py --version …` |

Storage model (`crates/hts/src/import/fhir_bundle.rs`): one `code_systems` row
per **(url, version)**, storage id `<fhir-id>|<version>`. Re-importing the *same*
version upserts in place (concepts are deleted and re-inserted per batch);
importing a *new* version adds a second row with its own concept set, so two
LOINC or SNOMED releases coexist and roughly double that system's disk use.

Which row answers an unversioned request is decided by one shared ORDER BY
(`crates/hts/src/backends/mod.rs::cs_precedence_order_by`):

1. `content = complete | supplement` before `fragment | example` before `not-present`
   (so a real import always beats the THO stubs `snomedct`, `v3-loinc`, `icd10PCS`);
2. rows that actually have concepts;
3. `authority_rank` (publisher's own distribution = 0 wins);
4. `version` **string** descending — `20250601 > 20250501`, `2.81 > 2.80`, `2026 > 2025`
   all sort correctly; mixing schemes (e.g. `2.0.1` THO stub vs `2025`) is
   harmless because tier 1–2 already decided.

Pinned requests (`system=…&version=…`, `url|version`) resolve the exact row.

## 2. Pre-flight (every update)

```bash
cd /home/brian/projects/hfs-eval
df -h /                                   # need ≥ 15 GB free AFTER the import (see table in §7)
docker compose ps                         # hts Up
docker exec hfs-eval-hts du -sh /data     # current DB size
docker exec hfs-eval-hts ls /import/…     # the new release is visible (read-only mount)
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8082/tx/metadata
```

**Backup first** (the only universal rollback, see §6):

```bash
docker compose stop hts
docker run --rm -v hfs-eval_hts-data:/data -v /home/brian/backups:/bak debian:trixie-slim \
  sh -c 'cp /data/hts.db /bak/hts.db.$(date +%F) && ls -l /bak'
docker compose start hts
```

(`hts.db-wal` is empty after a clean stop; if you cannot stop, use
`sqlite3 /data/hts.db ".backup /bak/hts.db.$(date +%F)"` from a host with sqlite3
and the volume mounted.)

## 3. Running an import beside the live server

`hts import` opens the same SQLite file as the running server. This is
supported by design: the backend sets `journal_mode=WAL` and
`busy_timeout=30000` (`crates/hts/src/backends/sqlite/mod.rs`), readers never
block on the writer, and after the last batch the CLI itself builds the concept
closure and the FTS index (`finalize_after_bootstrap`) so the server never has
to. Observed side effects:

* Query latency rises while batches commit (REPORT.md D-0: a 1 s HFS `$validate`
  became 14 s during a THO import). Schedule big imports off-peak.
* The server keeps in-process caches (`$expand` results, `$validate-code`
  responses, "ValueSet not found" negatives). The CLI runs in another process and
  cannot clear them, so **restart hts after the import** (`docker compose restart hts`,
  ~20 s with an existing DB) before validating results.
* Do not start an import while the server is still starting or warming its
  indexes (first ~minutes after `docker compose up -d hts`): the first LOINC run
  hit the 30 s busy timeout and finished with exit 2 and
  `CodeSystem 'http://loinc.org' import failed: database is locked` on every
  batch. Re-running once the server was idle succeeded. Run imports **one at a
  time** for the same reason.
* I/O contention dominates wall time. Use `--batch-size 5000` (the server's own
  bootstrap default) — the default 500 commits 10× more transactions and was 4×
  slower on this host while other jobs were writing to the disk.

Generic form:

```bash
docker exec hfs-eval-hts /app/hts import --format <fmt> --batch-size 5000 /import/<path>
# exit 0 = ok, 2 = finished with non-fatal record errors (listed on stderr), 1 = failed
docker compose restart hts
```

The eval helper `eval/scripts/p1/c/run_import.sh <label> <fmt> <path> [args]`
(in the hfs-eval repo) wraps this with timing, `df`, `du` and memory sampling and
writes `eval/results/p1c/<label>.*.log`.

## 4. Per-terminology procedures

### 4.1 ICD-10-CM — every October (CMS fiscal year)

Source: CMS "Code Tables, Tabular and Index" ZIP (`icd10cm-table-index-YYYY.zip`,
~20 MB). The importer reads the `*tabular*.xml` entry; the index/drug/neoplasm
files are ignored.

```bash
docker exec hfs-eval-hts /app/hts import --format icd10-cm --batch-size 5000 \
  /import/icd/10/icd10cm/icd10cm-table-index-2026.zip
```

Version is always `current`, so the new year **overwrites** the previous one
(same `(url, version)` row). Keep the old ZIP; rolling back = re-import it.
~47 k codes + ~27 k header/section nodes, a few minutes. Note the bundled image
bootstrap also ships an ICD-10-CM ZIP (`crates/hts/terminology-data/`); on a
*fresh* volume the bootstrap imports it before the server starts — update that
file in the fork when you update production so the two agree.

After the CM import, regenerate and reload the zh-Hant supplement if MOHW has
published a new 中文版 (§4.5) — codes added by CMS without a MOHW translation simply
have no zh-Hant designation.

### 4.2 ICD-10-PCS — every October

Source: CMS `icd10pcs_tables_YYYY.zip` or `icd10pcs_order_YYYY.zip` (contains
`icd10pcs_codes_YYYY.txt`, optional `icd10pcs_tables_YYYY.xml`).

```bash
docker exec hfs-eval-hts /app/hts import --format icd10-pcs --batch-size 5000 \
  /import/icd/10/icd10pcs/icd10pcs_tables_2026.zip
```

Version = fiscal year, so successive years **coexist**; unversioned requests get
the newest. Hierarchy: `ICD-10-PCS` → section (1 char) → body system (2) → root
operation (3) → code (7). When the tables XML is present in the ZIP the axis
labels come from it; otherwise built-in section / Medical-and-Surgical body
system names and a label derived from the child titles are used. ~79 k codes +
~1 k grouping nodes, ~1–2 min.

### 4.3 LOINC — February and August

Source: `Loinc_<ver>.zip` from loinc.org (free account). The importer reads
`LoincTable/Loinc.csv`, `AccessoryFiles/ComponentHierarchyBySystem/…`, and
**every** `AccessoryFiles/LinguisticVariants/*LinguisticVariant.csv` as
`designation`s tagged with the BCP-47 language (zh-CN included — no separate
supplement is needed for Simplified Chinese). Limit languages with
`--languages en,zh-CN` to save ~40 % of the designation rows.

```bash
docker exec hfs-eval-hts /app/hts import --format loinc --batch-size 5000 \
  /import/loinc/2.81/Loinc_2.81.zip
```

New versions coexist (`2.80`, `2.81` rows). After validating, free the disk by
deleting the old version (§6.2).

### 4.4 SNOMED CT International — monthly (1st of the month release)

Source: `SnomedCT_InternationalRF2_PRODUCTION_YYYYMMDDT120000Z.zip` (Snapshot
is enough; ~565 MB, licence-restricted — never copy it off this host).

```bash
docker exec hfs-eval-hts /app/hts import --format snomed-rf2 --batch-size 5000 \
  /import/snomed/SnomedCT_InternationalRF2_PRODUCTION_20250701T120000Z.zip
```

Imports **active** concepts only (~370 k), all active English descriptions as
designations (FSN + synonyms, `en`, `en-US`, `en-GB` preferred flags), Is-a
edges, role relationships and association refsets. Memory: the parser holds the
whole description set in RAM (peak ≈ 3–4 GB RSS for the importer process).
Versions coexist as `YYYYMMDD` rows; delete the previous month after
validation (§6.2) — each release is several GB of DB.

### 4.5 zh-Hant supplements (MOHW ICD-10-CM/PCS 中文版, Taiwan LOINC names)

```bash
cd /home/brian/projects/hfs            # fork checkout
python3 -I tools/terminology/make_zh_hant_supplements.py \
  --mohw-xlsx '/home/brian/projects/test_datas/icd/10/<MOHW xlsx>' \
  --loinc-csv examples/tw/taiwan-loinc-mapping.csv \
  --version 2024-11-18 --out-dir /home/brian/projects/test_datas/generated
docker exec hfs-eval-hts /app/hts import --format fhir-bundle /import/generated/icd10cm-zh-hant.bundle.json
docker exec hfs-eval-hts /app/hts import --format fhir-bundle /import/generated/icd10pcs-zh-hant.bundle.json
docker exec hfs-eval-hts /app/hts import --format fhir-bundle /import/generated/loinc-zh-hant.bundle.json
docker compose restart hts
```

The generator needs no third-party packages (it parses the .xlsx with the
standard library). Supplements are `content: supplement` CodeSystems whose
`supplements` points at the base URL *without* a version, so they apply to every
loaded version of the base system. Query:

```bash
# explicit (works today)
# (POST only: the v0.2.4 GET handler drops `useSupplement`)
curl -s -X POST http://127.0.0.1:8082/tx/CodeSystem/\$lookup -H 'content-type: application/fhir+json' -d '{
  "resourceType":"Parameters","parameter":[
    {"name":"system","valueUri":"http://hl7.org/fhir/sid/icd-10-cm"},
    {"name":"code","valueCode":"E11.9"},
    {"name":"useSupplement","valueCanonical":"https://terminology.healthymind.tw/CodeSystem/icd-10-cm-zh-hant"}]}'
# → designation zh-Hant "第二型糖尿病，未伴有併發症", source …icd-10-cm-zh-hant|2024-11-18
# implicit (needs the Phase 1 stream-B change that merges supplement designations into $lookup/$expand)
curl -s 'http://127.0.0.1:8082/tx/CodeSystem/$lookup?system=http://hl7.org/fhir/sid/icd-10-cm&code=E11.9&displayLanguage=zh-Hant'
```

### 4.6 Lab reference ranges → ObservationDefinition (HFS, not HTS)

```bash
python3 -I tools/terminology/make_observation_definitions.py \
  --ranges examples/tw/lab-reference-ranges.csv --mapping examples/tw/taiwan-loinc-mapping.csv \
  --out examples/tw/observation-definitions.json
# once per server: the custom SearchParameter (R4 has no ObservationDefinition.code search)
curl -X PUT http://127.0.0.1:8082/fhir/SearchParameter/tw-observationdefinition-code \
  -H 'content-type: application/fhir+json' --data-binary @examples/tw/searchparameter-observationdefinition-code.json
curl -X POST http://127.0.0.1:8082/fhir/ -H 'content-type: application/fhir+json' \
  --data-binary @examples/tw/observation-definitions.json
curl -s 'http://127.0.0.1:8082/fhir/ObservationDefinition?code=http://loinc.org|2160-0'
```

Ids are `tw-lab-<loinc>`; the Bundle uses PUT so re-running updates in place.

## 5. Post-import verification (copy/paste)

```bash
T=http://127.0.0.1:8082/tx
curl -s "$T/CodeSystem?url=http://snomed.info/sct&_elements=version,content,count" | jq '.entry[].resource'
curl -s "$T/CodeSystem/\$validate-code?url=http://snomed.info/sct&code=73211009" | jq .parameter[0]
curl -s "$T/CodeSystem/\$validate-code?url=http://loinc.org&code=2345-7"          | jq .parameter[0]
curl -s "$T/CodeSystem/\$lookup?system=http://www.cms.gov/Medicare/Coding/ICD10&code=0DTJ0ZZ" | jq '.parameter[]|select(.name=="display")'
# ECL: only as an inline compose `constraint` filter — the `?fhir_vs=ecl/…` URL form is
# not implemented in this build (404); `?fhir_vs` and `?fhir_vs=isa/<code>` are.
curl -s -X POST "$T/ValueSet/\$expand" -H 'content-type: application/fhir+json' -d '{"resourceType":"Parameters",
  "parameter":[{"name":"count","valueInteger":5},{"name":"valueSet","resource":{"resourceType":"ValueSet",
  "status":"active","compose":{"include":[{"system":"http://snomed.info/sct",
  "filter":[{"property":"constraint","op":"=","value":"<< 73211009"}]}]}}}]}' | jq .expansion.total
curl -s "$T/ValueSet/\$expand?url=http://snomed.info/sct?fhir_vs=isa/404684003&count=100&offset=50000" | jq .expansion.total
curl -s "$T/CodeSystem/\$subsumes?system=http://snomed.info/sct&codeA=73211009&codeB=44054006" | jq .parameter[0]
```

Expected (20250601): `result=true`, ECL `<< 73211009` total = 124, `isa/404684003` total = 125 710, `subsumes`.

## 6. Rollback

### 6.1 Whole-database restore (any situation)

```bash
cd /home/brian/projects/hfs-eval
docker compose stop hts
docker run --rm -v hfs-eval_hts-data:/data -v /home/brian/backups:/bak debian:trixie-slim \
  sh -c 'rm -f /data/hts.db-wal /data/hts.db-shm && cp /bak/hts.db.<date> /data/hts.db && chown 1000:1000 /data/hts.db'
docker compose start hts
```

### 6.2 Remove one version (SNOMED / LOINC / PCS) and keep the rest

`DELETE /tx/CodeSystem/{id}` where `{id}` is the storage id
(`snomed-ct|<version-uri>`, `loinc|2.80`, `icd10pcs|2025` — see
`CodeSystem?url=…` for the ids). On the v0.2.4 code base DELETE of CLI-imported
rows returns 500 (REPORT.md D-0, fixed by Phase 1 stream B T5); until that build
is deployed use 6.1, or re-import the previous release file (same-version
re-import is an in-place replacement for ICD-10-CM).

### 6.3 Image rollback

`HTS_IMAGE` in `/home/brian/projects/hfs-eval/.env` selects the image;
`docker compose up -d hts` applies it. The DB schema is forward-compatible
within Phase 1 builds; keep the previous tag in the image store
(`docker image ls healthymind/hts`).

## 7. Capacity planning (measured on this host, see `eval/results/p1c-terminology-load.md`)

| Import (`--batch-size 5000`) | Wall time | `/data` growth | Importer peak RSS | Notes |
|---|---|---|---|---|
| SNOMED CT INT 20250601 Snapshot | 106 min (6 360 s) | 2.0 → 5.8 GB (+3.8) | 2.7 GB | 373 972 concepts, 11 ConceptMaps; closure + FTS alone 23 min; includes ~8 min paused for disk |
| LOINC 2.80 (all 19 linguistic variants) | 133 min (7 999 s) | 5.0 → 7.2 GB (+2.2) | 2.5 GB | 176 668 concepts, 932 464 designations; parse+write 25 min, FTS 47 min, ANALYZE/prewarm 63 min |
| ICD-10-PCS 2025 | 10 min (587 s) | 7.2 → 7.4 GB (+0.2) | 1.9 GB | 79 988 concepts (54 s on an idle disk) |
| zh-Hant supplements (CM 96 802 + PCS 78 530 + LOINC 32) | 63 + 73 + 68 s | 7.4 → 7.5 GB (+0.1) | — | one at a time |

These were measured while three other builds saturated the same disk (I/O
pressure `full avg60 ≈ 27 %`); on an idle host expect roughly a third. Budget
**2× the growth** in free space during a load: the WAL reached 1.3–1.5 GB before
checkpointing.

Rule of thumb: keep at most two versions of SNOMED and LOINC loaded, and never
start an import that would leave < 15 GB free on `/`.
