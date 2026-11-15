#!/usr/bin/env python3
"""Time the common FHIR search patterns against a running HFS (PostgreSQL).

Usage:
  search-bench.py --base http://127.0.0.1:8080 --psql "docker exec -i hfs-db psql -U hfs -d hfs" \
      [--runs 7] [--out results.json] [--only NAME,NAME] [--explain-log-cmd "docker logs --since {since} hfs-db"]

Picks realistic parameters from the database itself (a patient with a median
number of observations, one of their identifiers, a common LOINC code, ...),
then runs every search --runs times over one keep-alive connection and reports
min / median / p95 wall time and the number of entries returned. The first run
of each search is a cold-ish run and is reported separately.

With --explain-log-cmd (or --explain-log-file for a server logging to a local
file), the server is expected to have auto_explain enabled
(auto_explain.log_min_duration=0, log_analyze=on, log_buffers=on): after the
timed runs every search is issued once more and the plans logged meanwhile are
written next to --out as <out>.plans.txt, one section per search.

Standard library only; responses are parsed as JSON data only.
"""
import argparse
import http.client
import json
import shlex
import statistics
import subprocess
import sys
import time
import urllib.parse

ap = argparse.ArgumentParser()
ap.add_argument("--base", required=True)
ap.add_argument("--psql", required=True, help="command prefix that runs psql against the hfs database, reading SQL on stdin")
ap.add_argument("--runs", type=int, default=7)
ap.add_argument("--out", default="search-bench.json")
ap.add_argument("--only", default="")
ap.add_argument("--explain-log-cmd", default="")
ap.add_argument("--explain-log-file", default="", help="PostgreSQL log file that auto_explain writes to (read from its current end)")
ap.add_argument("--tenant", default="default")
a = ap.parse_args()


def sql(q):
    r = subprocess.run(shlex.split(a.psql) + ["-At", "-F", "\t"], input=q, capture_output=True, text=True, check=True)
    return [line.split("\t") for line in r.stdout.strip().splitlines() if line]


def one(q):
    rows = sql(q)
    return rows[0][0] if rows else None


T = a.tenant
# A patient near the median observation count (not an outlier in either direction).
pat = one(f"""
WITH c AS (
  SELECT split_part(value_reference, '/', 2) AS pid, count(*) n
  FROM search_index WHERE tenant_id = '{T}' AND resource_type = 'Observation' AND param_name = 'subject'
    AND value_reference LIKE 'Patient/%'
  GROUP BY 1)
SELECT pid FROM c ORDER BY n OFFSET (SELECT count(*) / 2 FROM c) LIMIT 1""")
ident = sql(f"""SELECT value_token_system, value_token_code FROM search_index
  WHERE tenant_id = '{T}' AND resource_type = 'Patient' AND resource_id = '{pat}' AND param_name = 'identifier'
    AND value_token_system IS NOT NULL ORDER BY value_token_system LIMIT 1""")[0]
family = one(f"""SELECT value_string FROM search_index WHERE tenant_id = '{T}' AND resource_type = 'Patient'
  AND resource_id = '{pat}' AND param_name = 'family' LIMIT 1""")
birth = one(f"""SELECT to_char(value_date, 'YYYY-MM-DD') FROM search_index WHERE tenant_id = '{T}'
  AND resource_type = 'Patient' AND resource_id = '{pat}' AND param_name = 'birthdate' LIMIT 1""")
code = sql(f"""SELECT value_token_system, value_token_code, count(*) FROM search_index
  WHERE tenant_id = '{T}' AND resource_type = 'Observation' AND param_name = 'code' AND value_token_system = 'http://loinc.org'
  GROUP BY 1, 2 ORDER BY 3 DESC OFFSET 5 LIMIT 1""")[0]
recent = one(f"""SELECT to_char((max(last_updated) - interval '1 hour') AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
  FROM resources WHERE tenant_id = '{T}' AND resource_type = 'Observation'""")
params = {"patient": pat, "id_system": ident[0], "id_value": ident[1], "family": family, "birthdate": birth,
          "code_system": code[0], "code": code[1], "code_count": int(code[2]), "recent": recent}
