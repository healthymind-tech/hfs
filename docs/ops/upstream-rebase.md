# Following upstream: rebase / merge plan

The fork (`origin` = healthymind-tech/hfs, branch `hm/main`) is based on
upstream tag **v0.2.4** (`bcd7b84fb`, 2026-10-01) of `upstream` =
HeliosSoftware/hfs. This page records where upstream is, what bringing it in
costs, and the procedure. It was written on 2026-11-15 from `hm/p25-z`
(`610e60a0b`, which contains all of X/Y/Z); nothing here has been executed.

## 1. What upstream shipped since v0.2.4

Upstream now releases **weekly**:

| Tag | Date | Non-merge commits since previous tag | Files changed |
|---|---|---|---|
| v0.2.5 | 2026-10-18 | 488 | 2,611 |
| v0.2.6 | 2026-10-25 | 140 | 319 |
| v0.2.7 | 2026-11-01 | 225 | 1,983 |
| v0.2.8 | 2026-11-08 | 343 | 2,313 |
| `upstream/main` | 2026-11-15 | +185 after v0.2.8 | |

v0.2.4 → v0.2.8 in total: 1,764 commits, 3,096 files, +546k / −239k lines.
By type: 406 fix, 190 test, 84 ci, 79 docs, 76 perf, 67 feat. By area:
persistence (113 fixes, 14 features — much of it MongoDB/S3/Elasticsearch,
which the hospital deployment does not use), ui (65 fixes, 19 features —
resource editor, bulk export pages), rest, sof, fhirpath, hts (19 fixes plus
VCL support in `$expand` / `$validate-code`).

Items that matter for this deployment:

- **PostgreSQL schema v47–v55** (see §3): ranged `$reindex` index, composite
  ordinals, `idx_resources_live_tenant`, newest-first date index, SQL-on-FHIR
  terminology tables, `bulk_manifests` deferred-rebuild marker, composite
  indexes partial on `is_contained`, `bulk_submissions(updated_at)`, and
  `value_reference_target` (absolute-URL references now match `subject=Patient/x`
  and `_revinclude`). v55 backfills a column over `search_index`: expect minutes
  per 10M index rows.
- `$reindex` id ranges / `clearOnly`, `Patient/$everything` narrowed to the
  SMART launch patient, W3C `traceparent` joining in the request span,
  PostgreSQL pool metrics (already cherry-picked into the fork:
  `feat(observability): expose PostgreSQL pool size…`).
- Upstream removed the top-level `docs/` directory (moved to `crates/*/docs/`).

## 2. What the fork carries

`v0.2.4..hm/p25-z`: 357 non-merge commits + 26 merge commits, 420 files,
+108.6k / −7.1k lines. Mostly HTS (81 fix + 22 feat + 9 perf), the FHIR
validator (77 fix/feat, Phase 2.5 Y), REST/UI, zh-Hant locale, Taiwan examples.

A trial merge (`git merge-tree --write-tree hm/p25-z v0.2.8`, no working tree
touched) reports **106 conflicted paths**:

| Area | Paths | Nature |
|---|---|---|
| crates/hts (+hts-ui) | 32 | both sides rewrote the PG/SQLite backends, import, `$expand`/`$validate-code` — the hardest part |
| crates/persistence | 21 | PG schema/storage/search writer, SQLite search; fork perf work vs upstream v47–v55 |
| crates/ui | 16 | templates, assets, locales |
| crates/rest | 10 | config, search, batch, validation, terminology client |
| crates/fhir-validator | 10 | fork Phase 2.5 Y vs upstream fixes; packs (`*.json.gz`) regenerate rather than merge |
| crates/fhirpath | 4 | polymorphic access, evaluator |
| docs / skills / README / Cargo.lock / locales | 13 | 4 "file location" conflicts: fork `docs/ops/*` added inside a directory upstream moved; keep them at `docs/ops/` |

## 3. Schema compatibility (fixed in this branch)

The fork shipped a migration under upstream's number **v47**; upstream's v47 is
something else. `hm/p3-pgops` moves fork migrations to their own named track
(`hm_schema_migrations`) and re-stamps fork-v47 databases to v46 at startup
([database-migrations.md](database-migrations.md)). **This must be merged and
deployed to production before any upstream release is brought in**, so that
production is stamped 46 when upstream's v47..v55 arrive. Verify on
production: `SELECT version FROM schema_version` → 46 and
`SELECT name FROM hm_schema_migrations` → `hm-0001-search-parameter-epochs`.

During the rebase, `schema.rs` resolves as: upstream's ladder and
`SCHEMA_VERSION` (55) verbatim, plus the fork's `FORK_MIGRATIONS` machinery and
its `hm-0001` body, plus the fork-v47 re-stamp guard (still needed for any
database that has not seen the fork track yet). Re-run the four
`postgres_integration_*fork*` / `*newer_schema*` tests and add one that starts
from a v46 + hm-0001 database and migrates to 55.

