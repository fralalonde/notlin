# Architecture and recent-history review

Reviewed the 25 commits ending at `feac6e6`, including the newly pulled enum
group integration. The current architecture has four useful boundaries:

1. `workspace` indexes declarations and receiver/property relationships.
   `semantics` adds operation facts, with an optional run-scoped JVM helper.
2. `translation_plan` records ownership and required operations. The workspace
   migration pipeline iterates virtual source/caller repairs to convergence.
3. `transpiler::unit` prepares Java; `planning` parses and accepts it against
   the selected owners and source snapshots.
4. `migrate` applies the converged edits. Coverage counts original declarations,
   including declarations removed during earlier speculative rounds.

The useful simplification is fewer competing implementations of these decisions.
Removing the forwarding provider eliminates a second implementation of every
semantic accessor. Five independent syntax-lowering walks now share one traversal;
eager collections keep their separate inner-first traversal. Companion forwarding
signatures come from the emitted Java methods instead of a second Kotlin signature
renderer. Snapshot checks, declaration identity, caller repair, and convergence
remain necessary: they protect executable application behavior as well as ABI.

The largest remaining complexity is the overlap between source-property repair,
default-getter analysis, and compatibility planning. Replacing these wholesale
would obscure which generated changes actually build. Extend their shared method
model in measured slices instead: this change removes the getter-name restriction
from enum default-method selection, rather than creating another enum-specific
resolver. Kotlin interface properties use accessor methods; companion instances
use ordinary Java singleton objects.

## Commit relevance

| Commit | Assessment for this work |
| --- | --- |
| `feac6e6` | Historical acceptance and remaining frontier; keep measurements tied to their input. |
| `731065b` | Integration checkpoint; preserve its receiver and enum behavior, avoid counting merge statistics as new work. |
| `dab4e89` | Directly relevant enum groups; generalize constructors and multiple groups through one emitter. |
| `3e0e4e2` | Canonical receiver identity matters when rewriting retained Kotlin; preserve. |
| `5369a25` | Trial receiver implementation; consolidated by the following fix, no parallel resolver needed. |
| `93c2cff` | Annotation/parser recovery unlocks real declarations; preserve bounded recovery. |
| `a37f7ca` | Native frontier fixtures characterize remaining blockers; keep. |
| `97c1f0e` | Provider ranking helps choose useful changes; keep aggregate reporting. |
| `8fd81f2` | Constant-field source/caller migration is relevant to companions; preserve shared identity. |
| `5184065` | Nullable String exits and replans affect behavior; preserve snapshot binding. |
| `f896747` | Native JDK conformance guards fallback assumptions; keep. |
| `2b193ed` | Large contract layer prevents silently changing operations; retain contracts, share renderers. |
| `886852b` | Declaration types must remain distinct from call-site observations; preserve after provider collapse. |
| `5538089` | Constructor applicability affects emitted overloads; keep bounded resolution. |
| `5bce1e2` | Helper resource bounds and per-request unknowns remain necessary; preserve. |
| `c160d3f` | Unsupported helper queries should stay local unknowns; preserve. |
| `cb5f054` | String-case facts belong with shared syntax planning; traversal consolidated. |
| `76b6d24` | Forwarding getter bodies are useful for retained interface providers; preserve one bridge model. |
| `e74237f` | Provider diagnostics guide coverage work; keep rather than adding more refusal categories. |
| `7c9722c` | Deprecation annotation translation affects Java compilation; preserve. |
| `f8d59d7` | Static values versus type qualifiers matters for companion fields; preserve. |
| `fc21b49` | Lexical bindings and compiler-only annotations affect correct Java; preserve. |
| `f32c909` | Renamed setters are part of the accessor method model; preserve. |
| `fae5d8a` | Receiver-owned writes must use the chosen setter; preserve coordinated caller edits. |
| `8a5729e` | Release metadata; no architectural constraint. |

## Translation coverage under application-behavior acceptance

This work follows the requested source-migration policy: preserve application
behavior and working callers, while accepting different implementation details
and a mutable collection fallback when needed. Standard-collection subclasses
and incompatible overload families remain outside this coverage expansion.

- Empty enums emit an empty constant list, without dummy instances. Inferred
  literal strings use `java.lang.String` when a workspace type named `String`
  would otherwise capture their Java return or property types.
- Enum companion groups can coexist with enum constructors and ordinary enum
  members, and can contain multiple declared collection properties. Caller
  validation checks the complete supported getter set. Ordered deduplication,
  declared Collection versus Set types, and shared group identity are retained.
- Interface expression getters infer their Java result, rather than defaulting
  every undeclared result to Object. Mutable interface properties remain getter
  and setter methods.