print("parameters:", json.dumps(params), flush=True)

q = urllib.parse.quote
P, IDS, IDV, FAM, BD = pat, params["id_system"], params["id_value"], family, birth
CS, CD, REC = params["code_system"], params["code"], recent
year = BD[:4]
SEARCHES = [
    ("patient_by_identifier", f"/Patient?identifier={q(IDS + '|' + IDV)}"),
    ("patient_by_family", f"/Patient?family={q(FAM)}&_count=50"),
    ("patient_by_name", f"/Patient?name={q(FAM)}&_count=50"),
    ("patient_by_birthdate", f"/Patient?birthdate={BD}&_count=50"),
    ("patient_by_birthdate_range", f"/Patient?birthdate=ge{year}-01-01&birthdate=lt{year}-02-01&_count=50"),
    ("patient_name_and_birthdate", f"/Patient?family={q(FAM)}&birthdate={BD}"),
    ("obs_patient_code_date", f"/Observation?patient=Patient/{P}&code={q(CS + '|' + CD)}&date=ge2010-01-01&_count=50"),
    ("obs_patient_sorted", f"/Observation?subject=Patient/{P}&_sort=-date&_count=20"),
    ("obs_patient_category", f"/Observation?patient={P}&category=vital-signs&_count=50"),
    ("obs_code_date_broad", f"/Observation?code={q(CS + '|' + CD)}&date=ge2019-01-01&_count=50"),
    ("obs_code_count", f"/Observation?code={q(CS + '|' + CD)}&_summary=count"),
    ("encounter_patient_date", f"/Encounter?patient=Patient/{P}&date=ge2010-01-01&_count=50"),
    ("encounter_patient_include", f"/Encounter?patient=Patient/{P}&_include=Encounter:service-provider&_include=Encounter:participant&_count=50"),
    ("patient_revinclude_encounter", f"/Patient?_id={P}&_revinclude=Encounter:patient"),
    ("patient_revinclude_obs", f"/Patient?_id={P}&_revinclude=Observation:subject&_count=200"),
    ("obs_lastupdated", f"/Observation?_lastUpdated=ge{REC}&_count=50"),
    ("obs_lastupdated_sorted", f"/Observation?_sort=-_lastUpdated&_count=50"),
    ("obs_chain_patient_identifier", f"/Observation?patient.identifier={q(IDS + '|' + IDV)}&_count=50"),
    ("obs_chain_patient_family", f"/Observation?subject:Patient.family={q(FAM)}&_count=50"),
    ("patient_has_obs_code", f"/Patient?_has:Observation:patient:code={q(CS + '|' + CD)}&_count=50"),
    ("condition_patient", f"/Condition?patient=Patient/{P}"),
    ("medreq_patient_status", f"/MedicationRequest?patient=Patient/{P}&status=active"),
    ("patient_everything", f"/Patient/{P}/$everything?_count=1000"),
    # broad, highly correlated token memberships: the shapes the search_index
    # extended statistics were introduced for (schema v15 / v16)
    ("encounter_status_finished", "/Encounter?status=finished&_count=20"),
    ("obs_category_laboratory", "/Observation?category=laboratory&_count=20"),
    ("obs_status_final_code", f"/Observation?status=final&code={q(CS + '|' + CD)}&_count=20"),
    ("obs_category_date", "/Observation?category=vital-signs&date=ge2020-01-01&_count=20"),
    ("condition_code", "/Condition?code=http://snomed.info/sct|44054006&_count=20"),
    # token + range conjunctions, with and without a patient: the broad-walk
    # driver choice depends on the range membership's row estimate
    ("obs_category_date_patient", f"/Observation?patient={P}&category=vital-signs&date=ge2020-01-01&_count=20"),
    ("obs_status_date", "/Observation?status=final&date=ge2020-01-01&_count=20"),
    ("obs_status_date_patient", f"/Observation?patient={P}&status=final&date=ge2020-01-01&_count=20"),
    ("obs_category_date_old", "/Observation?category=laboratory&date=le2012-01-01&_count=20"),
    ("obs_category_date_window", "/Observation?category=vital-signs&date=ge2021-01-01&date=lt2021-02-01&_count=20"),
    ("obs_code_date_recent", f"/Observation?code={q(CS + '|' + CD)}&date=ge2022-01-01&_count=20"),
    ("encounter_status_date", "/Encounter?status=finished&date=ge2020-01-01&_count=20"),
    ("encounter_class_date", "/Encounter?class=AMB&date=ge2020-01-01&_count=20"),
    ("condition_status_onset", "/Condition?clinical-status=active&onset-date=ge2015-01-01&_count=20"),
    ("report_category_date", "/DiagnosticReport?category=LAB&date=ge2020-01-01&_count=20"),
    ("obs_category_value_quantity", "/Observation?category=vital-signs&value-quantity=gt100&_count=20"),
    ("obs_code_value_quantity", f"/Observation?code={q(CS + '|' + CD)}&value-quantity=gt20&_count=20"),
    # adversarial: both memberships broad, the intersection small or empty
    ("obs_category_date_ancient", "/Observation?category=vital-signs&date=le1975-01-01&_count=20"),
    ("obs_category_quantity_none", "/Observation?category=survey&value-quantity=gt1000&_count=20"),
    ("encounter_status_date_recent", "/Encounter?status=finished&date=ge2026-10-01&_count=20"),
]
if a.only:
    keep = set(a.only.split(","))
    SEARCHES = [s for s in SEARCHES if s[0] in keep]

