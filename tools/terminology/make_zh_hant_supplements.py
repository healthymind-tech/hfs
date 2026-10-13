#!/usr/bin/env python3
"""Generate zh-Hant CodeSystem *supplements* for ICD-10-CM, ICD-10-PCS and LOINC.

Inputs (all optional — pass whichever you have):

  --mohw-xlsx  PATH   衛福部《中文版 ICD-10-CM/PCS》Excel (sheets ``ICD-10-CM`` and
                      ``ICD-10-PCS``; columns: code, USE, English title, Chinese title,
                      status, revision date).  Read with the standard library only —
                      no openpyxl needed.
  --loinc-csv  PATH   ``taiwan_mapping.csv`` (loinc_code,name_zh,common_name_zh,
                      specimen_type,unit).

Outputs one FHIR *collection* Bundle per supplement into ``--out-dir``::

  icd10cm-zh-hant.bundle.json    supplements http://hl7.org/fhir/sid/icd-10-cm
  icd10pcs-zh-hant.bundle.json   supplements http://www.cms.gov/Medicare/Coding/ICD10
  loinc-zh-hant.bundle.json      supplements http://loinc.org

Each concept carries ``designation[language=zh-Hant, use=display]``.  Load with::

  hts import --format fhir-bundle <file>            # CLI, any size
  curl -X POST $HTS/import --data-binary @<file>    # REST (mind the body-size limit)

Query with ``$lookup?...&useSupplement=<supplement url>`` (and, once supplement
designations are merged generically, plain ``displayLanguage=zh-Hant``).

The files are treated as *untrusted input*: cells are taken as opaque strings,
codes are validated against the ICD-10 code grammar, nothing is eval'd.
"""

from __future__ import annotations

import argparse
import csv
import json
import re
import sys
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path

DESIGNATION_USAGE = "http://terminology.hl7.org/CodeSystem/designation-usage"
SNOMED = "http://snomed.info/sct"
SYNONYM = "900000000000013009"

CANONICAL_BASE = "https://terminology.healthymind.tw/CodeSystem"

ICD10CM_URL = "http://hl7.org/fhir/sid/icd-10-cm"
ICD10PCS_URL = "http://www.cms.gov/Medicare/Coding/ICD10"
LOINC_URL = "http://loinc.org"

CM_CODE_RE = re.compile(r"^[A-Z][0-9][0-9A-Z](\.[0-9A-Z]{1,4})?$")
PCS_CODE_RE = re.compile(r"^[0-9A-HJ-NP-Z]{7}$")
LOINC_CODE_RE = re.compile(r"^[0-9]{1,7}-[0-9]$")

# ── minimal .xlsx reader (stdlib only) ────────────────────────────────────────

NS = {
    "m": "http://schemas.openxmlformats.org/spreadsheetml/2006/main",
    "r": "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    "rel": "http://schemas.openxmlformats.org/package/2006/relationships",
}


def _col_index(cell_ref: str) -> int:
    """``"C12"`` → 2 (zero-based column)."""
    n = 0
    for ch in cell_ref:
        if ch.isalpha():
            n = n * 26 + (ord(ch.upper()) - 64)
        else:
            break
    return n - 1


def _shared_strings(zf: zipfile.ZipFile) -> list[str]:
    try:
        data = zf.read("xl/sharedStrings.xml")
    except KeyError:
        return []
    out: list[str] = []
    for _, el in ET.iterparse(zipfile_io(data), events=("end",)):
        if el.tag == f"{{{NS['m']}}}si":
            out.append("".join(t.text or "" for t in el.iter(f"{{{NS['m']}}}t")))
            el.clear()
    return out


def zipfile_io(data: bytes):
    import io

    return io.BytesIO(data)


def _sheet_paths(zf: zipfile.ZipFile) -> dict[str, str]:
    """sheet name → zip member path."""
    wb = ET.fromstring(zf.read("xl/workbook.xml"))
    rels = ET.fromstring(zf.read("xl/_rels/workbook.xml.rels"))
    rid_to_target = {
        r.get("Id"): r.get("Target") for r in rels.findall("rel:Relationship", NS)
    }
    out = {}
    for sh in wb.find("m:sheets", NS):
        rid = sh.get(f"{{{NS['r']}}}id")
        target = rid_to_target.get(rid, "")
        target = target.lstrip("/")
        if not target.startswith("xl/"):
            target = "xl/" + target
        out[sh.get("name")] = target
    return out


