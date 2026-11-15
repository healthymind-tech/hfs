#!/usr/bin/env bash
# Warm HTS's expansion cache for the ValueSets that `code:in` searches use, so
# the first clinical search (or CQL measure run) after a restart, an HTS
# terminology import or an IG install does not pay a multi-second cold
# expansion inside HFS's terminology timeout (HFS_TERMINOLOGY_TIMEOUT_SECS).
#
# Usage: warm-valuesets.sh HTS_BASE_URL [URL_FILE]
#   HTS_BASE_URL  e.g. http://hts:8090 (inside the compose network) or https://host/tx
#   URL_FILE      one ValueSet canonical per line ('#' comments allowed); default:
#                 every ValueSet HTS holds whose URL starts with
#                 http://cts.nlm.nih.gov/fhir/ValueSet/ (the eCQM/VSAC sets)
#
# Each ValueSet is expanded with the page size HFS uses (count=1000), the
# same request HFS sends, so HTS caches exactly what HFS will ask for. Prints
# one line per ValueSet with the HTTP status and seconds taken.
set -euo pipefail
BASE=${1:?HTS base URL}; FILE=${2:-}
urls() {
  if [ -n "$FILE" ]; then grep -v '^\s*#' "$FILE" | sed '/^\s*$/d'
  else
    off=0
    while :; do
      page=$(curl -sf "$BASE/ValueSet?_elements=url&_count=500&_offset=$off" -H 'Accept: application/fhir+json') || break
      got=$(printf '%s' "$page" | python3 -c 'import json,sys; b=json.load(sys.stdin); [print(e["resource"].get("url","")) for e in b.get("entry",[])]')
      [ -z "$got" ] && break
      printf '%s\n' "$got" | grep '^http://cts.nlm.nih.gov/fhir/ValueSet/' || true
      off=$((off + 500))
    done
  fi
}
n=0; fail=0
while read -r url; do
  [ -z "$url" ] && continue
  out=$(curl -s -o /dev/null -w '%{http_code} %{time_total}' --max-time 300 -G "$BASE/ValueSet/\$expand" \
        --data-urlencode "url=$url" --data-urlencode "count=1000" --data-urlencode "excludeNested=true" \
        -H 'Accept: application/fhir+json')
  echo "$out $url"; n=$((n + 1)); [ "${out%% *}" = 200 ] || fail=$((fail + 1))
done < <(urls)
echo "warmed $n ValueSets, $fail failed"
[ "$fail" = 0 ]
