#!/usr/bin/env bash
# Fetch the pinned Analysis API dependency set for the Track B spike.
#
# Every jar is addressed by its exact Maven Central path and verified against
# the sha256 recorded in deps.lock. Nothing is resolved transitively, no
# Gradle, no network access beyond repo1.maven.org. Jars land in
# spike/libs/ which is gitignored — they are never committed.
#
# usage:
#   ./fetch-deps.sh                 # download + verify every pinned jar
#   ./fetch-deps.sh --classpath     # print the runtime classpath (fetching first)
#   ./fetch-deps.sh --classpath-only  # print it without touching the network
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
lock="$here/deps.lock"
dest="$here/spike/libs"
base="${MAVEN_CENTRAL:-https://repo1.maven.org/maven2}"

mode="fetch"
case "${1:-}" in
  --classpath) mode="fetch+cp" ;;
  --classpath-only) mode="cp" ;;
  "") ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

jar_for() { printf '%s/%s' "$dest" "$(basename "$5")"; }

classpath() {
  local out="" scope coord sha size path
  while IFS=$'\t' read -r scope coord sha size path; do
    case "$scope" in runtime) ;; *) continue ;; esac
    out+="${out:+:}$dest/$(basename "$path")"
  done < "$lock"
  printf '%s' "$out"
}

if [[ "$mode" == "cp" ]]; then
  classpath
  exit 0
fi

mkdir -p "$dest"
fetched=0
cached=0
while IFS=$'\t' read -r scope coord sha size path; do
  case "$scope" in runtime|build) ;; *) continue ;; esac
  file="$dest/$(basename "$path")"
  if [[ -f "$file" ]]; then
    have=$(sha256sum "$file" | cut -d' ' -f1)
    if [[ "$have" == "$sha" ]]; then
      cached=$((cached + 1))
      continue
    fi
    echo "re-fetching $coord (checksum drift)" >&2
    rm -f "$file"
  fi
  echo "fetch $coord ($size bytes)"
  curl -fsSL --retry 3 --retry-delay 2 -o "$file.part" "$base/$path"
  got=$(sha256sum "$file.part" | cut -d' ' -f1)
  if [[ "$got" != "$sha" ]]; then
    echo "CHECKSUM MISMATCH for $coord:" >&2
    echo "  expected $sha" >&2
    echo "  actual   $got" >&2
    rm -f "$file.part"
    exit 1
  fi
  mv "$file.part" "$file"
  fetched=$((fetched + 1))
done < "$lock"

count=$(ls -1 "$dest"/*.jar | wc -l)
bytes=$(du -sb "$dest" | cut -f1)
echo "deps: $fetched fetched, $cached already verified, $count jars, $bytes bytes in $dest"

if [[ "$mode" == "fetch+cp" ]]; then
  classpath
fi
