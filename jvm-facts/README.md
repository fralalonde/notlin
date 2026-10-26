# jvm-facts — stdio facts helper

A small program that answers questions about the JVM classes and Kotlin call
sites a translation run can see, one JSON line in, one JSON line out. notlin's
Rust side spawns it when `NOTLIN_JVM_FACTS_JAR` points at a built jar; without it
the transpiler behaves exactly as before.

Two jars, one code base:

| jar | contents | needs |
|---|---|---|
| `build/notlin-jvm-facts.jar` (**slim**) | Track A: reflection + class-file facts, JDK only | nothing |
| `build/notlin-jvm-facts-full.jar` (**full**) | slim **+** Track B: K2 `ResolveCalls` behind `--kotlin` | the 38 deps-locked jars in `spike/libs/` |

The slim jar is the one Track A has always produced: same 6 class files, byte for
byte. The full jar is additive — same main loop, plus one new query
discriminator, off unless `--kotlin` is passed.

## Build

```bash
bash jvm-facts/build.sh            # slim, plus full when spike/libs has the deps
bash jvm-facts/build.sh --slim     # slim only (JDK only, no third-party deps)
bash jvm-facts/build.sh --full     # both; fails if ./fetch-deps.sh was never run
jvm-facts/fetch-deps.sh            # 39 pinned jars, sha256-verified (148 MiB)
```

The script uses `~/.rsdk/tools/java/25.0.4-tem/bin/{javac,jar}` by default;
override with `JAVAC=...` / `JAR=...`. `build/` is git-ignored.

The full jar carries **no** Analysis API code: its manifest `Class-Path` points at
`../spike/libs/*.jar` (relative to the jar), which is why it has to stay next to
`spike/`. The build-scope `kotlin-compiler` jar is deliberately *not* on that
list — it is a compiler, not a runtime dependency.

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

Newline-delimited JSON, one request per line, one answer line per **well-formed**
request. Field names are fixed by `src/facts/protocol.rs` on the Rust side and
by this helper; both ends are ours, so the helper parses with a rigid string
scanner instead of a JSON library and serializes with a `StringBuilder`.

The client waits for exactly one line per request, so a request this helper
cannot satisfy is answered with an explicit `NoEvidence` line rather than
silence. Silence is indistinguishable from a stalled helper, and a stalled
helper costs the run its deadline (the client kills and reaps it). A
`NoEvidence` answer costs only that request: the helper stays trusted for
`Ping`/`ClassMembers`/the next file, and the same question may be retried.

A single answer line is capped at 64 MiB including its newline, and the client
queues at most one line ahead of the request it is waiting for. A longer line,
a line that is not valid UTF-8, or a request left unanswered past the client's
120 s response deadline is a channel fault: the client kills and reaps the
helper rather than buffering or salvaging the frame. That deadline cannot tell
a slow helper from a stalled one, so this helper must answer — with a batch or
`NoEvidence` — rather than stay silent.

Requests:

```json
{"version":1,"id":N,"query":{"Ping":null}}
{"version":1,"id":N,"query":{"ClassMembers":{"owner":"binary.Name"}}}
{"version":1,"id":N,"query":{"ResolveCalls":{"file":"/abs/path.kt"}}}   // full jar with --kotlin only
```

Answers:

```json
{"version":1,"id":N,"answer":{"Pong":{"version":1}}}
{"version":1,"id":N,"answer":{"Members":{"owner":"<echo>","members":[...] or null}}}
{"version":1,"id":N,"answer":{"CallSites":{"file":"<echo>","sites":[...]}}}
{"version":1,"id":N,"answer":{"NoEvidence":{"query":"ResolveCalls","reason":"<diagnostic>"}}}
```

