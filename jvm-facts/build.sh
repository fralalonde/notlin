#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
JAVAC="${JAVAC:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/javac}"
JAR="${JAR:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/jar}"
out=build/classes
rm -rf "$out" build/notlin-jvm-facts.jar
mkdir -p "$out"
find src -name '*.java' | xargs "$JAVAC" -d "$out"
"$JAR" --create --file build/notlin-jvm-facts.jar --main-class notlin.facts.Main -C "$out" .
echo "built build/notlin-jvm-facts.jar"
