#!/usr/bin/env bash
# notlin end-to-end test: transpile all samples, javac them, report failures.
# Usage: tools/e2e.sh [javac-path]  (default JDK 25 temurin — see DEV.md)
set -u
JAVAC="${1:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/javac}"
ANN_JAR="${ANN_JAR:-$(dirname "$0")/../vendor/jetbrains-annotations.jar}"
NOTLIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/notlin"
OUT=/tmp/notlin-e2e

if [ ! -x "$JAVAC" ]; then
    echo "e2e: javac not found at $JAVAC — install JDK or pass path" >&2
    exit 2
fi

rm -rf "$OUT"
mkdir -p "$OUT"

extra_cp=""
[ -f "$ANN_JAR" ] && extra_cp="$ANN_JAR"

overall=0
for kt in "$(dirname "$0")"/../samples/*.kt; do
    name=$(basename "$kt" .kt)
    dir="$OUT/$name"
    mkdir -p "$dir"
    ok=1
    if ! "$NOTLIN" -o "$dir" "$kt" >"$dir/notlin.log" 2>&1; then
        echo "FAIL(notlin) $name: transpiler exited non-zero"
        ok=0
    fi
    if [ "$ok" -eq 1 ]; then
        javac_out=$("$JAVAC" -d "$dir" -cp "$extra_cp" "$dir"/*.java 2>&1 | grep -v '^Picked up JAVA_TOOL_OPTIONS')
        if [ -n "$javac_out" ]; then
            echo "FAIL(javac) $name:"
            echo "$javac_out" | head -8
            ok=0
        fi
    fi
    if [ "$ok" -eq 1 ]; then
        echo "OK $name"
    else
        overall=1
    fi
done
exit $overall
