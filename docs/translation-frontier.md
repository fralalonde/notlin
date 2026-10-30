# Translation frontier

## Current checkpoint — 2026-10-30

The standard fresh conversion of reference commit `179bb912` now produces
**3,940 Java files**, retaining 2,431 of 6,371 original Kotlin owners:
**+7 / -0** against the accepted 3,933-output baseline. The gains are
`ResourceActivityEntityDto`, `OAuthAuthenticationDto`,
`PlcDeviceWriteInstruction`, `IntegrationAdjustmentInfo`,
`ItemUnitPositionTypeDto`, `APIKeyCredentialsDto`, and `OAuthCredentialsDto`.

Visible string constants resolve across explicit, wildcard and aliased imports;
their consumers can migrate while a constant declaration stays Kotlin. Concrete
forwarding getter chains no longer reject an unrelated owner type parameter.
Compatibility with retained parents reuses instantiated getter types and checks
the child's complete getter obligations, replacing the older duplicate checks.
Jackson-observed data classes with literal or enum-entry defaults can use a
generated builder, preserving omission, explicit values, null checks and implicit
JSON naming. Custom binding, generic DTOs and nullable non-null defaults retain.
The builder also carries an established inherited
`JsonIgnoreProperties(ignoreUnknown = true)` contract. This preserves the original
request DTO behavior without changing strict DTOs to accept unknown fields.

The fresh standard invocation establishes the seven gains above. No application
source or runtime configuration was hand-fixed. Evidence and the frozen
`notlin-final.exe` are under `target/property-frontier-20261030/`; the fresh
translated tree is `clean-v3`. The executable SHA256 is
`006d041ffa8bcd9de6bfec1d868f7f71bf025d40f906a4b8e65e57da4cdf6353`.
The fresh full Gradle build passes in 7m04s: **110 tasks**, 80 executed and
30 from cache. Hastings loads all **9,113 records** (50 organization records and
9,063 warehouse records), followed by **105.9 seconds** with no errors or stack
traces after `Done loading dev resources [customers/hastings/hastings-dev.csv]`.
The disposable application is stopped after observation. The earlier candidate
that lost inherited ignore-unknown behavior is rejected; its failed startup logs
are preserved separately under `rejected-*`.

The frozen CLI passes the native before/after comparison in both Lombok modes:
literal string splitting and trailing empties, local shadowing, generic parent
getter dispatch and evaluation count, JSON defaults, explicit enum values,
renamed properties, snake-case naming, polymorphic reads, null rejection,
inherited ignore-unknown behavior and strict unknown-field rejection.
Formatting and clippy pass. Three standalone samples emit Java and compile;
`Nullspotting` remains Kotlin. The full host Rust sweep has **1,569 pass,
72 fail, 136 ignored**, with no newly failing test names against the recorded
baseline. The intermittent file-deletion race passes this run; it was not fixed.
The separate sandbox sweep also records its shell/cache/symlink limitations.

The two item-unit DTOs no longer stop at `LOOKUP_SEPARATOR`; their chained
`getType().getId().split(...).last()` calls still lack the required string-return
proof. The workflow command examples still retain their parent interfaces after
the concrete getter body is established. These remaining boundaries are not
reported as translation gains.

## Previous checkpoint — 2026-10-30

The standard clean conversion of reference commit `179bb912` produces
**3,933 Java files**, retaining 2,438 of 6,371 original Kotlin owners:
**+3 / -0** against the accepted 3,930-output baseline. The gains are
`ExpiryProperty`, `LotNoProperty`, and `JobActivity`.

Enum companions share the ordinary companion emitter, including map initializers,
declared collection types and property getter/setter bridges to shared static
storage. Ordinary instance methods named `with`, `let`, or another builtin
spelling use their resolved source target. Parameter ABI and indexed companion
ownership facts keep mangled methods and companion-holder calls out of that
ordinary instance path. Secondary-constructor lookup now includes Java receiver
types and their inherited methods.

