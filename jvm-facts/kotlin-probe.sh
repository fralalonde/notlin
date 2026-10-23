#!/usr/bin/env bash
# Probe suite for the Kotlin facts mode of the jvm-facts helper (Track B, plan 3.2).
#
#   ./kotlin-probe.sh            # build + every probe (bench probes skipped if absent)
#   ./kotlin-probe.sh --no-build # reuse build/ and the deps already fetched
#
# Prints one block per acceptance probe:
#   [1] build: slim jar unchanged (class set + class bytes), full jar built
#   [2] Ping on the slim jar and on the full jar
#   [3] ResolveCalls on the spike 2-file fixture (must match docs/analysis-api-spike.md)
#   [4] ResolveCalls on bench files (stdlib/receiver calls: let/takeIf/add), with timing
#   [5] EOF shutdown: slim, full without --kotlin, full with --kotlin (all < 2 s)
#   [6] slim-jar regression: java.math.BigDecimal divide rows still 6
#
# Nothing here is a unit test; it is the evidence pasted into the commit body.
# Requirements: JDK 25 (JAVA=... to override), the pinned deps (`./fetch-deps.sh`),
# python3 for the EOF timing measurement. Bench probes need .hermes/bench/run,
# which is untracked bench data — absent means those probes are skipped, not failed.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
here=$(pwd)
repo=$(cd .. && pwd)
JAVA="${JAVA:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/java}"
slim=build/notlin-jvm-facts.jar
full=build/notlin-jvm-facts-full.jar

if [[ "${1:-}" != "--no-build" ]]; then
  echo "[1] build"
  bash build.sh
  echo "    slim entries: $(unzip -l "$slim" | awk 'NR>3 && NF>=4 {print $4}' | grep -c '\.class$') class files"
  echo "    full entries: $(unzip -l "$full" | awk 'NR>3 && NF>=4 {print $4}' | grep -c '\.class$') class files"
  echo "    jars on the full jar's Class-Path: $(unzip -p "$full" META-INF/MANIFEST.MF | tr -d '\r' | awk '/^Class-Path:/{sub(/^Class-Path: /,"");cp=$0} /^ /{sub(/^ /,"");cp=cp $0} END{print cp}' | tr ' ' '\n' | grep -c '\.jar$')"
else
  echo "[1] build: skipped (--no-build)"
fi

echo
echo "[2] Ping"
printf '{"version":1,"id":1,"query":{"Ping":null}}\n' | "$JAVA" -jar "$slim" 2>/dev/null | sed 's/^/    slim: /'
printf '{"version":1,"id":1,"query":{"Ping":null}}\n' | "$JAVA" -jar "$full" 2>/dev/null | sed 's/^/    full: /'
echo "    ResolveCalls without --kotlin is declined per request (NoEvidence, never silence):"
printf '{"version":1,"id":2,"query":{"ResolveCalls":{"file":"/tmp/nope.kt"}}}\n' \
  | "$JAVA" -jar "$full" 2>&1 >/dev/null | sed 's/^/    stderr: /'
printf '{"version":1,"id":2,"query":{"ResolveCalls":{"file":"/tmp/nope.kt"}}}\n' \
  | "$JAVA" -jar "$full" 2>/dev/null | sed 's/^/    stdout: /'
echo "    (a NoEvidence line above = declined, not a stalled helper)"

echo
echo "[3] ResolveCalls on the spike fixture (expect 3 Established, offsets 406/496/582)"
fixture="$here/spike/fixtures/src/main"
printf '{"version":1,"id":1,"query":{"ResolveCalls":{"file":"%s/Site.kt"}}}\n' "$fixture" \
  | NOTLIN_KOTLIN_SOURCES="$fixture" "$JAVA" -jar "$full" --kotlin 2>/tmp/notlin-probe-3.log \
  | python3 -c '
import json, sys
answer = json.loads(sys.stdin.read().strip())
sites = answer["answer"]["CallSites"]["sites"]
print("    file: " + answer["answer"]["CallSites"]["file"])
for site in sites:
    print("    %d %-11s %-38s %-6s %s %s null=%s" % (
        site["byte_offset"], site["status"], site["callee_owner"], site["callee_name"],
        site["parameter_types"], site["jvm_descriptor"], site["result_nullable"]))
print("    sites=" + str(len(sites)) + ", established=" + str(sum(1 for s in sites if s["status"] == "Established")))
'
grep -E 'session ready|ResolveCalls|stdin closed' /tmp/notlin-probe-3.log | sed 's/^/    /'

