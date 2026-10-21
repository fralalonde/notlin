#!/usr/bin/env bash
# Compile and run the Track B Analysis API spike.
#
#   ./run.sh                 # fetch deps (if needed), compile, run on fixtures/
#   ./run.sh --no-build      # reuse out/ from the previous compile
#
# Compilation uses kotlinc from the pinned kotlin-compiler jar (scope=build in
# ../deps.lock) — no kotlinc installed on the machine, and we refuse to depend
# on whatever the host happens to have. Runs on JDK 21+ (verified on 25.0.4-tem).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
java_bin="${SPIKE_JAVA:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/java}"
[[ -x "$java_bin" ]] || java_bin="$(command -v java || true)"
[[ -x "$java_bin" ]] || { echo "no java found (set SPIKE_JAVA)" >&2; exit 2; }

"$here/../fetch-deps.sh" >/dev/null

cp="$("$here/../fetch-deps.sh" --classpath-only)"
compiler_jar=$(ls "$here"/libs/kotlin-compiler-*.jar | head -1)
stdlib_jar=$(ls "$here"/libs/kotlin-stdlib-*.jar | head -1)

if [[ "${1:-}" != "--no-build" ]]; then
  rm -rf "$here/out"
  mkdir -p "$here/out"
  "$java_bin" -Xmx1g -cp "$compiler_jar:$cp" org.jetbrains.kotlin.cli.jvm.K2JVMCompiler \
    -classpath "$cp" \
    -d "$here/out" \
    -no-stdlib -no-reflect -nowarn -Xskip-prerelease-check \
    "$here/src/SpikeMain.kt"
fi

exec "$java_bin" ${SPIKE_JVM_OPTS:--Xmx2g} \
  -Dspike.home="$here" \
  -cp "$here/out:$cp" SpikeMainKt "$@"