The `with` blocker is removed from `SystemValue`; the class still stays Kotlin
because its parent `PropertyValue` is retained for ambiguous `of` overloads.
`OrderActivityLineInfo` now reaches its separate inherited `getItems` proof
boundary. These are additional requirements beyond method-name dispatch.

The fresh full Gradle build passes: **110 tasks**, 44 executed and 66 from cache.
Hastings loads all **9,113 records**, followed by 105.5 seconds with no errors or
stack traces after the complete-import marker. No application sources or runtime
configuration were hand-fixed. Evidence is under
`target/generic-translations-20261030/`, using `notlin-indexed.exe` and `clean-v4`.
Every generated Java file matches the earlier fully built and started candidate;
ten retained Kotlin files differ only in retention notes.

The native regression passes in both Lombok modes, covering map nulls, duplicates,
order, identity, mutable companion state, Java companion callers, ordinary method
dispatch, constructor argument evaluation, and retained companion/mangled-method
boundaries. Three standalone samples emit Java and compile; `Nullspotting` remains
fully retained, as in the baseline. Formatting and clippy pass. The full Rust
sweep remains red: **1,563 pass, 73 fail, 135 ignored**; every failing test name
appears in the recorded prior run, including the intermittent file-deletion race.

## Previous checkpoint — 2026-10-29

The standard clean conversion of the same reference commit `179bb912` produces
**3,930 Java files**, with 2,441 of 6,371 original Kotlin owners retained:
**+6 / -0** against the previous accepted conversion. Checked instance
`lateinit` accessors unlock `GetBinStateResponseParams` and `ImportOrderLineDto`.
Scoped annotation class literals, including imported dependency nested classes,
unlock `TOHPickRequestDto`, `TOHPutawayRequestDto`, `TOHItemMasterRequestDto`
and `TOHLocationDto`. The baseline trial translates the five owners in the two
agent source files before the full clean pass confirms all six gains.

The fresh conversion passes the complete Gradle build: **110 tasks**, 80 executed
and 30 from cache. Hastings loads all **9,113 records** (50 organization and
9,063 warehouse records) with no errors or stack traces. Acceptance uses the
unchanged Hastings dev profile and no hand-fixed application sources.
Evidence is under `target/field-annotation-20261029/`.

The six focused tests pass, including a real Kotlin/Java/Jackson comparison in
both Lombok modes covering initialization checks, null setters, naming-strategy
serialization/deserialization, and parent back-references. All four standalone
samples are checked; three emit Java and compile while `Nullspotting` stays
Kotlin. Formatting and clippy pass. The broader Rust sweep is not
green on Windows: 1,560 pass and 74 fail, including shell-helper fixtures and
an intermittent access-denied failure while deleting indexed source files.
Those results are retained separately from application acceptance.

## Previous checkpoint — 2026-10-27

The standard clean conversion of reference commit `179bb912` now produces
**3,924 Java files**, with 2,447 of 6,371 original Kotlin owners retained.
That is **+36 / -0** against the earlier 3,888-output conversion: six interfaces
and 30 classes, including `IWorkRequestContext`, `EquipmentDto`,
`EquipmentTypeInfo` and `UIEquipmentDto`. Seven passes took 11m45s while other
validation overlapped; this is not a controlled performance comparison.

Its complete Gradle build passes with **all 110 tasks executed** in 4m21s.
Hastings starts in 46.415s and completes both imports (50 organization records
and 9,063 warehouse records). Observation continues for 93.4 seconds after
`Done loading dev resources`, with **zero errors or stack traces**. An earlier
startup was interrupted by the laptop closing; the unchanged retry supplies
this runtime result. No acceptance sources or application configuration were
hand-fixed. See the follow-on section of
[the architecture review](architecture-review.md) for construction-default,
parameter-annotation and native-test evidence.

## Earlier working checkpoints

The earlier 3,888-output clean run repaired source-wildcard property callers.
The user's existing 3,889-output tree also passes its full build after two
missed Kotlin caller expressions are repaired; all its Java sources remain
unchanged. Its previous 133.7-second startup observation preceded the stronger
complete-import gate above. The later getter-resolution change also translates
the DTO that this earlier conversion retained.

