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


## 4. Planning cost and extended statistics (hm-0006)

Line E found every search spending 8–17 ms in planning and < 0.3 ms executing
at 3.17M resources. Re-measured on 2,243,604 Synthea resources (line E's 25k
set, first 10,000 patients) / 30.9M `search_index` rows / 26 GB (search_index
16 GB = 61%), native PostgreSQL 16.10, `pg_stat_statements.track_planning`.

**Cause.** `search_index` carried three `(mcv, dependencies)` extended
statistics objects — `(resource_type, param_name, value_token_code)` from v15,
`(…, value_date)` from v16 and `(…, value_date_end)` from v43 — with
`value_token_code` at statistics target 4000. The planner deserialises each
applicable MCV list and tests every item on *every* planning; nothing caches
it between statements. A statement with many clauses (a chained search's
35-way reference `IN`) multiplies that.

**Configurations tried** (planning: `EXPLAIN (SUMMARY)` median of 22 warm
runs; execution: sum of the 28 benchmark medians in ms, HTTP wall time):

| config | token `code=` plan | `status=` plan | 35-way `IN` plan | code+date plan | ANALYZE search_index | 28-search sum | worst regressions |
|---|---|---|---|---|---|---|---|
| hm/main (3 MCV+deps objects, target 4000) | 1.46 ms | 1.42 ms | 17.2 ms | 2.47 ms | 62–80 s | 1,204–1,228 | — |
| no extended statistics | 0.35 | 0.17 | 2.47 | 0.57 | 25 s | 4,678 | `patient=&category=` 3,381 ms, code+date 512 ms, status+code 417 ms |
| same objects, statistics target 100 | 0.56 | 0.30 | 4.64 | 1.20 | 160 s | — | code+date 341 ms, status+code 298 ms |
| token MCV(100)+deps, no date objects | — | — | — | — | 41 s | — | code+date 329 ms, status+code 334 ms |
| **dependencies only (token + type/param), token target 1000** | **0.22** | **0.18** | **3.05** | **0.62** | **10 s** | **971** | none |
| dependencies only, token target 4000 | 0.22 | 0.17 | 2.33 | 0.63 | 36 s | 1,012 | none |
| dependencies only, all targets 100 | 0.17 | 0.12 | 2.42 | 0.64 | 0.7 s | 1,076 | none (chain identifier 18 ms vs 11) |

The dependency statistics are what fix the correlated estimates the v15
notes describe; the MCV lists contribute nothing measurable and at a small
target actively mislead (`code=&date=`, `status=&code=` ~40× slower).

**Change.** Fork migration `hm-0006-search-index-statistics` (schema.rs
`fork_search_index_statistics`): drop the three MCV objects, keep
`stx_search_type_param (dependencies)`, add
`stx_search_type_param_token_dep (dependencies)` on
`(resource_type, param_name, value_token_code)`, set `value_token_code`'s
target to 1000, then `ANALYZE search_index` without a statement timeout so the
new object has data before the first search (without it,
`Observation?patient=&category=` is a 3-second scan). On the 2.2M database the
first start after the upgrade took 8.4 s to the first `/metadata` 200 (the ANALYZE included); production (8.7k
resources) is sub-second.

Verified end to end (hm/main statistics restored, then this branch's binary
started on the same database): `hm_schema_migrations` gained
`hm-0006-search-index-statistics`; `search_index` kept exactly
`stx_search_type_param {f}` and `stx_search_type_param_token_dep {f}`;
`value_token_code` target 1000. Planning on the same database (box load ~20)
before → after: token 1.02 → 0.58 ms, `status=` 0.83 → 0.34, reference 0.59 →
0.18, 35-way `IN` 10.9 → 1.9, code+date 1.82 → 0.56. All 28 searches returned
the same entry counts; transcripts in
`hfs-eval/eval/results-p3-pgops/round2/`.

**Token + date conjunctions without a patient** (fixed in the follow-up
commit `perf(persistence): drive broad token+range searches …`).
`Observation?category=vital-signs&date=ge2020-01-01` took 430–790 ms with any
extended statistics. Cause, measured on the same database:

