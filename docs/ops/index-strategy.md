# Search index strategy (PostgreSQL)

## 1. How HFS stores searchable data

| Table | Rows | Role |
|---|---|---|
| `resources` | one per live resource (`PRIMARY KEY (tenant_id, resource_type, id)`) | current version, JSONB |
| `resource_history` | one per version | `_history`, vread |
| `search_index` | ≈ 25–35 per resource | one row per extracted search value: `param_name` + one typed value column (`value_token_code`, `value_date`/`value_date_end`, `value_string`/`value_string_folded`, `value_reference`, `value_quantity_*`, `value_number`, `value_uri`) |
| `resource_fts` | one per resource | `_text` / `_content` tsvectors |

Every index on `search_index` leads with `(tenant_id, resource_type,
param_name)` and is **partial** on its value column being non-null, so each
index only holds the rows of its type. The live set (schema v46 + fork
migrations; identical on production and in this branch):

| Index | Serves |
|---|---|
| `idx_search_token_code` `(…, value_token_code, last_updated DESC, resource_id) INCLUDE (value_token_system)` | `code=`, `identifier=`, `status=`, `category=`, `gender=` — membership and newest-first paging in one scan |
| `idx_search_token_code_recent` `(…, last_updated DESC, resource_id) INCLUDE (code, system)` | broad token lists where walking newest-first and filtering is cheaper |
| `idx_search_date` / `idx_search_date_end` / `idx_search_date_recent` | `date=`, `birthdate=`, ranges (period start / end) |
| `idx_search_string`, `idx_search_string_folded_pattern` (`text_pattern_ops`), `idx_search_string_trgm` (GIN trigram) | `name=` / `family=` prefix (default), `:exact`, `:contains` |
| `idx_search_reference_pattern` (`text_pattern_ops`), `idx_search_reference_target_id` (`split_part(value_reference,'/',-1)`) | `subject=Patient/x`, `patient=x` (bare id), `_revinclude`, chains |
| `idx_search_quantity*`, `idx_search_number`, `idx_search_uri`, `idx_search_identifier_type`, composite indexes | the remaining parameter types |
| `idx_search_resource (tenant_id, resource_type, resource_id)` | deleting/reindexing one resource's rows, `_include` target fetch |
| `resources.idx_resources_search (tenant_id, resource_type, last_updated DESC, id) WHERE NOT is_deleted` | `_lastUpdated`, `_sort=-_lastUpdated`, type scans |
| `resources.idx_resources_patient_refs_v1` (GIN over `hfs_patient_references_v1(data)`) | `Patient/$everything`, patient-compartment export |

Upstream tuned this set on 7–39M-row `search_index` corpora (see
`docs/postgres-*-benchmark.md`); this page records the hospital-shaped
re-check.

## 2. Method

1. Load Synthea R4 (the 1,180-bundle 2019 sample) five times with rewritten
   ids (`load.py --copies 3`, then `--copy-offset 3 --copies 2`, as in `hfs-eval/eval/bench/BENCHMARK.md` §1.5):
   2,636,942 resources (5 copies: 1.30M Observation, 305k Claim, 234k Encounter, 234k EOB, 182k Procedure, 5,900 Patient), 31.1M `search_index` rows (5.2 GB heap + 12 GB indexes), 27 GB database.
2. `ANALYZE`, then `scripts/ops/search-bench.py` — 23 searches covering
   Patient by identifier / name / family / birthdate, Observation by
   patient + code + date, Encounter by patient + date, `_include`,
   `_revinclude`, `_lastUpdated`, chained (`patient.identifier`,
   `subject:Patient.family`), reverse chained (`_has`), `_summary=count`,
   `$everything`. Parameters are chosen from the data (a median patient, one
   of their identifiers, a common LOINC code). 7 warm runs each over one
   keep-alive connection; median / p95 reported.
3. `auto_explain` (`log_min_duration=0, log_analyze, log_buffers`) for one
   extra pass to capture every plan.

## 3. Results

Test box: native PostgreSQL 16.10 on NVMe, `shared_buffers=6GB`,
`random_page_cost=1.1`, HFS release build of this branch with
`HFS_PG_MAX_CONNECTIONS=32`; the box was shared with other builds (load
average 50–90). Times are wall time at the HTTP client (HFS + PostgreSQL).