`NoEvidence` is the per-request "asked, healthy, nothing for this one" answer:
emitted for a `ResolveCalls` the full jar did not get `--kotlin` for, for a file
the session did not index, for a file it cannot read, for a batch that failed
midway (never a partial batch), and for any query discriminator this build does
not implement. It is **not** an empty `CallSites` batch: `sites: []` means
"looked, saw no calls". `reason` is diagnostic text and never carries paths; the
Rust side never parses it.

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
- Missing/garbled lines and a wrong `version`: the helper answers **nothing** for
  that line (the id it carries is not trustworthy either), logs to stderr, and
  keeps serving; EOF exits 0. It never crashes the loop. A well-formed query
  with an unknown discriminator *is* answered, with `NoEvidence`: the id is
  valid and the client is waiting for it.
- `parameter_nullability` / `result_nullability` are `non_null` or `nullable`
  only when proven: class-file `RuntimeInvisibleAnnotations` (JDK 22+
  `java.lang.classfile`, which is how the CLASS-retained
  `org.jetbrains.annotations` annotations become visible) first, runtime-visible
  annotations second; otherwise `unknown`. Nothing is inferred from JDK
  convention, `Optional`, or Kotlin metadata.

## Kotlin facts (`--kotlin`, Track B)

```bash
java --sun-misc-unsafe-memory-access=allow \
     -jar jvm-facts/build/notlin-jvm-facts-full.jar --kotlin
```

The optional JVM flag only silences the `sun.misc.Unsafe` deprecation lines that
the IntelliJ core prints on stderr under JDK 24+; it changes no behavior.

`--kotlin` builds **one** K2 `StandaloneAnalysisAPISession` at startup
(~1.1 s for 2 files, ~2.1 s for the 23-file bench module), then answers one
`ResolveCalls` batch per file over the same session. The first batch pays FIR
front-end initialization (~1.4 s on the fixture, ~2.6 s on a 56-call bench file),
later batches are tens of milliseconds — measured on the bench slice:
2555 ms, then 645 ms, then **22 ms** for a repeated file.

### Environment contract

