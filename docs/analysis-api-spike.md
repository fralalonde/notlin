# Analysis API spike (plan task 3.1) — verdict: **PROCEED**

Date: 2026-10-21 · Branch `exp/fact-sourcing` · Spike sources: `jvm-facts/spike/`
Deps: `jvm-facts/deps.lock` + `jvm-facts/fetch-deps.sh` · Measured on JDK 25.0.4-tem, 16 cores, 38 GiB.

## Verdict

**PROCEED.** A headless K2 `StandaloneAnalysisAPISession` bootstrapped on the first
attempt using only Maven Central artifacts, and answered all three fact families the
Rust side needs for a call site — resolved callee FQN, parameter/result types with
nullability, and erased JVM descriptors — for both a user-declared class and a
generic stdlib function. Startup is ~1 s, steady-state query cost is 11–63 ms, and
the whole thing runs in-process on the JDK already on the CI image.

The kill criterion (task 3.1: "cannot answer `expressionType` for `a.add(b)` in two
working days") was **not** triggered; the working bootstrap was reached in roughly
70 minutes of wall time, most of it spent on Kotlin-API spelling, not on the
dependency graph.

Two things are *not* blockers but must be planned for, and neither is a surprise:

1. Only a **pre-release** (`2.5.0-Beta2`) of the modular Analysis API is published to
   Maven Central. Stable Kotlin is 2.4.21 and ships **no** published standalone
   Analysis API. See "Version and stability risk".
2. The API is Kotlin-first: several functions that matter are `context(session)`
   extensions that must be called as member extensions (`type.render(...)`), and
   `KaType.nullability` is not reachable from Kotlin source in this version (use
   `isMarkedNullable` / `isNullable` / `hasFlexibleNullability` instead). This is a
   day-one learning cost for task 3.2, not a risk to the approach.

## What was run

```
jvm-facts/fetch-deps.sh                  # 39 pinned jars, sha256-verified
jvm-facts/spike/run.sh                   # compiles SpikeMain.kt, runs on fixtures/
jvm-facts/spike/run.sh <srcRoot> <stdlib.jar> '*'   # any source tree, all call sites
```

`run.sh` compiles with `kotlinc` taken from the pinned `kotlin-compiler` jar
(`java -cp kotlin-compiler.jar org.jetbrains.kotlin.cli.jvm.K2JVMCompiler`) — no
Gradle, no host `kotlinc`, no Maven at run time. `deps.lock` carries both scopes:
`runtime` (38 jars) and `build` (the compiler jar).

### Transcript — fixtures (`jvm-facts/spike/fixtures/src/main`, 2 files, all call sites)

```
spike    : java=25.0.4 vm=OpenJDK 64-Bit Server VM
phase    : session-build 1043 ms  used=26MiB max=2048MiB
phase    : modules spike-main(2 files)
phase    : files-loaded 1044 ms  [Counter.kt, Site.kt]
--- call-site Counter.kt:9:24 offset=287 text='mutableListOf<Int>()'
    expressionType   = kotlin.collections.MutableList<kotlin.Int>
    expressionDescr  = Ljava/util/List;
    calleeFqn        = kotlin.collections.mutableListOf
    calleeJvmOwner   = <no classId>
    extensionRecv    = <none>
    dispatchRecv     = <none>
    parameterTypes   = []
    returnType       = kotlin.collections.MutableList<kotlin.Int> (markedNullable=false, isNullable=false, flexible=false)
    methodDescr      = ()Ljava/util/List;
    candidates       = 2
    queryMs          = 1071
--- call-site Counter.kt:12:14 offset=356 text='add(value)'
    expressionType   = kotlin.Boolean
    expressionDescr  = Z
    calleeFqn        = kotlin.collections.MutableList.add
    calleeJvmOwner   = kotlin.collections.MutableList
    extensionRecv    = <none>
    dispatchRecv     = kotlin.collections.MutableList<kotlin.Int>
    parameterTypes   = [element: kotlin.Int]
    returnType       = kotlin.Boolean (markedNullable=false, isNullable=false, flexible=false)
    methodDescr      = (I)Z
    candidates       = 2
    queryMs          = 19
--- call-site Site.kt:10:36 offset=406 text='add(value)'
    expressionType   = kotlin.String
    expressionDescr  = Ljava/lang/String;
    calleeFqn        = fixture.Counter.add
    calleeJvmOwner   = fixture.Counter
    extensionRecv    = <none>
    dispatchRecv     = fixture.Counter
    parameterTypes   = [value: kotlin.Int]
    returnType       = kotlin.String (markedNullable=false, isNullable=false, flexible=false)
    methodDescr      = (I)Ljava/lang/String;
    candidates       = 1
    queryMs          = 19
--- call-site Site.kt:15:10 offset=496 text='add(value)'
    expressionType   = kotlin.Boolean
    expressionDescr  = Z
    calleeFqn        = kotlin.collections.MutableList.add
    calleeJvmOwner   = kotlin.collections.MutableList
    extensionRecv    = <none>
    dispatchRecv     = kotlin.collections.MutableList<kotlin.Int>
    parameterTypes   = [element: kotlin.Int]
    returnType       = kotlin.Boolean (markedNullable=false, isNullable=false, flexible=false)
    methodDescr      = (I)Z
    candidates       = 2
    queryMs          = 11
--- call-site Site.kt:19:32 offset=582 text='find("k")'
    expressionType   = kotlin.String?
    expressionDescr  = Ljava/lang/String;
    calleeFqn        = fixture.Counter.find
    calleeJvmOwner   = fixture.Counter
    extensionRecv    = <none>
    dispatchRecv     = fixture.Counter
    parameterTypes   = [key: kotlin.String]
    returnType       = kotlin.String? (markedNullable=true, isNullable=true, flexible=false)
    methodDescr      = (Ljava/lang/String;)Ljava/lang/String;
    candidates       = 1
    queryMs          = 54
phase    : queries-done 2523 ms  count=6  used=69MiB max=2048MiB
phase    : disposed 2528 ms
RESULT   : OK (6 call sites resolved)
```

### Transcript — the repo's own `samples/` (4 files, 16 call sites, every one resolved)

```
phase    : session-build 1084 ms  used=26MiB max=2048MiB
phase    : modules spike-main(4 files)
phase    : files-loaded 1084 ms  [BasicClass.kt, Expressions.kt, Nullspotting.kt, Untranslatables.kt]
phase    : queries-done 2825 ms  count=16  used=85MiB max=2048MiB
RESULT   : OK (16 call sites resolved)
```

Six of those call sites are exactly the S001 names the bench baseline measured as
`UnknownRequiredFact` — `add`, `greet`, `haveBirthday`, `register`, `display`, `map`:

```
BasicClass.kt:24:15   add(item)       -> kotlin.collections.MutableList.add  (Ljava/lang/String;)Z
BasicClass.kt:30:15   greet()         -> com.example.demo.Person.greet        ()Ljava/lang/String;
BasicClass.kt:31:7    haveBirthday()  -> com.example.demo.Person.haveBirthday ()Lkotlin/Unit;
BasicClass.kt:34:14   register("x")   -> com.example.demo.Registry.register   (Ljava/lang/String;)Lkotlin/Unit;
Expressions.kt:37:15  display()       -> com.example.expr.Account.display     ()Ljava/lang/String;
Untranslatables.kt:55 map { it * 2 }  -> kotlin.collections.map                (Lkotlin/jvm/functions/Function1;)Ljava/util/List;
```

(Four more names resolve too, e.g. `kotlin.io.println`, `kotlin.collections.listOf`.)

## Cost, measured

| What | Value |
|---|---|
| Session build (2 modules: stdlib library + source) | **1.04–1.08 s** |
| First query (includes FIR frontend init) | **1.0–1.3 s** |
| Subsequent queries | **11–63 ms** (typ. 15 ms) |
| 4-file / 16-call-site module, end to end | **2.83 s** |
| Heap actually used | 26 MiB after build → 85 MiB after 16 queries |
| Peak RSS of the JVM (`-Xmx2g`) | **~537 MiB** |
| Download | **39 jars, 148.2 MiB** (38 runtime + 1 build-tool jar) |

Interpretation for the helper: a single JVM per notlin run pays ~1 s bootstrap +
~1.3 s first-query warm-up, then ~15 ms per call site. On the 23-file bench slice
with ~126 declarations this is well under a second of analysis on top of bootstrap —
i.e. the cost is dominated by startup, which is exactly what a long-lived stdio
helper amortizes. Two orders of magnitude cheaper than a per-query process.

## Dependency set (exact, pinned)

`jvm-facts/deps.lock` — 39 rows of `scope <TAB> coordinate <TAB> sha256 <TAB> bytes <TAB> central path`.

Root coordinate: `org.jetbrains.kotlin:kotlin-analysis-api-standalone-implementation:2.5.0-Beta2`.
Resolved graph (all at 2.5.0-Beta2 unless noted): standalone surface + implementation,
`kotlin-analysis-api-surface`, `-implementation`, `-platform-interface`,
`-intellij-api-surface-components`, `-intellij-implementation-components`,
`-fir-diagnostics`; plus `kotlin-stdlib`/`kotlin-reflect` 2.3.21, `annotations` 26.0.2,
guava 33.3.1 + failureaccess + listenablefuture + j2objc-annotations,
caffeine 2.9.3 + checker-qual + error_prone_annotations, opentelemetry-api/context 1.41.0,
kotlinx-serialization-core/json 1.9.0, kotlinx-collections-immutable-jvm 0.5.1,
aalto-xml 1.3.0, stax2-api 4.2.1, antlr4-runtime 4.13.2, lz4-java 1.7.1, vavr 0.10.4
(+vavr-match), streamex 0.7.2, jansi 2.4.0, javax.inject 1, jsr305 1.3.9, oro 2.0.8.

Two pins are deliberate additions outside Maven's resolved tree:

- `kotlin-compiler:2.5.0-Beta2` — **build** scope only, provides the `kotlinc` we
  borrow to compile the spike. The compiler *frontend* the helper needs at run time is
  already inside `kotlin-analysis-api-implementation`, so there is **no** runtime
  compiler dependency and no compiler/analysis-api version skew to manage.
- `kotlinx-coroutines-core-jvm:1.8.0` — **runtime**; the IntelliJ core and the borrowed
  `kotlinc` both fail with `NoClassDefFoundError: kotlinx/coroutines/SupervisorKt`
  without it. Maven's tree does not list it (the artifacts do not declare it).

Provenance: every jar was resolved with Maven (`dependency:list`), then each local
file was hashed (`sha256` in the lock) and cross-checked against the sha1 that Maven
Central publishes next to the artifact — **37/37 matched, 0 mismatches**; the two
hand-added pins (compiler, coroutines) were sha1-checked the same way and matched. Jars are
fetched into `jvm-facts/spike/libs/`, which is gitignored; nothing binary is committed.
`fetch-deps.sh` re-verifies on every run and re-downloads on checksum drift.

## The three fact families → the exact API calls (all verified here)

Inside `analyze(callExpression) { … }` the receiver is a `KaSession`:

| Fact family | Call | Notes |
|---|---|---|
| Symbol/expression type | `call.expressionType` (`KaExpressionTypeProvider`) | returns `KaType`; render with `type.render(position = Variance.INVARIANT)` |
| Type nullability | `type.isMarkedNullable`, `type.isNullable`, `type.hasFlexibleNullability` (`KaTypeInformationProvider`) | `KaType.nullability` exists in the JVM signature but does **not** resolve from Kotlin source here; use the three predicates |
| Call resolution | `call.resolveToCall()` (`KaResolver`) → `KaCallInfo`; `info.successfulFunctionCallOrNull()` | `resolveToCallCandidates()` gives the full candidate list for the ambiguity case |
| Callee identity | `resolved.signature.callableId?.asSingleFqName()`; owner via `callableId?.classId?.asFqNameString()` | `callableId` is **null for constructors** (`<local>` in the transcript) |
| Dispatch/extension receiver | `resolved.dispatchReceiver?.type`, `signature.receiverType` | **dispatch** receiver is the notlin-relevant one for member calls; `signature.receiverType` is the *extension* receiver (usually `<none>`) |
| Parameter/result types | `signature.valueParameters.map { it.returnType }`, `signature.returnType` (`KaFunctionSignature`) | already substituted by the call's type arguments — `MutableList<Int>.add` gave `dispatchRecv = MutableList<kotlin.Int>` |
| JVM descriptor | `type.mapToJvmTypeDescriptor()` (`KaJavaInteroperabilityComponent`), param+return concatenated | returns erased descriptors: `String?` → `Ljava/lang/String;`, `MutableList<Int>` → `Ljava/util/List;` |

Two consequences for the protocol design (worth writing down before 3.2):

1. **A descriptor does not carry nullability.** `String?` and `String` both erase to
   `Ljava/lang/String;`. The protocol already has separate `parameter_nullability` /
   `result_nullability` fields — they must be filled from the `isMarkedNullable`
   family, never inferred from the descriptor.
2. **Declared vs. substituted matters.** The spike reports the *substituted* signature
   at the call site (`MutableList<Int>.add`, receiver `MutableList<Int>`), which is what
   a translation decision needs. `KaCallableSignature.substitute()` is available if a
   declared view is ever wanted.

## Version and stability risk (read before scheduling 3.2)

- **Only a beta is published.** `org.jetbrains.kotlin:kotlin-analysis-api*` exists on
  Maven Central at exactly one version, `2.5.0-Beta2` (metadata `lastUpdated`
  2026-10-21). Stable `kotlin-compiler` is at 2.4.21; there is **no** published
  standalone Analysis API for any stable Kotlin release. There is no
  `org.jetbrains.kotlin:analysis-api-standalone` artifact on Maven Central at all, and
  the JetBrains Space repositories that older documentation points at are gone
  (`maven.pkg.jetbrains.space/kotlin/p/kotlin/dev/...` now 301-redirects to
  `packages.jetbrains.team/maven/p/kt/dev/...` and returns 404 for the artifact).
  Track B therefore rides a pre-release compiler frontend, permanently pinned in
  `deps.lock`, until JetBrains publishes a stable one.
- **JetBrains' own stability map**, `analysis/docs/contribution-guide/api-evolution.md`
  (fetched 2026-10-21), says verbatim:

  > **Stable API** — Strong source and binary backward compatibility — Changes follow
  > strict deprecation cycles — Applies to: *Analysis API Surface* (excluding
  > declarations with opt-ins)
  >
  > **Unstable API** — May evolve without deprecation warnings — Changes *may* follow
  > deprecation cycles, depending on the API usage — Applies to: *Platform Interface*,
  > **Standalone API**, Declarations annotated with `@KaExperimentalApi`,
  > `@KaNonPublicApi`, `@KaIdeApi`, *Kotlin PSI* is formally not yet stable despite its
  > practical stability

  So: the query surface we use is Stable, **the Standalone bootstrap we need is
  Unstable by JetBrains' own classification**. Pin the version, treat the bootstrap
  code as replace-on-upgrade, keep it in one file.
- **API churn is real but cheap.** This spike compiled against 2.5.0-Beta2 only; the
  differences from the 2.1/2.2-era snippets still found online are exactly the ones
  noted in the source header of `SpikeMain.kt`
  (`resolveToCall` is a `KaResolver` member, `KaCall.signature` replaces
  `partiallyAppliedSymbol.signature`, no `StandaloneProjectEnvironment` type any more).
  Budget half a day for the equivalent shakedown in 3.2 — not a week.

## Sharp edges found (all worked around in `SpikeMain.kt`)

1. **Module registration is explicit.** `buildKtSourceModule { … }` does not register
   itself; without `addModule(...)` the session builds happily and reports
   `modulesWithFiles` without your source module (`session exposes no KaSourceModule`).
2. **Platform must be set per module.** Setting `platform` only on the container
   builder yields `UninitializedPropertyAccessException: lateinit property platform has
   not been initialized` inside `KtLibraryModuleBuilder.build()`.
3. **The session leaks non-daemon threads.** Returning from `main` after
   `Disposer.dispose(disposable)` leaves the JVM alive indefinitely (observed: the
   process was still running minutes later; the first spike run hung on this). A helper
   must terminate explicitly — fine for a long-lived stdio helper, but its shutdown path
   must be deliberate, and spike/test programs need `exitProcess`.
4. **Member-extension call syntax.** `render(type)` fails; `type.render(...)` works, and
   `position` must be passed explicitly (`Variance.INVARIANT`).
5. **Constructors report `callableId == null`.** Use
   `successfulConstructorCallOrNull()` (or the symbol's `classId`) for `Person(...)`
   call sites; don't treat `<local>` as unresolved.
6. **`-Xskip-prerelease-check`** is needed to compile Kotlin against the pre-release
   metadata; `-no-stdlib -no-reflect` keeps the borrowed `kotlinc` from hunting for a
   Kotlin home.

## If proceeding — concrete next steps

**3.2a (half day) — decide the helper's language.** The current helper
(`jvm-facts/src/notlin/facts/`, Java) can plausibly call this API from Java: the hot
entry points are interface methods (`call.resolveToCall()`,
`session.mapToJvmTypeDescriptor(type)`) plus one static
`AnalyzeKt.analyze(KtElement, Function1)`. **This was not executed** — the spike is
Kotlin. Probe it first: 30 lines of Java resolving one call site. If Java is awkward,
add one Kotlin source file to `jvm-facts/` compiled with the pinned compiler jar
(no Gradle; `build.sh` gains a `kotlinc` invocation) and keep the JSON protocol in Java.

**3.2b (1–1.5 days) — `KotlinFacts` handler.** Three queries per protocol:
`KotlinSymbolType { file, byte_offset }` → `expressionType` + nullability;
`ResolveCall { file, byte_offset }` → `callableId` / owner `classId` / dispatch
receiver / param+result types; `JvmSignature` → `mapToJvmTypeDescriptor` pair. Map
`successfulFunctionCallOrNull() == null` **or** error types to `ambiguous`, never to a
guess; `resolveToCallCandidates().size > 1` is `ambiguous` when the resolved one is not
uniquely applicable. Session lifecycle: one session per `--classpath` set, built lazily
on the first Kotlin query, source roots handed in by the Rust side.

**3.2c (1 day) — golden fixtures.** Check the JSON against `samples/*.kt` call sites,
the same ones pasted above (`add`, `greet`, `haveBirthday`, `register`, `display`,
`map`), plus one deliberately unresolvable file to prove the fail-closed path.

**3.3 (1–2 days) — decorator upgrade.** Only after 3.2's golden JSON is stable.

Estimated total for Track B as re-scoped: **3–4.5 days**, with the ±1 day concentrated
in the Java-interop question and in mapping `KaType` ambiguity to `FactStatus`.

## License / provenance note for the dep set

All 39 artifacts are permissive: Apache-2.0 (Kotlin/stdlib/reflect/components, Guava,
Caffeine, OpenTelemetry, kotlinx-*, Vavr, StreamEx, aalto-xml, lz4-java, jansi,
javax.inject, jsr305, JetBrains annotations, error-prone annotations), MIT
(checker-qual), BSD (stax2-api), BSD-3 (antlr4-runtime).

One item needs a human decision if the helper is ever **shipped** (not merely built):
`oro:oro:2.0.8` has no machine-readable license block in its POM and Apache ORO is
**Apache-1.1**, which carries an acknowledgement/NOTICE obligation rather than a
copyleft one. It arrives transitively via the IntelliJ implementation components
(ANTLR path). Nothing here is copyleft; nothing forces us to redistribute source. If
notlin ships a helper jar, generate a NOTICE that enumerates these 39 coordinates (the
lock file is already the manifest) — and if Apache-1.1 is unacceptable, look for a
newer Analysis API drop that has dropped ORO.

`kotlin-compiler` is **build-only** and must never be bundled into the shipped helper —
it is 60.8 MiB of compiler and would triple the artifact.

## What would still sink Track B (watch list)

- JetBrains withdrawing or renaming the modular artifacts before a stable release
  (they are new — published today — and only beta). Mitigation: the lock file pins the
  exact 39 jars with checksums; a vanished version is a re-pin, not a re-spike, as long
  as a successor version exposes `buildStandaloneAnalysisAPISession` + `KaResolver`.
- A notlin workload whose Kotlin sources need a real classpath (Jackson jars etc.).
  Untested here: the spike resolves against stdlib only. Building a library module from
  a dozen jars is the same `buildKtLibraryModule { addBinaryRoot(...) }` call in a loop,
  but whether K2 standalone indexes a big classpath within the helper's startup budget
  is the one number still missing — measure it first in 3.2, before the protocol work.
- Memory on small CI machines: ~537 MiB RSS peak with a 2 GiB heap cap. Set `-Xmx` to
  ~768 MiB for the helper and re-measure; do not let the helper inherit a blanket
  `-Xmx2g`.
