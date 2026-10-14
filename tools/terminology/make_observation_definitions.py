#!/usr/bin/env python3
"""Build ObservationDefinition resources (R4) from Taiwan lab reference ranges.

Inputs:
  --ranges  examples/tw/lab-reference-ranges.csv
            loinc_code,test_name_zh,test_name_en,age_min,age_max,gender,
            range_low,range_high,unit,interpretation,source
  --mapping examples/tw/taiwan-loinc-mapping.csv  (optional; adds the common
            Taiwanese abbreviation and specimen type)

Output: a FHIR *transaction* Bundle (``PUT ObservationDefinition/tw-lab-<loinc>``)
with one ObservationDefinition per LOINC code.  Every row of the ranges CSV
becomes one ``qualifiedInterval`` (category=reference, context=normal, gender
and age applied).  ``range_low == 0`` / ``range_high == 999`` are treated as
"no bound" and omitted.  Units are mapped to UCUM where the CSV spelling is not
already valid UCUM (``sec`` → ``s``, ``ratio`` → ``{ratio}``, ``μIU/mL`` → ``u[IU]/mL``).

R4 ObservationDefinition has no standard ``code`` search parameter; the sibling
file ``examples/tw/searchparameter-observationdefinition-code.json`` defines one
for HFS.  PUT it *before* the Bundle so the resources are indexed on create.
"""

from __future__ import annotations

import argparse
import csv
import json
import sys
from pathlib import Path

LOINC = "http://loinc.org"
UCUM = "http://unitsofmeasure.org"
OBS_CATEGORY = "http://terminology.hl7.org/CodeSystem/observation-category"
RANGE_MEANING = "http://terminology.hl7.org/CodeSystem/referencerange-meaning"
DESIGNATION_USAGE = "http://terminology.hl7.org/CodeSystem/designation-usage"

UCUM_FIX = {
    "sec": "s",
    "ratio": "{ratio}",
    "μIU/mL": "u[IU]/mL",
    "uIU/mL": "u[IU]/mL",
    "mL/min/1.73m2": "mL/min/{1.73_m2}",
}


def ucum(unit: str) -> str:
    return UCUM_FIX.get(unit, unit)


def quantity(value: str, unit: str) -> dict:
    return {"value": float(value), "unit": unit, "system": UCUM, "code": ucum(unit)}


def load_mapping(path: Path | None) -> dict[str, dict]:
    if not path:
        return {}
    with path.open(newline="", encoding="utf-8-sig") as fh:
        return {r["loinc_code"].strip(): r for r in csv.DictReader(fh)}


def build(ranges: Path, mapping: dict[str, dict]) -> list[dict]:
    per_code: dict[str, dict] = {}
    with ranges.open(newline="", encoding="utf-8-sig") as fh:
        for row in csv.DictReader(fh):
            code = row["loinc_code"].strip()
            od = per_code.get(code)
            if od is None:
                m = mapping.get(code, {})
                unit = row["unit"].strip()
                od = {
                    "resourceType": "ObservationDefinition",
                    "id": f"tw-lab-{code}",
                    "language": "zh-Hant",
                    "identifier": [
                        {"system": "https://terminology.healthymind.tw/identifier/lab-test", "value": code}
                    ],
                    "category": [
                        {"coding": [{"system": OBS_CATEGORY, "code": "laboratory", "display": "Laboratory"}]}
                    ],
                    "code": {
                        "coding": [
                            {"system": LOINC, "code": code, "display": row["test_name_en"].strip()}
                        ],
                        "text": row["test_name_zh"].strip(),
                    },
                    "permittedDataType": ["Quantity"],
                    "multipleResultsAllowed": False,
                    "preferredReportName": row["test_name_zh"].strip(),
                    "quantitativeDetails": {
                        "unit": {"system": UCUM, "code": ucum(unit), "display": unit}
                    },
                    "qualifiedInterval": [],
                }
                if m.get("common_name_zh"):
                    od["code"]["coding"].append(
                        {
                            "system": "https://terminology.healthymind.tw/CodeSystem/lab-abbreviation",
                            "code": m["common_name_zh"].strip(),
                            "display": m["name_zh"].strip(),
                        }
                    )
                if m.get("specimen_type"):
                    od["extension"] = [
                        {
                            "url": "https://terminology.healthymind.tw/StructureDefinition/lab-specimen-type",
                            "valueString": m["specimen_type"].strip(),
                        }
                    ]
                per_code[code] = od

            interval: dict = {
                "category": "reference",
                "context": {
                    "coding": [{"system": RANGE_MEANING, "code": "normal", "display": "Normal Range"}]
                },
            }
            rng: dict = {}
            low, high, unit = row["range_low"].strip(), row["range_high"].strip(), row["unit"].strip()
            if low and float(low) != 0:
                rng["low"] = quantity(low, unit)
            if high and float(high) != 999:
                rng["high"] = quantity(high, unit)
            if rng:
                interval["range"] = rng
            gender = row["gender"].strip().upper()
            if gender == "M":
                interval["gender"] = "male"
            elif gender == "F":
                interval["gender"] = "female"
            age_min, age_max = row["age_min"].strip(), row["age_max"].strip()
            if age_min or age_max:
                age: dict = {}
                if age_min:
                    age["low"] = {"value": float(age_min), "unit": "a", "system": UCUM, "code": "a"}
                if age_max and float(age_max) < 120:
                    age["high"] = {"value": float(age_max), "unit": "a", "system": UCUM, "code": "a"}
                interval["age"] = age
            cond = " — ".join(x for x in (row["interpretation"].strip(), row["source"].strip()) if x)
            if cond:
                interval["condition"] = cond
            od["qualifiedInterval"].append(interval)
    return list(per_code.values())


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ranges", type=Path, required=True)
    ap.add_argument("--mapping", type=Path)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()

    ods = build(args.ranges, load_mapping(args.mapping))
    bundle = {
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "fullUrl": f"urn:uuid:{od['id']}",
                "resource": od,
                "request": {"method": "PUT", "url": f"ObservationDefinition/{od['id']}"},
            }
            for od in ods
        ],
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(bundle, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"{len(ods)} ObservationDefinitions, {sum(len(o['qualifiedInterval']) for o in ods)} intervals → {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
