# CI on the HealthyMind fork

The upstream Helios workflows were written for Helios Software's self-hosted
runners (`runs-on: [self-hosted, Linux]`, a remote Docker host behind the
`DOCKER_HOST` secret, a macOS agent, a deploy agent). This repository has **no
self-hosted runners**, so those jobs queue until GitHub cancels them after
24 hours. The fork's gating CI is therefore its own workflow on GitHub-hosted
runners: **`.github/workflows/hm-ci.yml` ("HealthyMind CI")**.

## HealthyMind CI (`hm-ci.yml`)

Triggers: pull requests to `main`, pushes to `main` and `hm/main`, manual
dispatch. One run per ref (a new push cancels the running one).

| Job | What it runs |
|---|---|
| `rustfmt` | `cargo fmt --all -- --check` |
| `clippy (production features)` | `cargo clippy --locked --all-targets -p helios-hfs -p helios-hts --features $PROD_FEATURES -- $CLIPPY_ARGS` |
| `clippy (all features)` | `cargo clippy --locked --all-targets --all-features -- $CLIPPY_ARGS` (what upstream's ci.yml lint runs) |
| `test (<shard>)` ×10 | `cargo llvm-cov --no-report --locked <packages> [--features …] <targets>` per crate group, then `cargo llvm-cov report --lcov` |
| `coverage summary` | merges the shards' lcov files, writes a per-crate line-coverage table to the job summary, uploads `coverage-report` (merged `lcov.info` + `coverage.md`) |

`PROD_FEATURES` is the production image's feature list (kept in sync by hand
with `hfs-eval/deploy/remote-build.sh`). `CLIPPY_ARGS` is upstream's allow-list.

Test shards (all build R4 with the crate's default features plus the named
backend features, i.e. the deployed configuration; other FHIR versions and
backends are compiled by the all-features clippy):

| Shard | Packages | Notes |
|---|---|---|
| core | helios-fhir, -fhir-macro, -fhirpath-support, -serde, -serde-support, -cds-hooks, -observability | |
| fhirpath+sof | helios-fhirpath, helios-sof | |
| persistence-sqlite | helios-persistence | targets that start no container (`hm-test-args.py --docker-free`) |
| persistence-postgres | helios-persistence `postgres` | `--lib` + the PostgreSQL integration targets against the `postgres:16` service (`HFS_TEST_PG_HOST`) |
| rest+hfs | helios-rest, helios-hfs | targets that start no container |
| rest-postgres | helios-rest `postgres` | the Docker-backed targets except MongoDB / Elasticsearch / MinIO / S3; testcontainers on the runner's Docker |
| hts-sqlite / hts-postgres | helios-hts (`postgres`) | the PostgreSQL run uses `HTS_TEST_PG_URL` (service container) |
| fhir-validator | helios-fhir-validator `fhirpath,xml` | |
| services+ui | helios-auth, -admin-auth, -audit, -subscriptions, -ui, -ui-chrome, -hts-ui | admin-auth / subscriptions with `postgres`; the email suite starts mailpit with testcontainers |

Not run here: MongoDB, Elasticsearch, MinIO/S3 container suites (production
uses none of them), the Playwright browser suites, release builds, Inferno and
the other external smoke workflows. The Windows nightly (below) still runs the
whole Docker-free workspace selection with `--all-features`.

Runner sizing: `ubuntu-latest` has 4 vCPU, 16 GB RAM and ~14 GB free disk.
Every heavy job first runs `.github/scripts/hm-free-disk.sh` (removes .NET,
Android, GHC, CodeQL, … — about 30 GB more), builds without debuginfo
(`CARGO_PROFILE_DEV_DEBUG=0`) and caches dependencies with
`Swatinem/rust-cache` (one cache per job).

Toolchain: pinned to **1.100.0** with `RUSTUP_TOOLCHAIN` (rust-toolchain.toml
says `stable`; the variable wins). To bump: change `RUSTUP_TOOLCHAIN` and the
three `toolchain:` inputs in `hm-ci.yml`, fix any new clippy lints in the same PR.