echo
echo "[4] ResolveCalls on the bench slice (jackson-module-kotlin, 23 files)"
bench="$repo/.hermes/bench"
if [[ -d "$bench/run/src/main/kotlin" ]]; then
  bench_cp=""
  if [[ -f "$bench/jars/classpath.txt" ]]; then
    while IFS= read -r jar; do
      [[ -f "$bench/jars/$jar" ]] && bench_cp+="${bench_cp:+:}$bench/jars/$jar"
    done < <(tr ':' '\n' < "$bench/jars/classpath.txt")
  fi
  # kotlin-reflect is not in the bench classpath file but the sources need it.
  [[ -f spike/libs/kotlin-reflect-2.3.21.jar ]] \
    && bench_cp+="${bench_cp:+:}$here/spike/libs/kotlin-reflect-2.3.21.jar"
  root="$bench/run/src/main/kotlin"
  requests=/tmp/notlin-probe-4.req
  {
    printf '{"version":1,"id":1,"query":{"ResolveCalls":{"file":"%s/com/fasterxml/jackson/module/kotlin/ReflectionCache.kt"}}}\n' "$root"
    printf '{"version":1,"id":2,"query":{"ResolveCalls":{"file":"%s/com/fasterxml/jackson/module/kotlin/KotlinNamesAnnotationIntrospector.kt"}}}\n' "$root"
    printf '{"version":1,"id":3,"query":{"ResolveCalls":{"file":"%s/com/fasterxml/jackson/module/kotlin/ReflectionCache.kt"}}}\n' "$root"
  } > "$requests"
  start=$(date +%s%N)
  NOTLIN_KOTLIN_SOURCES="$root" NOTLIN_CLASSPATH="$bench_cp" \
    "$JAVA" -jar "$full" --kotlin < "$requests" > /tmp/notlin-probe-4.out 2>/tmp/notlin-probe-4.log
  end=$(date +%s%N)
  echo "    wall: $(( (end - start) / 1000000 )) ms for 3 batches in one session ($(echo "$bench_cp" | tr ':' '\n' | wc -l) classpath jars)"
  grep -E 'session ready|ResolveCalls' /tmp/notlin-probe-4.log | sed 's/^/    /'
  python3 - <<'PY'
import json
from collections import Counter
for line in open('/tmp/notlin-probe-4.out'):
    answer = json.loads(line)
    sites = answer["answer"]["CallSites"]["sites"]
    name = answer["answer"]["CallSites"]["file"].rsplit('/', 1)[-1]
    print("    id=%s %-40s sites=%3d %s" % (answer["id"], name, len(sites), dict(Counter(s["status"] for s in sites))))
    wanted = {"let", "takeIf", "add", "get", "isKotlinClass", "getOrNull", "runCatching", "<init>"}
    shown = 0
    for site in sites:
        if site["status"] != "Established" or site["callee_name"] not in wanted:
            continue
        if shown >= 6:
            break
        shown += 1
        print("        %5d %-13s %-52s %-8s %-34s %s" % (
            site["byte_offset"], site["callee_name"],
            (site["callee_owner"] or "<null>"),
            (site["parameter_types"][0] if site["parameter_types"] else ""),
            site["result_type"], site["jvm_descriptor"]))
PY
else
  echo "    skipped: $bench/run/src/main/kotlin not present"
fi

echo
echo "[5] EOF shutdown timing (must be < 2 s for all three modes)"
python3 - "$JAVA" "$here" "$fixture" <<'PY'
import os, subprocess, sys, time

java, here, fixture = sys.argv[1], sys.argv[2], sys.argv[3]
slim = os.path.join(here, "build/notlin-jvm-facts.jar")
full = os.path.join(here, "build/notlin-jvm-facts-full.jar")
ping = b'{"version":1,"id":1,"query":{"Ping":null}}\n'


def measure(name, argv, env=None):
    started = time.time()
    process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, env=env)
    process.stdin.write(ping)
    process.stdin.flush()
    answered = process.stdout.readline()
    answered_at = time.time()
    process.stdin.close()
    code = process.wait(timeout=30)
    exited_at = time.time()
    print("    %-28s answer in %5.2f s, EOF -> exit %5.2f s (rc=%d, pong=%s)"
          % (name, answered_at - started, exited_at - answered_at, code, bool(answered)))


measure("slim", [java, "-jar", slim])
measure("full (no --kotlin)", [java, "-jar", full])
env = dict(os.environ, NOTLIN_KOTLIN_SOURCES=fixture)
measure("full --kotlin", [java, "-jar", full, "--kotlin"], env)
PY

echo
echo "[6] slim-jar regression: ClassMembers(java.math.BigDecimal) divide rows"
printf '{"version":1,"id":1,"query":{"ClassMembers":{"owner":"java.math.BigDecimal"}}}\n' \
  | "$JAVA" -jar "$slim" 2>/dev/null \
  | python3 -c '
import json, sys
answer = json.loads(sys.stdin.read().strip())["answer"]["Members"]
print("    rows=%d divide_rows=%d" % (
    len(answer["members"]),
    sum(1 for m in answer["members"] if m["name"] == "divide")))
'