u = urllib.parse.urlparse(a.base)
base_path = u.path.rstrip("/")


def get(conn, path):
    t0 = time.perf_counter()
    conn.request("GET", base_path + path, headers={"Accept": "application/fhir+json"})
    r = conn.getresponse()
    body = r.read()
    dt = (time.perf_counter() - t0) * 1000
    entries = total = None
    if r.status == 200:
        try:
            js = json.loads(body)
            entries = len(js.get("entry", []))
            total = js.get("total")
        except ValueError:
            pass
    return r.status, dt, entries, total


conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=300)
results = []
for name, path in SEARCHES:
    status, first, entries, total = get(conn, path)
    times = []
    for _ in range(a.runs):
        status, dt, entries, total = get(conn, path)
        times.append(dt)
    times.sort()
    res = {"name": name, "path": path, "status": status, "entries": entries, "total": total,
           "first_ms": round(first, 1), "min_ms": round(times[0], 1),
           "median_ms": round(statistics.median(times), 1),
           "p95_ms": round(times[min(len(times) - 1, int(round(0.95 * (len(times) - 1))))], 1)}
    results.append(res)
    print(f"{name:32s} {status} entries={entries!s:>5} first={first:8.1f} median={res['median_ms']:8.1f} p95={res['p95_ms']:8.1f} ms", flush=True)

json.dump({"base": a.base, "params": params, "runs": a.runs, "results": results}, open(a.out, "w"), indent=1)

if a.explain_log_file:
    out = open(a.out.rsplit(".", 1)[0] + ".plans.txt", "w")
    for name, path in SEARCHES:
        with open(a.explain_log_file, "rb") as f:
            f.seek(0, 2)
            start = f.tell()
        get(conn, path)
        time.sleep(1.0)
        with open(a.explain_log_file, "rb") as f:
            f.seek(start)
            chunk = f.read().decode("utf-8", "replace")
        out.write(f"\n===== {name}  {path}\n{chunk}")
    out.close()
    print("plans written to", out.name)
elif a.explain_log_cmd:
    out = open(a.out.rsplit(".", 1)[0] + ".plans.txt", "w")
    for name, path in SEARCHES:
        since = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(time.time() - 1))
        time.sleep(1.1)
        get(conn, path)
        time.sleep(1.5)
        logs = subprocess.run(shlex.split(a.explain_log_cmd.format(since=since)), capture_output=True, text=True)
        out.write(f"\n===== {name}  {path}\n")
        out.write(logs.stdout + logs.stderr)
    out.close()
    print("plans written to", out.name)