- the planner estimates the date membership at 37k rows for 485k real:
  `value_date` is NULL on 91.5% of `search_index` rows (every non-date
  parameter), and the range selectivity is multiplied by that null fraction as
  if it were independent of `param_name` — no extended statistics object can
  express this (dependencies are equality-only);
- the broad-conjunction walk (`search_impl.rs`, first page of a conjunction of
  broad memberships) drove from the smallest estimate — the date — and an
  upward-open date predicate (`value_date_end > $a OR (value_date >= $b AND …)`)
  cannot be walked in `last_updated` order by any index, so PostgreSQL scanned
  and sorted all 485k rows;
- probes written as plain `EXISTS` were turned into hash semi-joins that read
  the whole probed range (66k onset rows for `Condition?clinical-status=active&
  onset-date=ge2015`, 251k quantity rows for `code=…&value-quantity=gt20`).

Fix: range estimates are corrected by `1 / (1 − null_frac)` (from `pg_stats`,
cached 5 min); the driver is the smallest token-like or index-walkable range
membership and an upward-open date never drives while anything else can;
probes are `EXISTS (… OFFSET 0)`, per-row correlated subplans that stop with
the page; repeated parameters and the code/value-quantity pair (`INTERSECT`
forms #1416/#1580) use the walk when a token drives it. Driver experiments
(per-row probes, ms):

| shape | token rows | range rows | token drives | range drives |
|---|---|---|---|---|
| `status=final&date=ge2020` | 655k | 485k (open) | 0.4 | 3,626 |
| `clinical-status=active&onset-date=ge2015` | 93k | 66k (open) | 0.5 | 549 |
| `category=vital-signs&value-quantity=gt100` | 139k | 99k | 4.8 | 2.6 |
| `category=laboratory&date=le2012` | 452k | 117k | 14 | 1.7 |
| `category=vital-signs&date=le1975` | 139k | 8.9k | 57 | 14 |
| `category=survey&value-quantity=gt1000` | 43k | 1.4k | 586 | 13 |

Result on the 2.24M set — all 43 searches (the 28 above plus token+range
shapes with and without a patient and three adversarial small-intersection
ones), HTTP medians in ms, before = median of five runs of the previous build
interleaved with two runs of the new one:

| search | before (median of 5 runs) | after run 1 | after run 2 | entries |
|---|---|---|---|---|
| patient_by_identifier | 3.3 | 3.1 | 3.4 | 1/1 |
| patient_by_family | 11.2 | 11.5 | 12.9 | 26/26 |
| patient_by_name | 11.9 | 11.2 | 13.6 | 26/26 |
| patient_by_birthdate | 3.1 | 3.3 | 3.6 | 1/1 |
| patient_by_birthdate_range | 37.5 | 35.7 | 47.1 | 6/6 |
| patient_name_and_birthdate | 7.6 | 7.3 | 7.5 | 1/1 |
| obs_patient_code_date | 7.0 | 7.5 | 7.2 | 1/1 |
| obs_patient_sorted | 6.7 | 7.0 | 6.4 | 20/20 |
| obs_patient_category | 5.8 | 5.7 | 5.8 | 9/9 |
| obs_code_date_broad | 41.5 | 13.6 | 14.4 | 50/50 |
| obs_code_count | 20.5 | 21.9 | 20.1 | 0/0 |
| encounter_patient_date | 6.8 | 7.4 | 7.3 | 16/16 |
| encounter_patient_include | 11.5 | 11.2 | 10.1 | 20/20 |
| patient_revinclude_encounter | 7.6 | 7.2 | 7.8 | 17/17 |
| patient_revinclude_obs | 7.2 | 7.8 | 7.7 | 24/24 |
| obs_lastupdated | 6.4 | 6.0 | 6.8 | 50/50 |
| obs_lastupdated_sorted | 6.3 | 6.0 | 5.5 | 50/50 |
| obs_chain_patient_identifier | 10.1 | 11.4 | 11.4 | 23/23 |
| obs_chain_patient_family | 45.9 | 47.3 | 45.9 | 50/50 |
| patient_has_obs_code | 34.9 | 42.4 | 38.9 | 50/50 |
| condition_patient | 3.5 | 3.8 | 2.9 | 7/7 |
| medreq_patient_status | 3.7 | 3.9 | 3.0 | 0/0 |
| patient_everything | 166.5 | 175.6 | 130.7 | 197/197 |
| encounter_status_finished | 6.3 | 6.2 | 4.9 | 20/20 |
| obs_category_laboratory | 5.2 | 5.0 | 4.2 | 20/20 |
| obs_status_final_code | 9.8 | 10.0 | 7.4 | 20/20 |
| obs_category_date | 626.4 | 9.7 | 7.6 | 20/20 |
| condition_code | 5.6 | 5.0 | 3.6 | 20/20 |
| obs_category_date_patient | 8.2 | 7.9 | 6.7 | 9/9 |
| obs_status_date | 604.7 | 9.4 | 7.5 | 20/20 |
| obs_status_date_patient | 8.7 | 9.2 | 8.2 | 20/20 |
| obs_category_date_old | 12.3 | 11.5 | 9.6 | 20/20 |
| obs_category_date_window | 1242.5 | 197.4 | 178.6 | 20/20 |
| obs_code_date_recent | 38.4 | 9.1 | 8.1 | 20/20 |
| encounter_status_date | 266.1 | 9.7 | 9.2 | 20/20 |
| encounter_class_date | 514.9 | 10.0 | 10.9 | 20/20 |
| condition_status_onset | 383.8 | 10.3 | 9.5 | 20/20 |
| report_category_date | 360.7 | 11.8 | 11.6 | 20/20 |
| obs_category_value_quantity | 11.9 | 14.4 | 11.8 | 20/20 |
| obs_code_value_quantity | 322.6 | 9.5 | 9.8 | 20/20 |
| obs_category_date_ancient | 30.3 | 31.4 | 25.5 | 20/20 |
| obs_category_quantity_none | 48.5 | 33.6 | 33.5 | 0/0 |
| encounter_status_date_recent | 36.2 | 12.0 | 9.4 | 20/20 |
| **sum** | 5010 | | 778 (best of 2) | |

No search got slower beyond noise and every entry count is unchanged.
Still the slowest broad shape: a one-month window on a broad category
(`category=vital-signs&date=ge2021-01-01&date=lt2021-02-01`, 179 ms; 1,243 ms
before) — 1.7% of vital-signs rows fall in the window, so the walk probes ~1,250
rows; a same-row range for single-valued date parameters would make it a
direct range scan (upstream issue candidate, see §3 finding 3).

## 5. search_index write amplification

At 2.24M resources `search_index` holds 30.9M rows (13.8 per resource) in
5.3 GB of heap and 11 GB of indexes. Largest contributors: Provenance
`target` 2.2M rows (Synthea writes one Provenance per patient targeting every
resource — data-specific), Observation `combo-code` 931k, `code`/`category`/
`date`/`encounter`/`status`/`subject`/`patient` ~655k each, `_profile` 631k,
the quantity and composite parameters ~2.9M together. Every row is a spec
search parameter; the writer already deduplicates identical rows per resource
and skips `_id`/`_lastUpdated` (answered from `resources`).

Where load time goes (500 patients, 107,775 resources, 8 clients, 37 s,
`pg_stat_statements`): database execution 123 s of ~296 client-seconds (42%);
of that `INSERT INTO search_index` 79.5 s (65%, 72 ms and ~19k buffers per
batched statement), `resource_fts` 22 s (18%), the `resources` insert 18 s
(15%). The rest of each request is HFS-side (parsing, extraction).

Index removal was tested and rejected:

| index (size, scans during the benchmark) | dropped → | verdict |
|---|---|---|
| `idx_search_token_code_recent` (3.1 GB, 3,277) | load 3,027 → 3,072 res/s (+1.5%); the 28 searches unchanged, but OR-lists and system-only tokens 7 ms → 194–487 ms (`category=laboratory,vital-signs`, `status=final,amended`, `code=http://loinc.org\|`, `code=a,b`, `clinical-status=active,recurrence`) | **keep** |
| quantity / composite / canonical indexes (1.5 GB, 0 scans) | not dropped: they serve `value-quantity`, `code-value-quantity` and composite searches the benchmark does not issue (lab thresholds) | keep |

Conclusion: no index on `search_index` is redundant, and dropping the
largest one buys 1.5% load speed — the index count is not the load-speed
limiter on NVMe. Levers that are left, none safe enough to switch on by
default: an opt-out of `resource_fts` for deployments that never use
`_text`/`_content` (18% of load-time DB work), and skipping search parameters a
site never queries (e.g. Claim/ExplanationOfBenefit financial parameters,
`_profile`) via a configurable denylist — both change search behaviour and need
a product decision. Lowering the token statistics target (above) removes the
other hidden write-path cost: autovacuum's ANALYZE of `search_index` drops
from 62–80 s to ~10 s per run.

## 6. Long token lists (`code:in`)

`code:in=<ValueSet>` is resolved by HFS (ValueSet expansion via HTS), and the
search reaches PostgreSQL as a token list of every expanded code. Line G (CQL
against HFS) measured `Condition?subject=&code:in=<451-code VS>` at ~380 ms
against 3–6 ms on HAPI.

Causes, measured on the 2.24M set (`pg_stat_statements`, track_planning):

- one OR arm per code, two bind parameters each, a BitmapOr of N index scans,
  and a new statement text for every list length;
- planning grows with the list: PostgreSQL estimates `value_token_code =
  ANY(<constant array>)` element by element against the column's MCV list
  (1000 entries since hm-0006) — a 5,000-code list planned for 160 ms and
  executed in 2 ms.

Changes (`query_builder.rs`, `search_impl.rs`):

1. A list of ≥ 10 values that are all `system|code` becomes one arm per
   distinct system, `value_token_system IN ($system, marker) AND
   value_token_code = ANY($codes)` (a ScalarArrayOp scan of
   `idx_search_token_code`); all code-only becomes `value_token_code =
   ANY($codes)`. Same semantics as the per-value form (own system or the
   implicit-system marker, #1379), `:not` unchanged; mixed, `|code`, `system|`
   and shorter lists keep the OR form. A first variant checked
   `system || '|' || code = ANY($pairs)`; that expression has no statistics,
   its default selectivity under-estimated a patient's 10-code membership 100×
   and the planner drove the search from the codes (1.3 s) — rejected.
2. A list of more than 1,000 values is first cut to the codes that have any
   `search_index` row for the tenant, type and parameter (one correlated
   probe per distinct code, ~7 µs each; 297 of 5,000 survive here). Absent
   codes cannot change the match, with or without `:not`; a list left empty
   keeps one value so it still matches nothing. Lowering the token statistics
   target to 100 instead (planning 160 → 24 ms) was measured and **rejected**:
   moderately frequent clinical codes fall out of the MCV list and a patient's
   10-code Condition search went from 10 ms to 0.9–2.3 s.

Benchmark (`scripts/ops/codelist-bench.py`; lists = the most frequent real codes
up to half the list, padded with codes that occur nowhere, POSTed to
`_search`), before = `hm/main` `1413d673a`, HTTP medians in ms, two runs each:

| codes | shape | before (hm/main) run1 / run2 | after run1 / run2 | entries |
|---|---|---|---|---|
| 10 | condition_codes | 11.0 / 8.8 | 13.8 / 21.1 | 20/20 |
| 10 | condition_patient_codes | 13.2 / 10.3 | 15.1 / 11.5 | 14/14 |
| 10 | observation_codes_date | 9.2 / 13.3 | 19.5 / 7.1 | 20/20 |
| 10 | observation_patient_codes_date | 50.2 / 49.9 | 57.8 / 35.9 | 50/50 |
| 100 | condition_codes | 15.3 / 12.9 | 17.2 / 14.9 | 20/20 |
| 100 | condition_patient_codes | 27.6 / 15.6 | 20.6 / 15.4 | 20/20 |
| 100 | observation_codes_date | 44.7 / 27.0 | 30.4 / 12.6 | 20/20 |
| 100 | observation_patient_codes_date | 51.0 / 50.4 | 58.1 / 66.4 | 50/50 |
| 500 | condition_codes | 50.0 / 58.0 | 42.5 / 20.8 | 20/20 |
| 500 | condition_patient_codes | 70.0 / 43.4 | 40.8 / 22.3 | 20/20 |
| 500 | observation_codes_date | 178.6 / 133.7 | 67.2 / 45.7 | 20/20 |
| 500 | observation_patient_codes_date | 142.6 / 104.5 | 67.8 / 76.7 | 50/50 |
| 5000 | condition_codes | 465.1 / 431.7 | 113.9 / 94.8 | 20/20 |
| 5000 | condition_patient_codes | 434.0 / 441.3 | 85.3 / 90.1 | 20/20 |
| 5000 | observation_codes_date | 1155.2 / 1133.3 | 142.2 / 134.6 | 20/20 |
| 5000 | observation_patient_codes_date | 604.9 / 473.6 | 125.1 / 80.6 | 50/50 |

The 43-search benchmark on the same database (no search slower beyond noise,
identical entry counts):

| search | before run1 / run2 | after run1 / run2 | entries |
|---|---|---|---|
| patient_by_identifier | 16.0 / 3.8 | 15.8 / 3.4 | 1/1 |
| patient_by_family | 5.5 / 3.0 | 3.7 / 2.9 | 1/1 |
| patient_by_name | 3.4 / 3.1 | 2.8 / 3.0 | 1/1 |
| patient_by_birthdate | 3.7 / 3.1 | 8.4 / 4.7 | 1/1 |
| patient_by_birthdate_range | 63.2 / 32.5 | 59.3 / 31.3 | 6/6 |
| patient_name_and_birthdate | 7.5 / 8.4 | 9.2 / 9.3 | 1/1 |
| obs_patient_code_date | 7.5 / 7.6 | 7.7 / 7.7 | 1/1 |
| obs_patient_sorted | 8.2 / 7.3 | 8.7 / 7.4 | 20/20 |
| obs_patient_category | 6.1 / 6.1 | 7.1 / 6.4 | 7/7 |
| obs_code_date_broad | 13.1 / 12.5 | 17.1 / 12.9 | 50/50 |
| obs_code_count | 22.0 / 24.7 | 25.4 / 24.7 | 0/0 |
| encounter_patient_date | 6.1 / 6.6 | 6.7 / 6.9 | 10/10 |
| encounter_patient_include | 9.9 / 9.9 | 11.1 / 10.5 | 26/26 |
| patient_revinclude_encounter | 7.1 / 6.3 | 7.6 / 7.4 | 21/21 |
| patient_revinclude_obs | 7.5 / 6.7 | 9.2 / 7.4 | 24/24 |
| obs_lastupdated | 4.9 / 4.9 | 6.5 / 5.3 | 50/50 |
| obs_lastupdated_sorted | 4.4 / 4.6 | 6.8 / 5.7 | 50/50 |
| obs_chain_patient_identifier | 13.2 / 11.4 | 14.0 / 11.5 | 23/23 |
| obs_chain_patient_family | 8.8 / 8.8 | 13.5 / 9.6 | 23/23 |
| patient_has_obs_code | 42.2 / 38.3 | 48.0 / 39.3 | 50/50 |
| condition_patient | 5.0 / 5.0 | 7.2 / 3.0 | 20/20 |
| medreq_patient_status | 5.7 / 5.0 | 8.1 / 2.4 | 7/7 |
| patient_everything | 106.9 / 85.0 | 93.1 / 70.8 | 215/215 |
| encounter_status_finished | 7.5 / 5.5 | 7.0 / 5.2 | 20/20 |
| obs_category_laboratory | 6.2 / 4.7 | 6.8 / 5.6 | 20/20 |
| obs_status_final_code | 12.2 / 8.9 | 13.0 / 7.5 | 20/20 |
| obs_category_date | 13.6 / 8.8 | 13.6 / 5.9 | 20/20 |
| condition_code | 4.6 / 5.0 | 7.2 / 2.7 | 20/20 |
| obs_category_date_patient | 9.5 / 8.7 | 10.6 / 8.9 | 7/7 |
| obs_status_date | 11.4 / 9.1 | 14.0 / 6.6 | 20/20 |
| obs_status_date_patient | 11.6 / 10.3 | 13.6 / 6.8 | 20/20 |
| obs_category_date_old | 13.7 / 11.6 | 15.4 / 5.8 | 20/20 |
| obs_category_date_window | 267.5 / 233.9 | 284.5 / 278.8 | 20/20 |
| obs_code_date_recent | 13.1 / 9.1 | 12.3 / 6.4 | 20/20 |
| encounter_status_date | 10.2 / 8.7 | 12.5 / 4.4 | 20/20 |
| encounter_class_date | 11.9 / 8.6 | 14.1 / 11.2 | 20/20 |
| condition_status_onset | 10.5 / 8.3 | 15.0 / 6.2 | 20/20 |
| report_category_date | 11.1 / 10.7 | 16.9 / 4.8 | 20/20 |
| obs_category_value_quantity | 14.3 / 12.7 | 21.8 / 12.0 | 20/20 |
| obs_code_value_quantity | 11.1 / 7.7 | 12.3 / 6.0 | 20/20 |
| obs_category_date_ancient | 32.3 / 31.3 | 31.8 / 26.5 | 20/20 |
| obs_category_quantity_none | 49.9 / 32.8 | 44.3 / 35.3 | 0/0 |
| encounter_status_date_recent | 13.0 / 8.7 | 15.7 / 8.9 | 20/20 |
| **sum** | 913 / 750 | 979 / 749 | |

Line G's shape (`Condition?subject=&code:in=<451>`) corresponds to the
500-code `condition_patient_codes` row: 43–70 ms → 22–41 ms here (the
remaining time is HTTP parsing of the list and the per-system arm; the HTS
expansion itself is cached by HFS for `HFS_SEARCH_EXPANSION_CACHE_TTL_SECS`).

### Terminology timeout and warming

- `HFS_TERMINOLOGY_TIMEOUT_SECS` (default **30**, was a fixed 10 s) is the
  per-request timeout of HFS's terminology client (each `$expand` page). A
  cold HTS expansion of a 4,000–6,000-code eCQM ValueSet exceeded 10 s on a
  loaded host and the search failed closed with 503.
- `scripts/ops/warm-valuesets.sh <HTS base> [url-file]` expands the ValueSets
  `code:in` uses (default: every VSAC/eCQM ValueSet HTS holds) after an HTS
  restart, terminology import or IG install, so the first clinical search or
  measure run finds them in HTS's expansion cache.

## 7. Counting from the index, and orphan index rows (hm-0040)

`_summary=count` and `_total=accurate` on a single search filter
(`Observation?code=…`) are answered from `search_index` alone: the distinct
resource ids of the matching rows. The general statement instead joins
`resources` and checks `is_deleted` once per match, which is what made the
count grow with the number of matches (§7.3).

The two are the same number only if `search_index` holds no *orphan* rows —
rows whose resource is deleted or absent. Three things make that so:

1. **The database removes index rows with the resource.** Migration
   `hm-0040-search-index-liveness` adds row triggers on `resources`: after a
   `DELETE`, and after an `UPDATE` that sets `is_deleted` to true, the
   resource's `search_index` rows are deleted in the same transaction. This
   holds for every path, including retention, purge and hand-written SQL. The
   triggers cost nothing on create or update (`UPDATE OF is_deleted` does not
   fire unless the statement assigns that column); a delete pays one probe of
   `idx_search_resource` per row.
2. **Writers index only what they hold.** Index rows are written in the
   transaction that inserts or updates the resource row, or (reindex, deferred
   bulk indexing) for resources re-read as live under the resource's advisory
   key.
3. **Existing rows are checked before they are trusted.** The first count on a
   `(tenant, resource type)` starts one background statement that looks for
   indexed ids without a live resource. A clean result is recorded in
   `hfs_search_index_liveness` and used by every instance from then on. Until
   it is recorded the count uses the `resources` join, so the answer is exact
   either way.

`AuditEvent` is never counted from the index (audit events are moving to
their own table). A version-restricted count and a count on a legacy-layout
database also keep the join.

### 7.1 When orphan rows are found

The server logs a warning and keeps the joined count for that type; it does
not delete anything:

```
WARN search_index holds rows for resources that are deleted or absent; counts for this type keep the slower resources join until they are removed tenant=default resource_type=Observation orphan_resources=3
```

Look at them, and remove them if they are not wanted (searches already ignore
them — every page statement joins `resources`):

```sql
-- what is there
SELECT s.resource_id, count(*) FROM search_index s
WHERE s.tenant_id = 'default' AND s.resource_type = 'Observation'
  AND NOT EXISTS (SELECT 1 FROM resources r
                  WHERE r.tenant_id = s.tenant_id AND r.resource_type = s.resource_type
                    AND r.id = s.resource_id AND r.is_deleted = FALSE)
GROUP BY 1;
-- remove
DELETE FROM search_index s
WHERE s.tenant_id = 'default' AND s.resource_type = 'Observation'
  AND NOT EXISTS (SELECT 1 FROM resources r
                  WHERE r.tenant_id = s.tenant_id AND r.resource_type = s.resource_type
                    AND r.id = s.resource_id AND r.is_deleted = FALSE);
```

The server re-checks a type with recorded orphans every 10 minutes; to force a
check now, delete its row from `hfs_search_index_liveness`. Deleting a row of
a clean type makes the server verify that type again (instances notice within
60 s).

### 7.2 Switches, rollout, rollback

- `HFS_PG_INDEX_COUNT=off` keeps every count on the `resources` join.
- Rollout: the migration runs at startup and only creates a small table, a
  function and three triggers (`CREATE TRIGGER` takes a brief
  `SHARE ROW EXCLUSIVE` lock on `resources`: writers wait for it, readers do
  not).
  Older binaries keep working against a migrated database. The first count per
  type after the upgrade is as slow as before while its check runs (one pass
  over the type's index rows; the statement runs without a statement timeout).
- Rollback:

```sql
DROP TRIGGER IF EXISTS trg_search_index_follow_delete ON resources;
DROP TRIGGER IF EXISTS trg_search_index_follow_soft_delete ON resources;
DROP TRIGGER IF EXISTS trg_search_index_follow_truncate ON resources;
DROP FUNCTION IF EXISTS hfs_search_index_follow_resource();
DROP TABLE IF EXISTS hfs_search_index_liveness;
DELETE FROM hm_schema_migrations WHERE name = 'hm-0040-search-index-liveness';
```

  A binary that knows the migration finds no marker table and keeps the join.
- What is not covered: index rows inserted by hand for a resource that does
  not exist, after the type was verified. Nothing in the server does that; if
  you do, delete the type's marker row afterwards.

### 7.3 Measured

Benchmark host, 527,113 Synthea resources plus the creates of earlier runs,
`Observation?code=http://loinc.org|72514-3` (17,837 matches), 1 client, 200
requests after 20 warm-ups, median / p95 of the HTTP request:

| | through `resources` (before) | from the index (after) | HAPI 8.12 |
|---|---|---|---|
| `_summary=count` | 114 / 142 ms | 32 / 38 ms | 20 / 25 ms |
| `_total=accurate&_count=50` | 121 / 143 ms | 36 / 43 ms | 62 / 66 ms |

The migration applied to the loaded database in under a second and the
Observation type was verified in the background
before the measured requests.