- Interface companion functions emit Java statics plus a real companion
  singleton with forwarding instance methods. Bridges are derived from the
  emitted Java method syntax, sharing return inference, generics, and parameter
  rendering with the emitter. The same extraction replaces the separate Kotlin
  signature renderer used for class companion bridges.
- Enum default bridges also select ordinary no-argument methods, not just
  getter-shaped names. Indexed source-package wildcard imports admit collection
  factories unless the package declares a conflicting factory or infix `to`.
  Unknown library/member wildcard imports still require binding facts.

`tests/enum_interface_coverage.rs` has ordinary output regressions and an
explicit native gate. Its five-owner application compiles as original Kotlin,
fully migrates through the production CLI, recompiles with the same Java driver,
and produces the same runtime result. The driver checks zero enum instances,
physical inferred String returns, constructor values, ordered/deduplicated
collection contents and shared identity, interface accessor behavior, and
companion methods/singleton identity. Private-project gains are measured
separately below.

Run the native gate with `JAVA_HOME`, `NOTLIN_KOTLINC`, `NOTLIN_JAVAC`,
`NOTLIN_JAVA`, and `NOTLIN_KOTLIN_STDLIB` set to installed tools:

```sh
cargo test --locked --test enum_interface_coverage -- --include-ignored
```

## Validation recorded for this working tree

- Formatting and default-target clippy with warnings denied pass.
- All-target clippy is not a clean gate on this checkout: existing test code
  triggers placeholder-name, unnecessary-unwrap and Windows-only unused-mut
  diagnostics. Experimental test-only lint cleanups were reverted; these are
  separate from the successful default-target check.
- Earlier full ordinary Rust run: **1,515 passed / 107 failed / 131 ignored**, across
  238 result groups. This is not a clean Windows gate. All 100 failures from
  the initial checkout remain; the other seven source-property test failures
  were reproduced with untouched accepted-main production sources at `feac6e6`.
  No new failing test names remain after that baseline comparison.
- Native JDK 25 / Kotlin 2.3.20 comparison passes. All five fixture owners
  migrate, and recompilation plus runtime checks agree with original Kotlin.
  Generic and void-returning interface companion methods are included.
- The five Python frontier-tool tests pass.

The final focused ordinary suites pass: 112 tests across semantic, container,
eager-collection, factory-scope and enum-bridge suites. The native enum/interface
and collection-factory suites pass (17 tests including their native gates).
The latter also exercises an indexed source-package wildcard with a retained
Kotlin caller. Other native suites requiring Lombok
remain unverified: a broader ignored-suite attempt failed on missing
`NOTLIN_LOMBOK`; its log is retained as
`final-focused-with-unconfigured-lombok.log`. Only the explicitly configured
native checks described above are counted as executed.

Local command evidence lives under `target/`: `final-test.log`,
`accepted-main-source-property.log`, `final-clippy.log`,
`final-native-test.log`, and `final-focused-test.log`. Ignored native suites
are not counted as executed checks.

## Reference-project measurement

The complete frozen input contains **6,443 source files** (608 Kotlin, 5,835
Java) and **2,683 original declaration owners**. The accepted-main binary was
built from `feac6e6` in its own Cargo target directory. Baseline and candidate
binary hashes differ. A rejected earlier comparison accidentally reused one
binary; those numbers are not acceptance evidence.

On this input and local helper configuration, accepted main translates **2**
owners. The working migration translates **8**, a gain of **6** and loss of
**0**: two enums and four classes. This is a new matched comparison, not an
extension of the historical 20-to-21 measurement on different source bytes and
fact configuration.

Iteration used the already-translated tree. Coverage combines original owner
identities across passes (normalized paths and full symbol identities), rather
than summing unrelated invocation percentages. The final command from the copy's
root is `notlin --in-place --lombok .`, adding only `--coverage-report` to capture
the result. Its final pass emits no further Java files.

The enum experiment initially emitted six additional enums. Gradle rejected
them because inherited overloads require unrelated return types. Those outputs
were removed, their original Kotlin files restored, and the converter corrected
to keep overload signatures distinct. They are excluded from the accepted
eight. The ordinary no-argument default bridge remains covered by a regression.

The original reference directory remains unchanged: all 7,216 snapshot file
hashes match. An early snapshot that excluded nested `state` source packages was
rejected and replaced before this comparison.

Both the untouched control and candidate use the Hastings dev profiles
`local,h2,cors,dev`, project-root working directory and the configured dev-loader
CSV. Port 8080 was already occupied, so startup checks use `--server.port=0`.
The candidate's complete Gradle `build` tasks finish before `tes-app:bootRun`;
the app reaches its started marker in 44.831 seconds and loads the dev resources.
Observation continued for 90.9 seconds after that marker with **zero runtime
error or stack-trace lines**. The disposable test instance was then stopped.