### Reproduce a shard locally

```bash
# selection for a shard (needs cargo on PATH)
python3 .github/scripts/hm-test-args.py helios-persistence --docker-free
.github/scripts/hm-select-multi.sh helios-rest helios-hfs
# PostgreSQL shards: any PostgreSQL 16 with superuser postgres/postgres
HFS_TEST_PG_HOST=127.0.0.1 HFS_TEST_PG_PORT=5432 \
  cargo test --locked -p helios-persistence --features helios-persistence/postgres --test postgres_tests
HTS_TEST_PG_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo test --locked -p helios-hts --features helios-hts/postgres
# coverage for one shard
cargo llvm-cov --locked -p helios-auth --lcov --output-path lcov.info
python3 .github/scripts/hm-coverage-summary.py merged.info coverage.md lcov.info
```

## Upstream workflows disabled in this repository

Disabled with `gh workflow disable` (a repository Actions setting; the files
are unchanged, so upstream merges do not conflict). All of these run only on
self-hosted runners and are triggered automatically (pull request, push,
schedule or comments), so they would sit queued on every PR or show as
cancelled every day.

| Workflow file | Name | Automatic triggers | Disabled |
|---|---|---|---|
| ci.yml | HFS Continuous Integration | push, pull_request, schedule | before the fork (already `disabled_manually`) |
| ci-extended.yml | HFS Extended CI (Beta/Nightly/MSRV) | schedule | 2026-11-20 |
| claude-code-review.yml | Claude Code Review | pull_request (also needs a `CLAUDE_CODE_OAUTH_TOKEN` secret) | 2026-11-20 |
| claude.yml | Claude Code | issue / review comments | 2026-11-20 |
| hts-ig-conformance.yml | HTS Terminology IG Conformance | schedule | 2026-11-20 |
| hts.yml | HTS Terminology Server CI | schedule | 2026-11-20 |
| inferno-us-core.yml | Inferno US Core Test Suite | schedule | 2026-11-20 |
| inferno-us-quality-core.yml | Inferno US Quality Core Test Suite | schedule | 2026-11-20 |
| keycloak-smoke.yml | Keycloak smoke (UI self-call) | schedule | 2026-11-20 |
| release-build.yml | Release build | pull_request (self-hosted build legs) | 2026-11-20 |
| ui-tests-matrix.yml | UI Tests (backend matrix) | push, schedule | 2026-11-20 |
| ui-tests.yml | UI Tests | pull_request | 2026-11-20 |
| validator-conformance.yml | Validator Conformance | pull_request, schedule | 2026-11-20 |

Re-enable one (e.g. after registering a self-hosted runner with the labels
`self-hosted, Linux` and Docker):

```bash
gh workflow enable -R healthymind-tech/hfs ui-tests.yml
gh workflow list -R healthymind-tech/hfs --all     # shows active / disabled_manually
```

Left enabled on purpose:

- **build-image.yml** — the team's CD image build (ubuntu-24.04).
- **windows-nightly.yml** — its `Test Rust (Windows, no Docker)` and
  `Test Python` jobs run on `windows-latest`; its `Linting (clippy, Windows
  target)` job is self-hosted and stays queued until cancelled.
- **fhir-bench-tests.yml** — benchmark script tests and shellcheck/actionlint,
  on `ubuntu-latest`.
- **hm-ci.yml** — this fork's CI.
- The `workflow_dispatch`-only / `workflow_call` workflows (audit-events,
  bulk-export-smoke, bulk-submit-smoke, cluster-smoke, deploy-server,
  fhir-benchmark, hts-benchmark, inferno-bulk-data, inferno-subscription,
  subscriptions-channels, subscriptions-smoke, validator-differential): they
  never start on their own, so they do not affect PRs. A manual dispatch would
  queue forever for lack of a self-hosted runner.
