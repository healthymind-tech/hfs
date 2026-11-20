#!/usr/bin/env bash
# Docker-free `cargo test` selection for several packages at once (hm-ci.yml):
# the union of hm-test-args.py --docker-free for each package, harness args
# (`--skip ...`) after one `--`.
set -euo pipefail
here=$(dirname "$0")
targets=(); skips=()
for p in "$@"; do
  mapfile -t a < <(python3 "$here/hm-test-args.py" "$p" --docker-free)
  after=0
  for x in "${a[@]}"; do
    if [ "$x" = "--" ]; then after=1; continue; fi
    if [ $after = 1 ]; then skips+=("$x"); else targets+=("$x"); fi
  done
done
# --lib/--bins once; --test pairs de-duplicated.
printf '%s\n' --lib --bins
for ((i = 0; i < ${#targets[@]}; i++)); do
  if [ "${targets[$i]}" = "--test" ]; then echo "--test"; echo "${targets[$((i + 1))]}"; i=$((i + 1)); fi
done | paste - - | sort -u | tr '\t' '\n'
echo --
printf '%s\n' "${skips[@]}" | paste - - | sort -u | tr '\t' '\n'