Local evidence is under `target/tes-trial-v2`: `snapshot.json`,
`accepted-baseline-coverage.json`, `candidate-coverage.json`,
`factory-incremental-coverage.json`, `final-incremental-coverage.json`,
`incremental-comparison.json`, `candidate-final-gradle-hastings.log`,
`native-final-regression.log`, `startup-validation.json` and
`original-source-final-verification.json`.
The six rejected enum outputs and the failed startup attempts are retained in
the trial logs as failed experiments, not counted as successful migrations.

## Interface accessor follow-up — 2026-10-27

The reference `IEnumeration.java` was manually translated by the user. Its presence
was not evidence of converter support. This trial removes that Java file in the
disposable copy and supplies the user's original annotated Kotlin interface.
The standard CLI regenerates Java getter declarations, the default `getKey()`
body and Jackson class literals from that input. This regeneration is reported
separately from new ownership gains.

Starting from the previously validated eight-output copy, the interface changes
translate **238 additional owners: 84 interfaces and 154 classes**. Complete
normalized owner identities match the previous 2,675 remaining-owner report;
2,437 remain after conversion. All eight earlier Java outputs remain byte-for-byte
identical. The complete Gradle build tasks pass before `tes-app:bootRun` starts.
Hastings starts in 74.844 seconds with `local,h2,cors,dev`, loads its configured
dev CSV and creates the Hastings organization. After 99 seconds beyond the
started marker there are zero runtime error or stack-trace lines, captured in
`startup-validation.json`. The disposable instance is stopped after observation.
Port selection uses the same random-port override as
the earlier trial.

The changes use one getter/setter model for interface contracts, default bodies,
retained Kotlin implementers and Boolean names. Inferred literal getter results,
custom mutable accessor bodies, companion properties and ordinary methods with
arguments are supported. A concrete Kotlin method default can provide an explicit
forwarding override even when its own interface remains Kotlin, removing a
circular retention dependency.

The first reference build exposed a read-only collection covariance return:
Java rejected `List<Subtype>` as `List<Base>`. The converter now emits an erased
view cast at an explicit read-only return boundary. The failing class was restored
from original Kotlin and regenerated by the CLI; its Java was not hand-fixed.
Native tests verify that getter, expression-body and block-body returns preserve
the same live list and mutations.

`tests/interface_heuristics.rs` compiles and runs original and migrated Kotlin/Java
callers under JVM default modes `disable`, `enable` and `no-compatibility`; both
tests pass. Fourteen existing native default-getter controls also pass. Focused
property, companion, method, collection and transpiler regressions pass, as do
formatting and default-target clippy. The complete library run has 360 passes,
11 failures and four ignored tests, identical to the untouched `feac6e6` baseline;
the 11 failures are not claimed fixed.

Evidence is under `target/tes-interface-trial`: `input.json`, `migration.log`,
`method-default-migration.log`, `matched-comparison.json`,
`previous-output-verification.json`, `collection-fix-migration.log`,
`final-translation-verification.json` and `gradle-hastings-fixed.log`.
The live reference directory changed in 133 snapshot paths during this work;
acceptance remains tied to the frozen copy rather than claiming that the live
tree is byte-identical to the earlier snapshot. The live project was not edited
by this trial.

## Full clean-conversion build gate — 2026-10-27

The earlier incremental acceptance above does not establish that a complete
conversion from clean reference sources builds. The user's unedited applied
conversion produced 3,889 new Java files but failed core API Kotlin compilation.
The failure is reproduced on a complete copy of that resulting source tree.

Two calls in retained Kotlin used `work.request` after the providing Kotlin
property had been rewritten to `fun getRequest()`. The receiver's declared type
was imported with `work.*`; the caller repair required an explicit import or a
same-package classifier and therefore missed both calls. Property caller repair
now accepts a uniquely imported indexed classifier through a structural wildcard
import. Explicit imports, same-package classifiers and ambiguous wildcard imports
retain their precedence; the global simple-name fallback is not used. The
stricter type proof for JVM covariance contracts is unchanged.

All 33 property caller-repair unit tests pass, including import ambiguity and
shadowing controls. The CLI fixture reproduces a retained provider and retained
caller in different packages connected through a wildcard import. Original and
migrated Kotlin/Java callers agree under all three JVM default-method modes.
Fourteen covariance regressions, formatting and default-target clippy also pass.

Reference commit `179bb912a3dca0063261103d27448496b58df898` is exported without
resetting the user's worktree: 972 Kotlin and 2,147 Java sources, 6,371 original
Kotlin owners. Its untouched core API Kotlin compilation passes. A diagnostic
copy of the failed output builds after correcting the two expressions, but that
manually repaired copy is excluded from final acceptance. Final acceptance uses
the standard CLI on another clean export, with no manual Kotlin or Java fixes.