| variable | required | meaning |
|---|---|---|
| `NOTLIN_KOTLIN_SOURCES` | **yes** with `--kotlin` | colon-separated **directories** or **`.kt` files**. A directory becomes a source root (recursively indexed). A file adds its **parent directory** as the root, because K2 indexes by root and cross-file resolution (`Counter.add` called from `Site.kt`) needs the siblings in the same module. Missing/empty ⇒ the helper logs why and exits 2. |
| `NOTLIN_CLASSPATH` | no | same jars/class-dirs Track A uses; they become one extra library module, a dependency of the source module. Best effort: a type that cannot be resolved becomes an error type and the sites that touch it are `Ambiguous`, never guessed. |
| `NOTLIN_KOTLIN_STDLIB` | no | explicit stdlib jar. Default: the code source of the already-loaded `kotlin.Unit` class (the full jar's manifest `Class-Path` puts the deps-locked stdlib there), then `java.class.path`, then a `libs/` directory near the jar. |

The JDK is added to the session as an sdk module
(`addBinaryRootsFromJdkHome(java.home)`). This is not optional: without it the
session cannot see `java.lang` *at all*, and a 56-call bench file produced 168
errors with every Java-touching site degraded — the difference between 33/56 and
56/56 `Established` on that file.

### CallSites wire shape

```json
{"byte_offset":406,"text":"add(value)","status":"Established",
 "callee_owner":"fixture.Counter","callee_name":"add","parameter_types":["kotlin.Int"],
 "result_type":"kotlin.String","result_nullable":false,"jvm_descriptor":"(I)Ljava/lang/String;"}
```

- `byte_offset` — UTF-8 byte offset of the call expression **in the file on
  disk**. PSI offsets are char offsets in a line-ending/BOM-normalized copy, so
  they are mapped, not copied; the two coincide for plain ASCII/LF sources.
- `callee_owner` — the K2 `ClassId` (`fixture.Counter`,
  `kotlin.collections.MutableList`), not a JVM binary name. For a top-level
  function, where there is no class id, it is the **JVM facade class**
  (`kotlin.StandardKt` for `let`, `kotlin.io.ConsoleKt` for `println`,
  `…KotlinModuleKt` for `isKotlinClass`) via
  `getContainingJvmClassName`. Constructors report the constructed class.
- `callee_name` — Kotlin simple name; `<init>` for constructors.
- `parameter_types`, `result_type` — K2 renderings, substituted at the call site,
  normalized to plain Kotlin names: `kotlin.Int`, `kotlin.String?`,
  `kotlin.collections.MutableList<kotlin.Int>`, `(kotlin.String) -> kotlin.Boolean`
  for function types. Flexible (Java platform) types render as their non-null
  lower bound and K2's `!` markers are dropped, because `A..B` / `String!` are not
  Kotlin type names.
- `result_nullable` — true when the result is marked nullable **or** has flexible
  (platform) nullability. Descriptors are erased, so this is the only nullability
  signal on the wire; there is no per-parameter nullability field, so a dropped
  `!` on a parameter is information the wire never had a slot for.
- `jvm_descriptor` — erased descriptor of the *substituted* call-site view:
  `(I)Ljava/lang/String;`, `(I)Z` for `MutableList<Int>.add`, `(…)V` for
  constructors. For an extension call this is the value-parameter list and the
  result (the spike's convention); the extension receiver is not prepended.

One batch walks **every** call expression in the file (nested ones included,
outer first), so a file is one request, not one request per site.

### Status rules (fail closed)

| status | when | fields |
|---|---|---|
| `Established` | resolved to one callable, and the expression, every parameter and the result type are real K2 types | all filled |
| `Ambiguous` | resolution succeeded **but** an error type was involved (unresolved external, incomplete classpath, failed inference), **or** resolution failed while K2 saw candidates | `callee_owner`/`callee_name`/`result_type`/`jvm_descriptor` are `null`, `parameter_types` is `[]`, `result_nullable` is `false` |
| `Unknown` | resolution failed with no candidates at all (unresolved name, non-call construct) | same as `Ambiguous` |

`Established` is the only status whose fact fields mean anything; a consumer must
gate on it. Nothing is ever inferred from a partial resolution.

### Session lifecycle

`stdin` EOF ⇒ the session is disposed on a bounded (1.5 s) daemon thread and the
JVM is then halted. This is deliberate: the standalone K2 session leaves
non-daemon threads behind, so a plain `return` from `main` keeps the process
alive forever (observed: minutes). Measured EOF→exit: 0.03 s with `--kotlin`,
0.01 s without, 0.00 s on the slim jar.

## Probes

```bash
jvm-facts/kotlin-probe.sh            # [1] build [2] Ping [3] fixture [4] bench [5] EOF [6] BigDecimal
```

`kotlin-probe.sh` is the evidence script: both jars build, `Ping` answers on
both, `ResolveCalls` is refused without `--kotlin`, the spike fixture resolves
to the exact values recorded in `docs/analysis-api-spike.md`, the bench slice
resolves `let`/`takeIf`/`add`/`get`/constructors with Kotlin names and
descriptors, EOF exits under 2 s in all three modes, and the slim jar still
reports 6 `divide` rows for `java.math.BigDecimal` (80 members).

## Known limitations

- The Analysis API is a **pre-release** (`2.5.0-Beta2`, the only published
  version) and its standalone bootstrap is unstable by JetBrains' own
  classification. Everything that touches it lives in
  `src/notlin/facts/kotlin/` so an upgrade is a local change; the query surface
  (`KaResolver`, `KaFunctionSignature`, `mapToJvmTypeDescriptor`, …) is stable.
- The session's classpath is best effort by design: notlin's `NOTLIN_CLASSPATH`
  is what the caller passed, so a source tree that needs more jars than that
  gets `Ambiguous` on the affected sites (reported honestly) rather than a guess.
- Java interop needed no Kotlin bridge: the API's `context`/member extensions are
  plain interface methods after lowering (`session.isMarkedNullable(type)`,
  `RenderingKt.render(session, type, Variance.INVARIANT)`,
  `KaCallInfoKt.successfulFunctionCallOrNull(info)`), so the driver is Java like
  the rest of the helper and `build.sh` needs no `kotlinc`.
