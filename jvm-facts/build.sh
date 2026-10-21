#!/usr/bin/env bash
# Build the notlin JVM facts helper.
#
#   ./build.sh           # slim jar, plus the full jar when the pinned deps exist
#   ./build.sh --slim    # slim jar only (JDK only, no third-party deps)
#   ./build.sh --full    # both, failing loudly if the pinned deps are missing
#
# slim -> build/notlin-jvm-facts.jar       Track A reflection facts, JDK only.
# full -> build/notlin-jvm-facts-full.jar  slim + Track B K2 facts behind
#                                          `--kotlin`, launched with
#                                          `java -jar build/notlin-jvm-facts-full.jar --kotlin`.
#
# The slim step compiles exactly the sources it always did (kotlin/ and FullMain
# are the full-jar additions and are excluded), so the slim jar's class set and
# class files are unchanged. The full jar carries no Analysis API classes: its
# manifest Class-Path points at the deps-locked jars in spike/libs/ (relative to
# the jar), which is why it must stay next to spike/ — run ./fetch-deps.sh once.
set -euo pipefail
cd "$(dirname "$0")"

JAVAC="${JAVAC:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/javac}"
JAR="${JAR:-$HOME/.rsdk/tools/java/25.0.4-tem/bin/jar}"
deps=spike/libs

mode=auto
case "${1:-}" in
  ""|--auto) mode=auto ;;
  --slim) mode=slim ;;
  --full) mode=full ;;
  *) echo "usage: $0 [--slim|--full]" >&2; exit 2 ;;
esac

# --- slim: Track A only -----------------------------------------------------
slim_sources=()
while IFS= read -r source; do
  slim_sources+=("$source")
done < <(find src -name '*.java' \
  -not -path 'src/notlin/facts/kotlin/*' \
  -not -path 'src/notlin/facts/FullMain.java' | sort)
if [[ ${#slim_sources[@]} -eq 0 ]]; then
  echo "no slim sources under src/" >&2
  exit 1
fi

rm -rf build/classes build/notlin-jvm-facts.jar
mkdir -p build/classes
"$JAVAC" -d build/classes "${slim_sources[@]}"
"$JAR" --create --file build/notlin-jvm-facts.jar --main-class notlin.facts.Main -C build/classes .
echo "built build/notlin-jvm-facts.jar (slim: Track A reflection facts, ${#slim_sources[@]} sources)"

# --- full: + Track B K2 facts ----------------------------------------------
if [[ "$mode" == slim ]]; then
  exit 0
fi
if ! compgen -G "$deps/kotlin-analysis-api-standalone-surface-*.jar" >/dev/null; then
  if [[ "$mode" == full ]]; then
    echo "missing pinned Analysis API jars in $deps — run ./fetch-deps.sh" >&2
    exit 2
  fi
  echo "skipping full jar: no pinned jars in $deps (run ./fetch-deps.sh to build it)"
  exit 0
fi

classpath=$(./fetch-deps.sh --classpath-only)
if [[ -z "$classpath" ]]; then
  echo "fetch-deps.sh --classpath-only produced an empty classpath" >&2
  exit 2
fi

all_sources=()
while IFS= read -r source; do
  all_sources+=("$source")
done < <(find src -name '*.java' | sort)

rm -rf build/classes-full build/notlin-jvm-facts-full.jar build/MANIFEST-full.mf
mkdir -p build/classes-full
"$JAVAC" -nowarn -cp "$classpath" -d build/classes-full "${all_sources[@]}"

# Manifest Class-Path entries are URLs relative to the jar's own location, so
# the full jar resolves the pinned deps from ../spike/libs without a -cp flag.
# Lines are capped at 72 bytes, with a single leading space on continuations.
runtime_jars=()
while IFS= read -r jar; do
  runtime_jars+=("$jar")
done < <(printf '%s\n' "$classpath" | tr ':' '\n' | sed '/^$/d')

{
  printf 'Manifest-Version: 1.0\n'
  printf 'Main-Class: notlin.facts.FullMain\n'
  line="Class-Path:"
  for jar in "${runtime_jars[@]}"; do
    # scope=runtime only: the build-scope kotlin-compiler jar must never be on
    # the helper's classpath (spike: "build-only, must never be bundled").
    entry=" ../spike/libs/$(basename "$jar")"
    if (( ${#line} + ${#entry} > 70 )); then
      printf '%s\n' "$line"
      line=" $entry"
    else
      line="$line$entry"
    fi
  done
  printf '%s\n\n' "$line"
} > build/MANIFEST-full.mf

"$JAR" --create --file build/notlin-jvm-facts-full.jar --manifest build/MANIFEST-full.mf \
  -C build/classes-full .
echo "built build/notlin-jvm-facts-full.jar (full: + --kotlin, ${#all_sources[@]} sources,"
echo "      ${#runtime_jars[@]} deps-locked runtime jars on the manifest Class-Path)"