The interface follow-up lowers properties, default accessors and retained Kotlin
implementers through one method model. It also handles Boolean accessor names,
companion properties, methods with fixed arguments and forwarding to retained
Kotlin method defaults. On the previously validated eight-output copy this
unlocks **238 additional owners: 84 interfaces and 154 classes**, with all eight
earlier outputs unchanged. The user's manually translated `IEnumeration.java`
was replaced with their original Kotlin in the disposable input and regenerated
by notlin; that proof is separate from the ownership gain. See the interface
follow-up in [the architecture review](architecture-review.md) for tests,
the collection-return fix and full application acceptance evidence.

The architecture/coverage refactor on top of `feac6e6` removes the provider
forwarding layer and combines independent syntax planning walks. It also admits
zero-constant enums, multiple companion groups on parameterized enums, inferred
interface getter results, and interface companion functions with singleton
forwarders. See [the architecture review](architecture-review.md) for scope and
verification. The new five-owner application fixture fully migrates and matches
its original Kotlin runtime result. A new complete 6,443-source / 2,683-owner
comparison measures **2 → 8, +6 / -0** on the local fact configuration. The gain
is two enums and four classes; source-package wildcard imports no longer block
unshadowed collection factories. Six experimental enum outputs with incompatible
inherited overload return types were rejected by Gradle and restored to Kotlin.
The corrected candidate completes its Gradle build tasks and starts under the
Hastings dev profile. See the architecture review for evidence and invocation
details. The accepted checkpoint below is historical evidence for the prior tree
on different inputs and must not be added to this measurement.

## Historical checkpoint — 2026-10-26

The bounded enum companion-group slice is complete in main integration
`731065bd` (feature `dab4e891`), together with the accepted receiver/getter repair.
On identical frozen **6,444-source / 2,681-owner** inputs, translated owners rose
**20 → 21, +1 / -0**. Full migrated Gradle build passed, 106/106 actionable tasks
executed; migration was 263.275s versus 272.358s for the accepted baseline.

See [the handoff](translation-handoff.md) for precise source/CLI/helper identity,
1,630 ordinary passes, native five-case results, scope and remaining #2/#6 work.
This is source-migration acceptance, not full companion-object ABI equivalence.
The earlier annotation experiment below used different input bytes and owner
counts; its figures are historical and must not be added to this checkpoint.

## Historical checkpoint: annotated marker declarations

## Verified slice

Parameterless annotation declarations preceded by annotations can now use the existing annotation emitter instead of remaining opaque parser boundaries. Planning and emission share one recovered declaration. No annotation target/retention mapping, JVM helper protocol, or metadata-loss policy was changed.

The pinned grammar parses `@Target(...) annotation class Marker` as an annotated infix expression. A plain prefix reparse does not fix it. Recovery first tries the established interface path; for a bodyless annotation whose name ends its wrapper, it appends an explicit empty constructor to disambiguate the grammar, verifies the declaration's name/kind/bounds and the exact empty suffix, removes only the synthetic coordinates, and validates the original bounds again. No synthetic constructor or member is emitted.

## Workload result

On the same frozen 6,443-file input snapshot:

| Measure | Approved baseline CLI | Candidate |
|---|---:|---:|
| Original declarations | 2,683 | 2,683 |
| Translated | 20 | 22 |
| Retained | 2,663 | 2,661 |
| Gained / lost | — | +2 / -0 |

Both gained identities are annotation declarations. Complete declaration identities, ownership scope and source hashes match. Both migrated reference builds succeeded; baseline executed 3 tasks with 107 up-to-date, candidate executed 24 with 86 up-to-date. These were incremental `build` runs, not clean rebuilds.

The previous input digest no longer matched the original reference bytes despite unchanged Git status and HEAD. The comparison therefore uses a newly captured immutable source-only snapshot and reruns both binaries, not the old report. Input SHA256: `ec91de55bafe2582b72d077eb0bff35dd324c1afc459ab4aee6cc9ef244c8ea2`. The original project was not migrated, edited or committed.