## 4. Recommendation: merge, not commit-by-commit rebase, this time

`ROADMAP-fork.md` assumed a rebase per release at 0.5–2 days. With 357 fork
commits, 26 internal merges and five weeks of upstream at ~300 commits/week,
a commit-by-commit rebase would replay the same HTS and persistence conflicts
dozens of times.

| Option | Work | Estimate |
|---|---|---|
| A. `git rebase --rebase-merges v0.2.8` | conflicts resolved per commit, 357 picks; history stays linear on top of the tag | 8–12 engineer-days, high risk of mis-resolution in HTS |
| **B. one merge of v0.2.8 into a branch off `hm/main`** | 106 paths resolved once; history keeps fork commits as they are | **3–5 engineer-days** (1–2 conflicts, 1 build/test fix-up, 1 full regression + PG upgrade rehearsal, 0.5 deploy) |
| C. squash fork into ~15 topic commits, then rebase | one-off cost of B plus regrouping | 6–8 days; makes later rebases cheaper |

Choose **B now**, then keep pace **weekly**: merge each upstream tag within a
week of release (≈ 0.5–1 day each while the delta is ~200–400 commits). Revisit
C only if upstream PRs land enough fork work to shrink the delta.

Shrink the delta before merging — upstream PRs worth opening first (each
removes conflicts in the next merge): the PG search-index long-value cut
(`42e7d9a0d`), OperationDefinition routing fix, SearchParameter epoch
(fork `hm-0001`; upstream may want it as its own migration), HTS tx-ecosystem
fixes that are not Taiwan-specific.

## 5. Procedure (option B)

```bash
git -C hfs fetch upstream --tags
git -C hfs worktree add ../hfs-merge-028 -b hm/merge-v0.2.8 hm/main
cd ../hfs-merge-028
git merge --no-ff v0.2.8            # resolve; keep fork docs at docs/ops/
#   crates/fhir-validator/packs/*.json.gz: take upstream's, then regenerate with
#   the fork generator (see crates/fhir-validator/README) and commit separately
#   Cargo.lock: take upstream's, then `cargo update -w` and build
rcargo.sh ../hfs-merge-028 -- build --workspace
rcargo.sh ../hfs-merge-028 -- test --workspace            # SQLite + unit
# PostgreSQL crate tests (persistence, hts with HTS_TEST_BACKEND=postgres)
# eval regressions: test_validator.py, test_hts.py, test_hfs_server.py, tx-ecosystem runner, difftest
```

Then the PostgreSQL upgrade rehearsal (mandatory for this merge because of
v47–v55):

1. restore the latest production dump into a scratch PostgreSQL
   (`scripts/ops/pg-restore.sh`), start the merged HFS on it, time the
   migration (log lines `Migration v54->v55` etc.), compare row counts and run
   the search benchmark (`scripts/ops/search-bench.py`) against the pre-merge
   numbers;
2. start the merged HTS on the restored `hts` and run the tx-ecosystem runner;
3. only then follow [upgrade.md](upgrade.md) on production.

Rollback is the pre-upgrade dump: once v47+ has run, a pre-merge image built
from `hm/p3-pgops` or later refuses to start against the database (by design,
see `HFS_PG_ALLOW_NEWER_SCHEMA`); images older than that would start and
silently miss the new columns, so never roll back by image tag alone.

## 6. Executed: v0.2.8 + v0.2.9 (2026-11-16, branch `hm/upstream-merge`)

Option B was taken: one merge of `v0.2.8` into a branch off production
(`1413d673a`), then `origin/hm/main` (`b75c853c5`), then `v0.2.9` as a
second merge commit. 168 conflicted paths for v0.2.8, 26 for v0.2.9; the
fork's `docs/ops/` stays here (`.gitignore` now ignores `/docs/*` except
these pages). Every decision where both sides had built the same thing, the
test numbers, the migration rehearsal on the production dump and the
deploy/rollback notes are in `hfs-eval/eval/results-upstream/REPORT.md`.

Lessons for the next weekly merge:

- auto-merged `Cargo.toml` files can silently drop a dependency the fork still
  uses (roxmltree in `helios-fhirpath`); `cargo check --workspace --all-targets`
  with the production features catches it;
- upstream tests often assume upstream's harness (shared PG database per
  suite, single-connection backends, statement_timeout ending lock waits);
  those are adapted or ignored with a reason naming the fork commit;
- regenerate the validator schema packs after every merge that touches the
  converter (`generate-schema-packs R4 R4B R5`).
