# Deployment manual

Two supported layouts:

- **Docker Compose on one host** (`docker/production/`) — the hospital's first
  production deployment; matches how the evaluation stack (`hfs-eval`, :8082)
  runs today, with the production hardening listed below.
- **Kubernetes** (`docker/kubernetes/`) — minimal manifests for sites that
  already operate a cluster with CloudNativePG.

Everything else (PostgreSQL tuning, backups, monitoring, upgrades) is shared
and lives in the other pages of this directory ([index](README.md)).

## 1. Sizing

| | Minimum | Recommended (≤ 10M resources) |
|---|---|---|
| CPU | 8 cores | 16–24 cores |
| RAM | 32 GB | 64 GB (PostgreSQL ~24 GB, HFS 1–6 GB peaks, HTS 1–8 GB during imports) |
| Data disk | SSD, 3 × expected DB size | NVMe; DB ≈ 10 GB per million resources + 5 GB HTS |
| WAL archive + base backups | separate disk or NFS/SMB mount | 2 base backups + 7 days of WAL ≈ 3 × DB size |
| Off-host copy | daily | daily logical dump to the hospital backup system |

HFS idles at ~0.1 GB RSS; large transaction Bundles peak at 3–5 GB. HTS first
boot imports bundled terminologies (THO, ICD-10-CM…) in ~4 min; SNOMED CT and
LOINC imports need several GB of RAM and ~1.4 GB of disk per version.

## 2. Docker Compose

### 2.1 Prepare the host

```bash
# 1. Docker Engine + compose plugin; the host clock on NTP (FHIR lastUpdated, PITR targets)
# 2. Directories (uid 70 = postgres in the alpine image)
sudo install -d -o 70 -g 70 -m 700 /srv/hfs/pgdata /backup/hfs/wal-archive
sudo install -d -m 755 /srv/hfs/terminology-sources /var/lib/node_exporter/textfile
# 3. Terminology source files (SNOMED RF2, LOINC zip, ICD-10 …) into /srv/hfs/terminology-sources
# 4. Configuration
cd docker/production
cp .env.example .env && chmod 600 .env && $EDITOR .env      # every empty value must be filled
sed -i 's/PUBLIC_HOST/fhir.hospital.example/' nginx.conf
```

Images: build with `hfs-eval/deploy/remote-build.sh <checkout> <tag>` (adds
`--features` for R4, postgres, ui, xml, subscriptions; add
`helios-hfs/otel,helios-hts/otel` for tracing) and push them to the hospital
registry; put the exact tag in `.env`. Never deploy `:latest`.

### 2.2 First start

```bash
docker compose up -d db            # initdb creates roles hfs/hts and both databases
docker compose logs db | grep -E 'ready to accept|ERROR'
docker compose up -d hts           # first boot: bundled terminology import (~4 min)
docker compose logs -f hts         # wait for "listening"
docker compose up -d hfs nginx     # first boot seeds 1,372 SearchParameters (~60 s)
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8082/fhir/metadata   # 200
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8082/tx/metadata     # 200
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8082/fhir/metrics    # 404 (internal only)
```

Then: import SNOMED CT / LOINC / ICD-10 ([terminology-updates.md](terminology-updates.md)),
install the IGs ([ig-update-sop.md](ig-update-sop.md)), enable backups
(§2.4) and monitoring (§2.5), and go through the
[go-live checklist](go-live-checklist.md).

### 2.3 TLS and the network

nginx listens on `HTTP_BIND` (default `127.0.0.1:8082`). TLS is terminated by
the hospital's edge proxy / load balancer, which forwards to this port and
must be listed in `TRUSTED_PROXIES` (admin sign-in uses `X-Forwarded-For` for
lockout and the secure cookie). To terminate TLS in nginx instead, add a
`listen 443 ssl` server block with the site certificate, HSTS, and publish 443.