Local evidence is under `target/build-gate-20261027`: `input.json`,
`initial-gradle-build.log`, `clean-baseline-coreapi.log`,
`diagnostic-gradle-build.log`, `fresh-input.json`, `fresh-migration.log`,
`fresh-coverage.json`, `property-callsite-tests.log` and `receiver-native.log`.

The clean conversion emits **3,888 Java files in eight passes**, taking 12m02s
while the diagnostic Gradle build overlaps it; this is not a controlled timing
comparison with the user's 7m51s run. `UIEquipmentDto` remains Kotlin because its
rewritten getter chain has an ambiguous `getCategory` fact. It is the sole Java
output difference from the user's applied tree. The full clean-conversion Gradle
build tasks pass before Hastings starts. Hastings starts in 56.019 seconds,
loads the dev CSV and creates its organization; 133.7 seconds of observation
after the started marker contain **zero error or stack-trace lines**. The
disposable instance is stopped afterward. See `fresh-gradle-hastings.log` and
`startup-validation.json`.

The user's existing applied tree is also repaired, with a strict check that its
caller file still matches the reproduced input before replacing the two
expressions. Its original bytes are backed up in `task-model-before-repair.kt`.
All **6,036 existing Java source files are byte-identical** to the reproduced
input, so its original 3,889 generated outputs are preserved. Its own full
Gradle `build` passes: **110 actionable tasks, all 110 executed**, in 6m07s.
See `live-reference-repair.json`, `live-java-verification.json` and
`live-reference-gradle-build.log`. This live repair does not substitute for the
independent clean-conversion gate above.

## Follow-on coverage and application construction

Unique source wildcard imports now prove covariant getter returns as well as
property callers. Compatible inherited getters select the narrowest source
return type; incompatible, nullable and ambiguous branches still retain their
existing checks. Typed instance properties are visible to method analysis even
when declared later. Enum emission registers computed getters before emitting
methods, so those reads call the accessor instead of an absent backing field.
Declaration annotations discard parsed comments before lowering arguments;
disabled class literals in comments no longer retain an otherwise valid type.

The baseline trial gained 21 owners (five interfaces and 16 classes). Final
clean-conversion acceptance gains **36 owners: six interfaces and 30 classes**,
producing **3,924 Java files** without losing a previously translated owner.
Hastings initially exposed missing constructor defaults and then missing
parameter annotations during its warehouse import, despite successful builds.

Lombok data classes with supported defaults now receive an explicit Jackson
factory. The ordinary constructor keeps its parameter types, null checks,
`@Default` and `ConstructorProperties`; Jackson routes through the factory.
Defaulted primitives are boxed only at that entry point. A contextual parameter
deserializer distinguishes omitted values from explicit null, delegates supplied
values to Jackson, and preserves Jackson's primitive-null policy. Defaults are
evaluated in constructor order. Source parameter annotations also belong on the
factory: the application's `@Type` and `@Parent` resolve lookup references there.
The existing metadata-retention rules remain in force, including nullable
non-null defaults that require additional presence tracking.

Native tests compare original Kotlin and generated Java for missing values,
supplied values, `false`, nullable properties, explicit reference/primitive null,
defaults depending on earlier parameters, map conversion, and a contextual
deserializer reading a parameter-only annotation. All five Jackson tests pass.
The enum/interface fixture also passes under all three JVM default modes. The
88 related Rust regressions and default-target clippy pass; the library suite
has 361 passes and the same 11 failures reproduced on the unchanged baseline.

Final acceptance exports all files from reference commit
`179bb912a3dca0063261103d27448496b58df898`, including 972 Kotlin and 2,147 Java
sources. The standard `notlin --in-place --lombok .` invocation adds only a JSON
coverage report. Seven passes take 11m45s while validation overlaps. The full
Gradle build passes in 4m21s, **110 tasks all executed**. Hastings starts in
46.415s, imports 50 organization records and 9,063 warehouse records, and logs
`Done loading dev resources`. Another **93.4 seconds after that marker contain
zero errors or stack traces**. No sources or application configuration in this
clean acceptance copy are hand-fixed.

The first clean startup was interrupted by the laptop closing. Hikari reports
a 33-minute scheduling gap; that run fails with an empty H2 database. The
successful retry uses exactly the same converted sources and configuration.
The failed log is preserved alongside the accepted retry. This is separate
from the earlier converter defects, which require production fixes.

Evidence is under `target/coverage-boost-20261027`: `clean-v3-input.json`,
`clean-migration-v3.log`, `clean-coverage-v3.json`, `clean-gains-v3.json`,
`clean-gradle-build-v3.log`, `clean-hastings-v3-retry.log`,
`clean-runtime-v3-validation.json`, `defaults-context-native.log` and
`library-comparison.json`. The release binary SHA256 is
`c0061c243b7b37426b30cc93d947d6945a47fca54799807da057549ed89d7ba0`.
