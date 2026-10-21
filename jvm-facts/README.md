# jvm-facts — stdio facts helper

A small plain-Java program (JDK only, no Gradle, no third-party dependencies) that
answers questions about the JVM classes a translation run can see, one JSON line
in, one JSON line out. notlin's Rust side spawns it when `NOTLIN_JVM_FACTS_JAR`
points at a built jar; without it the transpiler behaves exactly as before.

This is Track A of the fact-sourcing plan: reflection over the Java classpath.
Kotlin facts (Track B) are a later phase and are not implemented here.

## Build

```bash
bash jvm-facts/build.sh   # -> build/notlin-jvm-facts.jar
```

The script uses `~/.rsdk/tools/java/25.0.4-tem/bin/{javac,jar}` by default;
override with `JAVAC=...` / `JAR=...`. It compiles `src/**/*.java` into
`build/classes` and packs a single runnable jar (`build/` is git-ignored).

## Pointing notlin at it

```bash
NOTLIN_JVM_FACTS_JAR=jvm-facts/build/notlin-jvm-facts.jar \
NOTLIN_CLASSPATH=/path/to/app.jar:/path/to/other.jar \
  notlin -o out samples/*.kt
```

- `NOTLIN_JVM_FACTS_JAR` — path to the built jar; unset means "helper absent".
- `NOTLIN_CLASSPATH` — colon-separated jars/directories, read **by the helper**
  at startup. Platform classes (`java.*`, `javax.*`) resolve without it.
  Classes are loaded with `Class.forName(name, false, loader)`, so querying a
  type never runs its static initializers.

## Wire format

Newline-delimited JSON, one request per line, at most one answer line per
request. Field names are fixed by `src/facts/protocol.rs` on the Rust side and
by this helper; both ends are ours, so the helper parses with a rigid string
scanner instead of a JSON library and serializes with a `StringBuilder`.

Requests:

```json
{"version":1,"id":N,"query":{"Ping":null}}
{"version":1,"id":N,"query":{"ClassMembers":{"owner":"binary.Name"}}}
```

Answers:

```json
{"version":1,"id":N,"answer":{"Pong":{"version":1}}}
{"version":1,"id":N,"answer":{"Members":{"owner":"<echo>","members":[...] or null}}}
```

Each member object:

```json
{"name":"add","static_member":false,"parameter_types":["java.math.BigDecimal"],
 "result_type":"java.math.BigDecimal","parameter_nullability":["unknown"],
 "result_nullability":"unknown"}
```

- `name` is `<init>` for constructors.
- `result_type` is `<void>` for void.
- Primitives use the JVM names `Class.getName()` already produces (`int`, `long`,
  `char`, …); references are binary names (`java.math.BigDecimal`), arrays are
  `[C`, `[Ljava.lang.String;`.
- Members are public only and sorted: constructors first, then by name, then by
  parameter list. Declared plus inherited (superclass walk, subclass overrides
  win); `synthetic`/`bridge` members are filtered out, matching what a caller can
  actually reach.

### Fail-closed rules

- `members: null` (not `[]`) when the type cannot be established: invalid owner
  (outside `[A-Za-z0-9_$.]`), `ClassNotFoundException`, linkage error, non-public
  type, or a member enumeration that fails halfway. Null always means "unknown".
- Missing/garbled lines, wrong `version`, unknown `query` discriminator: the
  helper answers **nothing** for that line, logs to stderr, and keeps serving;
  EOF exits 0. It never crashes the loop. The Rust client treats a missing answer
  as `Unknown`.
- `parameter_nullability` / `result_nullability` are `non_null` or `nullable`
  only when a **runtime-visible** `org.jetbrains.annotations.NotNull` /
  `org.jetbrains.annotations.Nullable` annotation is present; otherwise
  `unknown`. Nothing is inferred from JDK convention, `Optional`, or Kotlin
  metadata.

### Known limitation: annotation retention

`vendor/jetbrains-annotations.jar` declares `@NotNull`/`@Nullable` with
`RetentionPolicy.CLASS`, so plain reflection cannot see them on those
declarations and the helper reports `unknown` there. The check is written
against annotation names, not compiled-in types, so any runtime-retained
`NotNull`/`Nullable` on the classpath is honored — verified with a scratch
annotation of the same FQN at `RetentionPolicy.RUNTIME`. Reading CLASS-retained
annotations would require parsing class files; that is deliberately out of scope
here (fail closed, never guess).
