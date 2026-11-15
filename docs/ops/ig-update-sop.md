# IG (FHIR package) update SOP

Mechanics of installing, upgrading and removing FHIR packages are in
[ig-packages.md](ig-packages.md). This page is the change procedure for the
hospital's production server.

## 1. When

- a new version of an IG the hospital uses (TW Core, 健保 PAS/EMR/CI/NGS, IPS…),
  typically announced by MOHW / NHI with an effective date;
- a new integration that needs a new IG;
- a dependency bump required by one of the above.

## 2. Procedure

1. **Ticket**: package `name#version`, reason, effective date, which systems
   send data conforming to it, whether the old version must stay installed
   (data validated against old profiles keeps `meta.profile` pointing at the
   old canonical version).
2. **Dry run on production** (read-only):
   ```bash
   curl -u admin:$ADMIN_PASSWORD -X POST https://<host>/fhir/admin/packages \
     -H 'Content-Type: application/json' -d '{"package":"tw.gov.mohw.twcore#1.1.0","dryRun":true}'
   ```
   Review the plan: new dependencies, resources replaced, SearchParameters
   added (a new SearchParameter on a populated resource type needs `$reindex`
   of that type to find existing data), terminology sent to HTS.
3. **Rehearse** on a production copy when the plan adds SearchParameters or
   replaces profiles in use: restore `hfs` + `hts` dumps into a scratch
   PostgreSQL, run the install there, then the IG's examples through
   `$validate` (`eval/scripts/p25-z/matrix.py` / `lifecycle.py`) and a
   sample of real (de-identified) payloads from the sending systems.
4. **Backup**: `pg-backup.sh` (both databases — terminology lands in `hts`).
5. **Install** in a quiet period (the install writes hundreds of resources and
   may trigger reindex jobs):
   ```bash
   curl -u admin:$ADMIN_PASSWORD -X POST 'https://<host>/fhir/admin/packages?wait=300' \
     -H 'Content-Type: application/json' -d '{"package":"tw.gov.mohw.twcore#1.1.0","dryRun":false}'
   ```
   Offline hospital: upload the `.tgz` (ig-packages.md "Offline hospitals").
6. **Post-install**: if new SearchParameters target populated types, run
   `$reindex` for those types and watch `PostgresLongTransaction` / load;
   rehydrate profiles if the deployment script normally does
   (`eval/scripts/rehydrate_profiles.py` on the evaluation stack).
7. **Verify**: `GET /fhir/admin/packages` lists the version; `$validate` one
   example per profile the hospital uses; HTS `$expand` of the IG's key
   ValueSets; Grafana error ratio flat.
8. **Record** the install job id, duration, and the verification output.
9. **Rollback**: `DELETE /fhir/admin/packages/{name}/{version}` removes exactly
   what the package owns; if a dependency upgrade replaced shared resources,
   restore from the backup of step 4 instead.