**FHIR REST writes are unauthenticated until SMART on FHIR / OIDC (Phase 3
item 2) is enabled.** Until then the service must be reachable only from the
hospital network segments that need it (firewall at the edge proxy).

### 2.4 Backups (cron)

```cron
# /etc/cron.d/hfs-backup   (as root, or a user in the docker group)
15 1 * * *  root  PG_CONTAINER=hfs-db PGUSER=postgres BACKUP_DIR=/backup/hfs/dumps KEEP_DAYS=14 PROM_TEXTFILE_DIR=/var/lib/node_exporter/textfile /opt/hfs/scripts/ops/pg-backup.sh >> /var/log/hfs-backup.log 2>&1
45 2 * * 0  root  PG_CONTAINER=hfs-db PGUSER=postgres BASEBACKUP_DIR=/backup/hfs/base KEEP_COUNT=2 /opt/hfs/scripts/ops/pg-basebackup.sh >> /var/log/hfs-backup.log 2>&1
30 3 * * *  root  /opt/hfs/scripts/ops/wal-prune.sh /backup/hfs/base /backup/hfs/wal-archive >> /var/log/hfs-backup.log 2>&1
```

Copy `/backup/hfs/dumps` off the host daily (hospital backup agent). Details,
RPO/RTO and the restore drill: [backup-restore.md](backup-restore.md).

### 2.5 Monitoring

```bash
docker compose --profile monitoring up -d
# Grafana: http://127.0.0.1:3000 (admin / GRAFANA_ADMIN_PASSWORD) -> Helios -> "Helios FHIR"
# Prometheus: http://127.0.0.1:9090/alerts
```

Wire `alerting.alertmanagers` in `docker/monitoring/prometheus.yml` to the
hospital's Alertmanager (or run one). See [monitoring.md](monitoring.md).

### 2.6 Day-2 commands

| Task | Command |
|---|---|
| status | `docker compose ps` |
| logs | `docker compose logs --since 1h hfs` |
| restart one service | `docker compose restart hfs` (nginx needs `docker compose restart nginx` after hfs/hts are **recreated**: it resolves their IPs at start) |
| stop everything, keep data | `docker compose down` |
| psql | `docker compose exec db psql -U hfs -d hfs` |
| upgrade | [upgrade.md](upgrade.md) |

## 3. Kubernetes

`docker/kubernetes/` (apply with `kubectl apply -k`):

| File | Contents |
|---|---|
| `postgres-cluster.yaml` | CloudNativePG `Cluster` (2 instances, WAL + base backups to an object store, PITR) and a nightly `ScheduledBackup` |
| `config.yaml` | ConfigMaps with the same settings as the compose file |
| `hfs.yaml`, `hts.yaml` | Deployments (1 replica, `Recreate`), Services, PVCs, probes, non-root security context |
| `ingress.yaml` | ingress-nginx routes `/fhir` → hfs, `/tx` → hts, `/metrics` blocked |

Prerequisites: CloudNativePG operator, a storage class on SSD, an S3-compatible
object store for backups, ingress-nginx, Prometheus operator (for the
PodMonitor), and the `hfs-secrets` secret (see `kustomization.yaml`). CNPG
creates `hfs-db-app` with the `hfs` owner's URI; use its `uri` key for
`hfs-db-url`.

Limitations of the minimal layout:

- **one HFS replica.** Multiple HFS processes can share the database (schema
  migration is serialised by an advisory lock and the write path is
  multi-instance safe), but subscription delivery and bulk-export workers are
  per process until Phase 3 item 4 externalises them. Scale out only after that.
- `Recreate` strategy: an upgrade has a short outage while the new pod runs
  migrations; rolling updates would run old and new side by side against a
  schema the old one may not understand.
- the web consoles (`/ui`, `/ui/hts`) and HTS paging links need the nginx
  rules in `docker/production/nginx.conf` (path rewriting and `sub_filter`);
  for the API-only ingress above they are not routed.