## Validation

- Production CLI regression with metadata, an annotated receiver and a translated constant in one physical file.
- Custom source `Target("custom")` remains that exact annotation/value and is not mapped to the Kotlin builtin. The old test's opaque-retention assertion is replaced by this stronger binding assertion because the declaration is now recoverable.
- PROPERTY targets still retain; native controls also cover malformed boundaries, comments/Unicode names and partially retained files through repeated migration.
- Exact annotation class flags `0x2601`, `java.lang.annotation.Annotation`, zero physical fields/methods/constructors, runtime retention, JDK target arrays and meta-annotation visibility.
- `tests/annotation_recovery_jvm.rs` compiles Kotlin and Java callers once against the original, runs those same class files against actual emitted Java and checks loaded origins. It also exercises Kotlin annotation construction and `primaryConstructor.call()`. Missing native prerequisites fail when the ignored test is explicitly run; there is no silent self-skip.
- Existing annotation-emitter policy still omits Kotlin metadata/target meta-annotations and can widen Kotlin-specific target constraints. This is not a claim of byte-identical class files or equality of `kotlin.Metadata` observations.
- Ordinary fmt, default-target clippy, tests/build and qualified test inventory; compatibility comparisons and sample javac checks with helper off/on; explicit default-getter native controls.

Run the native gate with configured absolute tool paths:

```sh
NOTLIN_JAVA="$JAVA_HOME/bin/java" \
NOTLIN_JAVAC="$JAVA_HOME/bin/javac" \
NOTLIN_KOTLINC="$KOTLINC" \
NOTLIN_KOTLIN_STDLIB="$KOTLIN_STDLIB" \
cargo test --locked --test annotation_recovery_jvm -- --ignored --nocapture
```

The compiler's adjacent `kotlin-reflect.jar` is required. Keep an isolated absolute `CARGO_TARGET_DIR` for concurrent builds.

## Performance

Observed migrations: 219.410s baseline, 262.968s candidate (+19.85%). The candidate converged in three speculative rounds versus two; the third ownership pass took 42.247s. Earlier native activity overlapped measurement, so this is not a controlled speed benchmark. Preserve the extra correctness pass; do not stop early to manufacture a faster result.

## Other measured areas

The retention-frontier tool is `tools/retention_frontier.py`; it emits only aggregate counts and opaque provider IDs unless a private mapping is explicitly requested. Do not publish the private mapping.

1. **Retained default providers:** 527 blocked declarations, 72 provider roots. The seemingly small fixed-return generic family also inherits generic covariant and annotated-default obligations. Exact physical bridge flags/default mode and complete transitive ownership need proof before widening.
2. **Secondary constructors:** 39 first-visible refusals, mostly data classes. A small collection wrapper still has 12 compiler-generated mapped-collection members. A manual Java rewrite passed 19 behavioral checks but failed an original-compiled caller with `NoSuchMethodError(getSize)`. Body-call facts, bare-supertype kind, varargs/generic signatures and whole-owner ABI are separate obligations.
3. **Jackson defaults:** 697 declarations carry the omitted-versus-explicit-null compatibility blocker. Treat this as a presence-aware constructor/metadata project; ordinary Java/Lombok defaults or deleting the guard do not establish equivalence.
4. **Missing facts:** select a measured lowering consumer before adding resolution. Complete source/binary identity and the emitted operation/ABI must be proved separately.

Counts overlap and are not forecasts of recovered declarations. See `tests/fixtures/frontier/README.md` for preserved generic characterization fixtures, including the withdrawn contaminated-index result.

## Independent helper-policy observations

The broader exploratory harness records helper-startup checks separately from annotation correctness. An unavailable required helper still returns success on the same no-facts-needed input in both approved baseline and candidate; this is pre-existing behavior, not fixed here. When all selected Kotlin annotations move, the helper may find an empty source root. A separate retained-anchor control established real full-helper startup. The exploratory harness's failed checks remain recorded; they are not silently renamed passing annotation gates.