def iter_sheet_rows(xlsx: Path, sheet_name: str):
    """Yield each row of ``sheet_name`` as a list of cell strings (sparse → '')."""
    with zipfile.ZipFile(xlsx) as zf:
        strings = _shared_strings(zf)
        paths = _sheet_paths(zf)
        if sheet_name not in paths:
            raise SystemExit(f"sheet {sheet_name!r} not found; have {sorted(paths)}")
        data = zf.read(paths[sheet_name])
    row_tag = f"{{{NS['m']}}}row"
    c_tag = f"{{{NS['m']}}}c"
    v_tag = f"{{{NS['m']}}}v"
    is_tag = f"{{{NS['m']}}}is"
    t_tag = f"{{{NS['m']}}}t"
    for _, el in ET.iterparse(zipfile_io(data), events=("end",)):
        if el.tag != row_tag:
            continue
        cells: list[str] = []
        for c in el.findall(c_tag):
            idx = _col_index(c.get("r", ""))
            while len(cells) <= idx:
                cells.append("")
            kind = c.get("t")
            if kind == "s":
                v = c.find(v_tag)
                cells[idx] = strings[int(v.text)] if v is not None and v.text else ""
            elif kind == "inlineStr":
                node = c.find(is_tag)
                cells[idx] = "".join(t.text or "" for t in node.iter(t_tag)) if node is not None else ""
            else:
                v = c.find(v_tag)
                cells[idx] = (v.text or "") if v is not None else ""
        yield cells
        el.clear()


# ── supplement builders ───────────────────────────────────────────────────────


def designation(value: str, language: str = "zh-Hant", use_code: str | None = "display"):
    d = {"language": language, "value": value}
    if use_code == "display":
        d["use"] = {"system": DESIGNATION_USAGE, "code": "display", "display": "Display"}
    elif use_code == "synonym":
        d["use"] = {"system": SNOMED, "code": SYNONYM, "display": "Synonym"}
    return d


def supplement_resource(
    *,
    res_id: str,
    name: str,
    title: str,
    version: str,
    supplements: str,
    concepts: list[dict],
    properties: list[dict] | None = None,
    description: str = "",
) -> dict:
    cs = {
        "resourceType": "CodeSystem",
        "id": res_id,
        "url": f"{CANONICAL_BASE}/{res_id}",
        "version": version,
        "name": name,
        "title": title,
        "status": "active",
        "experimental": False,
        "publisher": "healthymind-tech (HFS fork)",
        "description": description,
        "language": "zh-Hant",
        "content": "supplement",
        "supplements": supplements,
        "count": len(concepts),
        "concept": concepts,
    }
    if properties:
        cs["property"] = properties
    return cs


def bundle(resources: list[dict]) -> dict:
    return {
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [{"resource": r} for r in resources],
    }


def build_mohw(xlsx: Path, sheet: str, code_re: re.Pattern, report: dict) -> list[dict]:
    concepts: list[dict] = []
    seen: set[str] = set()
    skipped = 0
    for i, row in enumerate(iter_sheet_rows(xlsx, sheet)):
        if i == 0:
            continue  # header
        code = (row[0] if len(row) > 0 else "").strip()
        zh = (row[3] if len(row) > 3 else "").strip()
        if not code or not zh or not code_re.match(code):
            skipped += 1
            continue
        if code in seen:
            skipped += 1
            continue
        seen.add(code)
        concepts.append({"code": code, "designation": [designation(zh)]})
    report[sheet] = {"concepts": len(concepts), "skipped_rows": skipped}
    return concepts


