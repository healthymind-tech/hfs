#!/usr/bin/env python3
"""Print `cargo test` target-selection arguments for ONE workspace package.

    python .github/scripts/hm-test-args.py PACKAGE --docker-free
    python .github/scripts/hm-test-args.py PACKAGE --docker [--include RE] [--exclude RE]

Used by hm-ci.yml (HealthyMind CI) to split the workspace's tests into
GitHub-hosted matrix jobs. It reuses the classification in
docker-free-tests.py (a target "needs Docker" when any file it compiles
mentions testcontainers), restricted to one package:

  --docker-free  `--lib --bins --test <t>...` for every target of PACKAGE that
                 starts no container, then `-- --skip <module>` for library
                 modules that do (same rule as the Windows nightly).
  --docker       only `--test <t>` for the targets of PACKAGE that DO start
                 containers, filtered by --include / --exclude (regular
                 expressions matched against the target name). No `--lib`.

One argument per line, so `mapfile -t` can read it.
"""

import argparse
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("docker_free_tests", HERE / "docker-free-tests.py")
dft = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dft)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("package")
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--docker-free", action="store_true")
    mode.add_argument("--docker", action="store_true")
    ap.add_argument("--include", default=None)
    ap.add_argument("--exclude", default=None)
    a = ap.parse_args()

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    pkgs = [p for p in meta["packages"] if p["name"] == a.package]
    if not pkgs:
        print(f"unknown package {a.package}", file=sys.stderr)
        return 2
    pkg = pkgs[0]
    run, docker, skips = [], [], []
    for t in pkg["targets"]:
        root = Path(t["src_path"])
        if "test" in t["kind"]:
            (docker if dft.needs_docker(dft.module_files(root)) else run).append(t["name"])
        elif {"lib", "rlib", "cdylib"} & set(t["kind"]):
            skips += dft.lib_skips(root, dft.module_files(root))

    inc = re.compile(a.include) if a.include else None
    exc = re.compile(a.exclude) if a.exclude else None

    def keep(name: str) -> bool:
        return (inc is None or inc.search(name)) and (exc is None or not exc.search(name))

    if a.docker:
        sel = sorted(n for n in docker if keep(n))
        if not sel:
            print(f"no Docker-backed targets selected in {a.package}", file=sys.stderr)
            return 1
        out = []
        for n in sel:
            out += ["--test", n]
    else:
        out = ["--lib", "--bins"]
        for n in sorted(n for n in run if keep(n)):
            out += ["--test", n]
        out.append("--")
        for s in sorted(set(skips)):
            out += ["--skip", s]
    print("\n".join(out))
    print(
        f"{a.package}: {len(run)} docker-free targets, {len(docker)} Docker-backed "
        f"({'docker' if a.docker else 'docker-free'} mode)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
