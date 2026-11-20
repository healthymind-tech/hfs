#!/usr/bin/env python3
"""Merge lcov files from the HealthyMind CI test shards and summarise line
coverage per crate.

    python .github/scripts/hm-coverage-summary.py OUT.info OUT.md IN.info...

Lines are merged per (file, line) by summing hit counts, so a line counts as
covered when ANY shard executed it. Only files under `crates/<name>/src/` are
reported (integration tests, examples, generated build output are not).
Writes the merged lcov to OUT.info, a Markdown table to OUT.md, and a TSV
(crate, lines, covered, percent) to stdout.
"""

import re
import sys
from collections import defaultdict

CRATE = re.compile(r"(?:^|/)crates/([^/]+)/src/")


def parse(path, hits):
    sf = None
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("SF:"):
                sf = line[3:]
            elif line.startswith("DA:") and sf is not None:
                parts = line[3:].split(",")
                try:
                    ln, cnt = int(parts[0]), int(parts[1])
                except (ValueError, IndexError):
                    continue
                hits[sf][ln] += cnt
            elif line == "end_of_record":
                sf = None


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    out_info, out_md, inputs = sys.argv[1], sys.argv[2], sys.argv[3:]
    hits = defaultdict(lambda: defaultdict(int))
    for p in inputs:
        parse(p, hits)

    with open(out_info, "w", encoding="utf-8") as f:
        for sf in sorted(hits):
            f.write("TN:\nSF:%s\n" % sf)
            lines = hits[sf]
            for ln in sorted(lines):
                f.write("DA:%d,%d\n" % (ln, lines[ln]))
            f.write("LF:%d\nLH:%d\nend_of_record\n" % (len(lines), sum(1 for c in lines.values() if c > 0)))

    per = defaultdict(lambda: [0, 0])
    for sf, lines in hits.items():
        m = CRATE.search(sf.replace("\\", "/"))
        if not m:
            continue
        per[m.group(1)][0] += len(lines)
        per[m.group(1)][1] += sum(1 for c in lines.values() if c > 0)

    tot_l = sum(v[0] for v in per.values())
    tot_c = sum(v[1] for v in per.values())
    rows = sorted(per.items())
    md = ["| crate | lines | covered | line % |", "|---|---:|---:|---:|"]
    for name, (l, c) in rows:
        pct = 100.0 * c / l if l else 0.0
        md.append(f"| {name} | {l} | {c} | {pct:.1f} |")
        print(f"{name}\t{l}\t{c}\t{pct:.1f}")
    pct = 100.0 * tot_c / tot_l if tot_l else 0.0
    md.append(f"| **total** | {tot_l} | {tot_c} | **{pct:.1f}** |")
    print(f"total\t{tot_l}\t{tot_c}\t{pct:.1f}")
    with open(out_md, "w", encoding="utf-8") as f:
        f.write("\n".join(md) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