def build_loinc(csv_path: Path, report: dict) -> tuple[list[dict], list[dict]]:
    concepts: list[dict] = []
    with csv_path.open(newline="", encoding="utf-8-sig") as fh:
        for row in csv.DictReader(fh):
            code = (row.get("loinc_code") or "").strip()
            if not LOINC_CODE_RE.match(code):
                continue
            desigs = []
            name_zh = (row.get("name_zh") or "").strip()
            if name_zh:
                desigs.append(designation(name_zh))
            common = (row.get("common_name_zh") or "").strip()
            if common:
                lang = "en" if common.isascii() else "zh-Hant"
                desigs.append(designation(common, language=lang, use_code="synonym"))
            props = []
            if (row.get("specimen_type") or "").strip():
                props.append({"code": "specimen-type", "valueString": row["specimen_type"].strip()})
            if (row.get("unit") or "").strip():
                props.append({"code": "unit", "valueString": row["unit"].strip()})
            concept = {"code": code}
            if desigs:
                concept["designation"] = desigs
            if props:
                concept["property"] = props
            concepts.append(concept)
    properties = [
        {"code": "specimen-type", "description": "常用檢體類型 (zh-Hant)", "type": "string"},
        {"code": "unit", "description": "台灣常用報告單位", "type": "string"},
    ]
    report["loinc"] = {"concepts": len(concepts)}
    return concepts, properties


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mohw-xlsx", type=Path)
    ap.add_argument("--loinc-csv", type=Path)
    ap.add_argument("--version", default="2024-11-18", help="CodeSystem.version for the MOHW supplements")
    ap.add_argument("--loinc-version", default="2026-10", help="CodeSystem.version for the LOINC supplement")
    ap.add_argument("--out-dir", type=Path, required=True)
    args = ap.parse_args()

    if not args.mohw_xlsx and not args.loinc_csv:
        ap.error("give --mohw-xlsx and/or --loinc-csv")
    args.out_dir.mkdir(parents=True, exist_ok=True)
    report: dict = {}

    if args.mohw_xlsx:
        cm = build_mohw(args.mohw_xlsx, "ICD-10-CM", CM_CODE_RE, report)
        pcs = build_mohw(args.mohw_xlsx, "ICD-10-PCS", PCS_CODE_RE, report)
        desc = (
            "衛生福利部《2023年中文版 ICD-10-{kind}》中文名稱，以 CodeSystem supplement "
            "形式提供 zh-Hant designation。來源：衛福部中央健康保險署公告檔 1131118V3。"
        )
        (args.out_dir / "icd10cm-zh-hant.bundle.json").write_text(
            json.dumps(
                bundle([
                    supplement_resource(
                        res_id="icd-10-cm-zh-hant",
                        name="ICD10CMZhHant",
                        title="ICD-10-CM 中文名稱 (zh-Hant) supplement",
                        version=args.version,
                        supplements=ICD10CM_URL,
                        concepts=cm,
                        description=desc.format(kind="CM"),
                    )
                ]),
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        (args.out_dir / "icd10pcs-zh-hant.bundle.json").write_text(
            json.dumps(
                bundle([
                    supplement_resource(
                        res_id="icd-10-pcs-zh-hant",
                        name="ICD10PCSZhHant",
                        title="ICD-10-PCS 中文名稱 (zh-Hant) supplement",
                        version=args.version,
                        supplements=ICD10PCS_URL,
                        concepts=pcs,
                        description=desc.format(kind="PCS"),
                    )
                ]),
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )

    if args.loinc_csv:
        concepts, properties = build_loinc(args.loinc_csv, report)
        (args.out_dir / "loinc-zh-hant.bundle.json").write_text(
            json.dumps(
                bundle([
                    supplement_resource(
                        res_id="loinc-zh-hant",
                        name="LOINCZhHant",
                        title="LOINC 台灣常用檢驗項目中文名稱 (zh-Hant) supplement",
                        version=args.loinc_version,
                        supplements=LOINC_URL,
                        concepts=concepts,
                        properties=properties,
                        description=(
                            "台灣常用檢驗 LOINC 中文名稱、常用簡稱、檢體類型與單位，"
                            "以 CodeSystem supplement 形式提供 zh-Hant designation。"
                        ),
                    )
                ]),
                ensure_ascii=False,
                indent=2,
            ),
            encoding="utf-8",
        )

    json.dump(report, sys.stdout, ensure_ascii=False, indent=2)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
