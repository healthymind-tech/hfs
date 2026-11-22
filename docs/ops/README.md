# Operations documentation

For running HFS (FHIR server) and HTS (terminology server) on PostgreSQL in a
hospital production environment.

| Page | For |
|---|---|
| [go-live-checklist.md](go-live-checklist.md) | everything that must be true before cut-over |
| [deployment.md](deployment.md) | Docker Compose (`docker/production/`) and Kubernetes (`docker/kubernetes/`) |
| [postgresql.md](postgresql.md) | server parameters, connection pools, timeouts, roles, maintenance |
| [database-migrations.md](database-migrations.md) | how schema versions work (upstream ladder + fork track), adding a migration |
| [index-strategy.md](index-strategy.md) | search index review on 1.5M+ resources, EXPLAIN results |
| [backup-restore.md](backup-restore.md) | logical dumps, WAL archiving + PITR, drill timings, RPO/RTO |
| [monitoring.md](monitoring.md) | Prometheus / Grafana / alerts / OTLP traces and the alert runbook |
| [upgrade.md](upgrade.md) | image upgrades, verification, rollback |
| [ci.md](ci.md) | HealthyMind CI on GitHub-hosted runners, coverage, disabled upstream workflows |
| [upstream-rebase.md](upstream-rebase.md) | bringing in upstream releases (v0.2.4 → v0.2.8 plan) |
| [ig-update-sop.md](ig-update-sop.md) | changing installed Implementation Guides |
| [terminology-release-calendar.md](terminology-release-calendar.md) | yearly terminology calendar and change SOP |
| [terminology-updates.md](terminology-updates.md) | per-terminology import commands |
| [ig-packages.md](ig-packages.md) | the package installer itself |
| [admin-sign-in.md](admin-sign-in.md) | administrator sign-in for the consoles and admin APIs |

Scripts: `scripts/ops/` — `pg-backup.sh`, `pg-restore.sh`, `pg-basebackup.sh`,
`pg-pitr-restore.sh`, `wal-prune.sh`, `search-bench.py`.
