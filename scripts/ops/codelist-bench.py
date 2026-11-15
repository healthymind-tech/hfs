#!/usr/bin/env python3
"""Time token searches with long code lists (what `code:in=<ValueSet>` becomes
after HFS expands the ValueSet) against a running HFS on PostgreSQL.

Usage:
  codelist-bench.py --base http://127.0.0.1:8080 --psql "docker exec -i hfs-db psql -U hfs -d hfs" \
      [--sizes 10,100,500,5000] [--runs 5] [--out codelist.json]

Each list holds the most frequent real codes of the resource type (up to half
the list) padded with codes that exist nowhere, the way an eCQM ValueSet is
mostly codes a given hospital never recorded. Searches are POSTed to
`/{type}/_search` (a 5000-code list does not fit in a URL). Shapes: the list
alone, with a patient, with a date, and with a patient and a date.
Standard library only; responses are parsed as JSON data only.
"""
import argparse, http.client, json, shlex, statistics, subprocess, time, urllib.parse

ap = argparse.ArgumentParser()
ap.add_argument("--base", required=True)
ap.add_argument("--psql", required=True)
ap.add_argument("--sizes", default="10,100,500,5000")
ap.add_argument("--runs", type=int, default=5)
ap.add_argument("--out", default="codelist.json")
ap.add_argument("--tenant", default="default")
a = ap.parse_args()


def sql(q):
    r = subprocess.run(shlex.split(a.psql) + ["-At", "-F", "\t"], input=q, capture_output=True, text=True, check=True)
    return [line.split("\t") for line in r.stdout.strip().splitlines() if line]


T = a.tenant
pat = sql(f"""SELECT split_part(value_reference, '/', 2) FROM search_index
  WHERE tenant_id='{T}' AND resource_type='Condition' AND param_name='subject' AND value_reference LIKE 'Patient/%'
  GROUP BY 1 ORDER BY count(*) DESC, 1 OFFSET 50 LIMIT 1""")[0][0]
real = {}
for rt in ("Condition", "Observation"):
    real[rt] = [f"{s}|{c}" for s, c, _ in sql(f"""SELECT value_token_system, value_token_code, count(*) FROM search_index
      WHERE tenant_id='{T}' AND resource_type='{rt}' AND param_name='code' AND value_token_system IS NOT NULL
      GROUP BY 1, 2 ORDER BY 3 DESC, 2""")]
fake_system = {"Condition": "http://snomed.info/sct", "Observation": "http://loinc.org"}
print("patient", pat, "distinct codes:", {k: len(v) for k, v in real.items()}, flush=True)


def codes(rt, n):
    take = real[rt][: max(1, min(len(real[rt]), n // 2))]
    pad = [f"{fake_system[rt]}|99{rt[:3].upper()}{i:07d}" for i in range(n - len(take))]
    return take + pad


u = urllib.parse.urlparse(a.base)
base_path = u.path.rstrip("/")
conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=600)


def post(rt, form):
    body = urllib.parse.urlencode(form)
    t0 = time.perf_counter()
    conn.request("POST", f"{base_path}/{rt}/_search", body,
                 {"Content-Type": "application/x-www-form-urlencoded", "Accept": "application/fhir+json"})
    r = conn.getresponse(); data = r.read()
    dt = (time.perf_counter() - t0) * 1000
    n = None
    if r.status == 200:
        try: n = len(json.loads(data).get("entry", []))
        except ValueError: pass
    return r.status, dt, n


results = []
for size in [int(x) for x in a.sizes.split(",")]:
    shapes = [
        ("condition_codes", "Condition", [("code", ",".join(codes("Condition", size))), ("_count", "20")]),
        ("condition_patient_codes", "Condition", [("patient", pat), ("code", ",".join(codes("Condition", size)))]),
        ("observation_codes_date", "Observation", [("code", ",".join(codes("Observation", size))), ("date", "ge2020-01-01"), ("_count", "20")]),
        ("observation_patient_codes_date", "Observation", [("patient", pat), ("code", ",".join(codes("Observation", size))), ("date", "ge2015-01-01"), ("_count", "50")]),
    ]
    for name, rt, form in shapes:
        status, first, n = post(rt, form)
        times = sorted(post(rt, form)[1] for _ in range(a.runs))
        res = {"size": size, "name": name, "status": status, "entries": n, "first_ms": round(first, 1),
               "median_ms": round(statistics.median(times), 1), "p95_ms": round(times[-1], 1)}
        results.append(res)
        print(f"{size:5d} {name:32s} {status} entries={n!s:>4} first={first:8.1f} median={res['median_ms']:8.1f} max={res['p95_ms']:8.1f} ms", flush=True)
json.dump({"patient": pat, "results": results}, open(a.out, "w"), indent=1)