| Search | Entries | first (ms) | median (ms) | p95 (ms) |
|---|---|---|---|---|
| `Patient?identifier=sys\|value` | 5 | 35.6 | 7.2 | 13.8 |
| `Patient?family=` | 25 | 21.7 | 12.5 | 20.5 |
| `Patient?name=` | 25 | 12.0 | 8.7 | 15.6 |
| `Patient?birthdate=` | 5 | 4.7 | 3.5 | 7.8 |
| `Patient?birthdate=ge…&birthdate=lt…` (one month) | 5–10 | 66–803 (cold) | 30–33 | 40–48 |
| `Patient?family=&birthdate=` | 5 | 27.7 | 10.1 | 14.8 |
| `Observation?patient=&code=&date=ge` | 10 | 20.0 | 10.5 | 16.9 |
| `Observation?subject=&_sort=-date` | 20 | 15.5 | 9.1 | 12.7 |
| `Observation?patient=&category=vital-signs` | 50 | 10.5 | 8.5 | 14.4 |
| **`Observation?code=<57,580 matches>&date=ge2019-01-01`** | 50 | 1,212 (cold) | **180** | **224** |
| `Observation?code=<57,580>&_summary=count` | – | 50.2 | 47.7 | 61.1 |
| `Encounter?patient=&date=ge` | 13 | 7.9 | 4.6 | 5.9 |
| `Encounter?patient=&_include=Encounter:service-provider&_include=Encounter:participant` | 21 | 12.0 | 6.1 | 8.0 |
| `Patient?_id=&_revinclude=Encounter:patient` | 16 | 4.9 | 5.1 | 6.1 |
| `Patient?_id=&_revinclude=Observation:subject` | 103 | 11.2 | 10.6 | 12.9 |
| `Observation?_lastUpdated=ge<1 h ago>` | 50 | 7.2 | 4.7 | 12.5 |
| `Observation?_sort=-_lastUpdated` | 50 | 5.1 | 5.6 | 15.6 |
| `Observation?patient.identifier=` (chain) | 50 | 77.8 | 22.8 | 25.7 |
| `Observation?subject:Patient.family=` (chain, 2,130 patients' refs) | 50 | 201 | 108 | 119 |
| `Patient?_has:Observation:patient:code=` | 50 | 28.9 | 24.0 | 27.7 |
| `Condition?patient=` | 4 | 5.6 | 2.8 | 4.0 |
| `MedicationRequest?patient=&status=active` | 0 | 6.5 | 3.8 | 4.2 |
| `Patient/{id}/$everything` | 189 | 161 | 142 | 172 |

For comparison, the October benchmark (`BENCHMARK.md`, 527k resources,
build `a3a0f9f88`) measured `code=…&date=ge…` at 314 ms p50 and
`Patient/$everything` at 389 ms; both are faster here on five times the data.

### Findings

1. **No missing index.** Every plan above uses one of the indexes in §1; no
   sequential scan of `search_index` or `resources` appears in any captured
   plan (`hfs-eval/eval/results-p3-pgops/bench-baseline-explain.plans.txt`).
   The patient-scoped searches the hospital will run most (a patient's
   observations, encounters, conditions, includes) stay at 3–17 ms p95.
2. **Broad token + date** (`Observation?code=<common LOINC>&date=ge…`, no
   patient) is the slowest pattern: 180 ms median. The plan bitmap-scans every
   Observation date after the bound (67k index rows, 23.5k heap pages) and
   hash-joins with the 57k code rows; row estimates are 3–13× low because
   `tenant/type/param/value` are correlated in this one-table design. Tried:
   statistics target 1000 on the value columns (plan unchanged, 128–139 ms
   after the warm-up), a `UNION ALL` instead of the `OR` in the date predicate
   (26.6k → 8.5k buffers but only 116 ms), and driving from the
   newest-first token index (255 ms — `last_updated` is not correlated with the
   clinical date in this data). None is worth a code change; in production this
   query shape comes from population dashboards, which belong in SQL-on-FHIR
   views or `$export`, not the transactional API.
3. **Two-sided ranges on one parameter** (`birthdate=ge…&birthdate=lt…`) are
   evaluated as two independent half-open scans intersected (FHIR semantics for
   repeated parameters on multi-valued elements). Each half reads every patient
   on its side of the bound: 5–6k index rows here, 30 ms. That grows linearly
   with the patient count — expect ~1–3 s at 1M patients. Upstream issue
   candidate: when the parameter's elements are single-valued (`birthdate`),
   the two comparisons can be one range scan on the same row.
4. **Chained search** cost is proportional to the number of chain targets:
   `subject:Patient.family=Mills423` matches 2,130 patients' references (all
   five copies share names) → 108 ms. Identifier chains are 23 ms.
5. **Writes**: 1,581,339 resources loaded in 434 s (3,645 resources/s, 8
   clients, 0 failed Bundles) and a further 1,054,226 in 366 s (2,884/s) —
   against 371–488 resources/s with 0.5–4.9% failed Bundles in October.
6. **Disk matters more than indexes.** The same load against the same
   PostgreSQL data directory moved to a rotational HDD on the same host (`/mnt/data1`, while the box was also heavily loaded) failed
   **every** transaction Bundle with `canceling statement due to statement
   timeout` while inserting search-index rows (30 s default) once the database
   reached ~9 GB. Production PostgreSQL is on an HDD today; move it to SSD
   before go-live (go-live checklist C).

### Recommendations

- Keep the index set as is; do not add indexes speculatively — every
  `search_index` index costs write throughput (31M rows, 12 GB of indexes at
  2.6M resources).
- After any bulk load: `VACUUM (ANALYZE)` (1 min 45 s at this size) — the
  visibility map makes the index-only scans above actually index-only.
- Run `scripts/ops/search-bench.py` after loading the hospital's real data
  (HAPI export) and after every upstream merge; compare against this table.

