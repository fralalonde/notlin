# Translation frontier

## Current checkpoint — 2026-10-31

The standard fresh conversion of reference commit `179bb912` produces
**4,417 Java files**, retaining 1,954 of 6,371 original Kotlin owners:
**+37 / -0** against the accepted 4,380-output baseline. The baseline trial
gains the same 37 owners; all original owner identities are compared. Gains
include `EquipmentCategory`, `ContainerId`, `DeviceId`, `EquipmentId`,
`InventoryContainerImportRequest` and `JobContainerImportRequest`.

Eight no-argument text operations share typed receiver and callable binding:
`trim`, `trimStart`, `trimEnd`, `isBlank`, `isNotBlank`, `isNullOrBlank`,
`isNullOrEmpty` and nullable String `orEmpty`. Kotlin helpers preserve Unicode
whitespace behavior and CharSequence views. Aliases, safe calls and effectful
receivers retain their identities and evaluation counts. An indexed source
wildcard can prove the absence of a conflicting standard type name.

Expression-bodied calls infer their result from the selected source declaration,
including qualified parent-interface calls. Java return facts preserve boxing,
explicit nullability and platform types. An explicit getter reading its own
`@JvmField` property reads the backing field directly instead of recursively
calling itself; ordinary accessor dispatch elsewhere remains intact.
The native fixture independently compiles original Kotlin and compares both
Lombok modes, including default nullness annotations, producer-scope types,
Unicode values, receiver effects and backing-field reads.

The I/O experiment reduced reads by 54.47% and bytes read by 28.05%, but took
**125.281 seconds versus 115.219 seconds before**. At the user's request, the
experiment is reverted: migration snapshot collection and stale-output cleanup
use their original implementations. The translation and getter fixes remain.

Both full Gradle builds pass: baseline **2m 41s**
(110 actionable tasks: 44 executed, 66 from cache), fresh **1m 24s**
(110 actionable tasks: 44 executed, 66 from cache). Both Hastings runs load
**9,113 records**, then remain free of errors and stack traces for **105.8**
and **105.6 seconds** after the complete dataset marker. Both disposable apps
are stopped. No generated application source is patched.

Of 6,564 Java files in each tree, 6,555 match as text. All nine variants are
reviewed through source and compiled instructions. These include getter
scaffolding, nullness annotations, member order, equivalent string conversion,
and corrections to recursive getters already present in the old baseline.
The audit explicitly records those intended corrections rather than treating
their old recursive behavior as a requirement.
The I/O revert leaves all 6,564 Java files unchanged in each conversion path
compared with the experimental executable.

Formatting, lint, four standalone samples and frozen native checks pass. The
Windows Rust sweep is **1,610 pass, 62 known fail, 142 ignored**, with no new
failure names. It uses CI's debug-symbol and incremental settings. The preceding
intermittent source-deletion failure is absent. Exact-head Linux CI is required
before installing the frozen executable.

Evidence: `target/text-whitespace-final-20261031/acceptance.json`.
The declined experiment is preserved in `target/optimization-pass-20261031/io-comparison.json`.
Frozen SHA256: `13019a5b24fbdd7d5420eb8a56bab2e41ac7bbeaecaefdd8a3a7348b31392c50`.
The translation frontier remains active.

## Previous checkpoint — 2026-10-31

The standard fresh conversion of reference commit `179bb912` produces
**4,380 Java files**, retaining 1,991 of 6,371 original Kotlin
owners: **+6 / -0** against the accepted 4,374-output baseline.
The baseline trial gains 6 owners. All 6,371 original
owner identities are compared. New owners: `IJobAssetInfo`, `IOrderAssetInfo`, `IWorkAssetInfo`, `IWorkflowAwareAssetInfo`, `IWorkflowAwareStorageAssetInfo`, `EmptyWorkerInfo`.

Java source facts now distinguish resolved member-neutral library metadata from
annotations with unknown effects. The shared fact supports declared getter reads
and field-absence checks. Jackson metadata on a Java ancestor therefore no longer
prevents a proven standard `TODO()` from becoming `throw new NotImplementedError`.
Non-null and Java platform getter results feed ordinary receiver chains.
Explicitly nullable results retain their guard; the nullness contract distinguishes
them from platform results even though both have a nullable `TypeRef` bit.

Annotation identities are checked against imports, source packages, lexical and
inherited nested types. Java annotation declarations join the semantic type index.
Unknown wildcard packages, conflicting classifiers, custom processors, nullness
defaults and callable fields keep their independent guards. Serialization metadata
never establishes nullability or waives constructor/Kotlin-metadata contracts.
The source package query is indexed; the index cache version advances to 33.

The native fixture independently compiles original Kotlin/Java, then compares it
with both Lombok modes using the frozen executable and actual Jackson libraries.
Getter values and evaluation counts, polymorphic JSON serialization and
deserialization, standard TODO exception classes/messages and an explicitly bound
callable field match. Unit checks cover nullness, unknown effects, source wildcard
conflicts and an inherited annotation type that shadows an imported library type.

Both full Gradle builds pass: baseline **4m**
(110 actionable tasks: 44 executed, 66 from cache), fresh **4m 20s**
(110 actionable tasks: 80 executed, 30 from cache). Both Hastings runs import **9,113 records**,
then remain free of errors and stack traces for **105.6** and
**105.9 seconds** after the complete dataset marker. Both
disposable applications are stopped. No generated application source is patched.

All **6,527 Java files match as text** between baseline and fresh
conversion.
Formatting, required lint, four standalone samples and frozen native checks pass.
The permissions-matched Windows Rust sweep is **1,603 pass,
63 fail, 141 ignored**, with no new unrecorded failure names against
the preceding accepted checkpoint. The previously recorded source-deletion
timing test fails in the full sweep and passes its isolated rerun. Final-head Linux CI is required before install.

Evidence: `target/java-metadata-frontier-v2-20261031/acceptance.json`.
Frozen SHA256: `057ba2d1e77012816087c1fec7ffa551788a761c262f925a047792c603e63e74`.
A separate original-Kotlin probe reproduces the pre-existing Unicode `trim()`
difference in both the preceding executable and this candidate; its code-point
outputs are in `next-trim-characterization/results.json` beside the evidence.
That is the next behavior correction; wildcard type binding also remains open.
The translation frontier remains active.

## Previous checkpoint — 2026-10-31

The standard fresh conversion of reference commit `179bb912` produces
**4,374 Java files**, retaining 1,997 of 6,371 original Kotlin owners:
**+12 / -0** against the accepted 4,362-output baseline. The baseline trial gains
the same twelve owners: five classes and seven interfaces. Gains include
`IWorkerInfo`, `IWorkerPoolInfo`, `IEquipmentInfo`, `IAgentEquipmentInfo`,
`WorkerInfo` and `AgentEquipmentInfo`. All 6,371 original owner identities are
compared; the complete owner list is in the evidence.

Ordinary source reference returns can override methods on retained Kotlin
ancestors through javac's covariant bridges. The override guard now shares the
source-scoped subtype proof already used by default-getter bridge planning.
It follows source inheritance across Kotlin and Java, keeps primitive and value
class JVM shapes outside this path, and distinguishes method-scoped type
parameters from same-named source classifiers. Other generic return shapes and
property ownership continue to use their separate contract proofs.

Generic inheritance carries the source file where each concrete return argument
was introduced. For example, `Root<T>.getId(): T` may acquire its actual `Id`
binding from a Java intermediate in another package. Resolving `Id` in Root's
file loses that binding; the conflict planner now records its actual source
scope and includes it in traversal identities. Cached method metadata records
type parameters, so the index cache version advances to 32.

The native fixture independently compiles original Kotlin, then compares mixed
Java/Kotlin behavior in both Lombok modes with the frozen executable. It covers
class and enum implementations, interface defaults, overloads, a cross-package
generic ancestor, retained Kotlin descendants and callers, and evaluation
effects. Primitive/value-class and method-type-parameter guards execute too.
The actual compiled `WorkerInfo.getId(): IObjectId` bridge delegates directly
to its narrower `IResourceWorkerId` getter.

Both full Gradle builds pass: baseline **11m 4s**
(110 actionable tasks: 80 executed, 30 from cache), fresh **7m 18s**
(110 actionable tasks: 70 executed, 40 from cache). Both Hastings runs import all **9,113
records**, followed by **105.2** and **105.9 seconds**,
respectively, without errors or stack traces after the complete dataset marker.
Both disposable applications are stopped. Application sources receive no manual
fixes.

Each conversion produces 6,521 Java files. Of these, 6,519 match as text; the
remaining two differ only by explicit `this` qualifiers on instance getters.
Their complete `javap -p -s -c` outputs match exactly. Formatting, lint, four
standalone samples and the frozen native checks pass. The permissions-matched
Windows Rust sweep is **1,597 pass, 62 fail, 140 ignored**,
with exactly the preceding checkpoint's failure names and no new failures.
The initial sandbox run's additional permission/helper failures are preserved
separately in the evidence.

Evidence: `target/reference-method-covariance-frontier-20261031/acceptance.json`.
Frozen SHA256: `1be29b3111e0fbcdd62b192a7425ccbe6d5ace682f015a5d467ece363c9e2801`.
The translation frontier remains active.

## Previous checkpoint — 2026-10-31

The standard fresh conversion of reference commit `179bb912` produces
**4,362 Java files**, retaining 2,009 of 6,371 original Kotlin owners:
**+5 / -0** against the accepted 4,357-output baseline.

New owners: `PickOrderBatchProperties`, `AccessGateProperties`,
`WorkstationGateProperties`, `WmsJobProperties` and `IWorkflowOrderBatchProperties`.
All 6,371 original owner identities are compared.

The baseline trial gains one interface; the fresh pass gains four additional
classes. Their already-Java supertype signatures trigger the existing covariant
return guard in the baseline trial. Added files are checked against these exact
fresh-only owners; all 6,505 common Java outputs match after generated temporary
names are normalized.

Proven standard map operands now support `left + right` and `left.plus(right)`
through Kotlin's static map helper. The shared binding follows parameters,
locals, constructor properties, companion properties, source returns, aliases
and nested merges. Java static fields are indexed with their declared types,
including implicit interface statics, so the same operation works after a
provider has already migrated. Static field reads preserve their actual names,
including lowercase names. Own source properties such as `holder.values` take
precedence over same-named collection intrinsics. Typed locals initialized from
covariant collection parameters keep the same collection reference through an
erased Java cast. The map overload is selected explicitly for target-typed
collectors; both operands are evaluated once, left to right.

The native fixture independently compiles original Kotlin, then compares mixed
Java/Kotlin behavior in both Lombok modes using the frozen executable. It checks
null keys and values, duplicate-key replacement, iteration order, independence
from later input mutations, generic maps, property/companion/static receivers,
locals, chains and effectful calls. Custom maps/operators, foreign imports,
nullable operands and shadowed bindings retain their guards. Map-plus-Pair,
map removal and collection/sequence operand families are separate work.

Both full Gradle builds pass: baseline **9m 39s**, fresh **8m 56s**
(110 actionable tasks: 80 executed, 30 from cache). Both Hastings runs import
all **9,113 records**, followed by **105.0** and **106.0 seconds**, respectively,
without errors or stack traces after the complete dataset marker. Both
disposable applications are stopped. Application sources receive no manual fixes.

Formatting, lint, focused tests, the frozen native fixture and all four standalone
samples pass. The Windows Rust sweep is **1,594 pass, 62 fail, 139 ignored**;
its existing failures are compared by target and name against the preceding
accepted checkpoint. No previously unseen failures remain. Linux CI on
`541e586` passes the complete suite, build and Java end-to-end checks after
updating retention fixtures and reducing CI debug artifacts to fit the runner.

Evidence: `target/map-algebra-frontier-v2-20261031/acceptance.json`.
Frozen SHA256: `d8735a2adbc6f2493c78b82bdf021854d0f9c44661d64720d8cf27e6fe1289e4`.
The zero-gain candidate is preserved under `target/map-algebra-frontier-20261031/`;
its disposable workers were stopped before correcting Java static-field binding.
The translation frontier remains active.

## Previous checkpoint — 2026-10-31

The standard fresh conversion of reference commit `179bb912` produces
**4,357 Java files**, retaining 2,014 of 6,371 original Kotlin owners:
**+403 / -0** against the accepted 3,954-output baseline. The gains include
146 interfaces, 245 classes and 12 enums.
Examples include `EthernetIPPLCProperties`, `ModbusPLCProperties`, `PickOrderConsolidationProperties`, `IntegrationState`, `IIntegrationState`, `EquipmentStateEnum`, `AgvState`.
The ownership comparison checks all 6,371 original declarations.

Type-level Jackson deserializers now have an effective inherited contract.
A custom whole-type decoder owns construction; its ordinary no-argument and
map constructors can translate without introducing automatic Jackson creator
binding. A nearer `JsonDeserialize` annotation replaces the inherited binding,
including omitted `using` and `JsonDeserializer.None`. Parameter and field
deserializers remain part of automatic constructor binding. Import aliases,
foreign annotations and unresolved type aliases retain distinct handling.
The native fixture exercises the actual reflective construction pattern,
defaults copied from an immutable map, input merging, null policy, fresh mutable
maps and serialization.

Memberless interfaces can forward inherited abstract Kotlin property accessors
when declared Gradle settings establish modern JVM defaults. Mutable accessors
and generic properties work without copying getter bodies. Legacy or unresolved
settings and inherited concrete getters keep their existing guards. Compiler
configuration files and absent alternative files participate in migration write
snapshot verification. Native Kotlin/Java comparisons exercise default,
`disable` and `all` JVM modes in both Lombok modes, including retained Kotlin
callers and the concrete-getter negative case.

Retained Kotlin enums now use the same explicit default selection as emitted
Java enums: the first direct interface branch providing the implementation.
This prevents conflicting JVM defaults after providers move to Java. Return
inference preserves primitive, nullable, platform and generic return shapes.
Still-Kotlin property contracts keep their existing safeguard. Native comparisons
reverse provider order and exercise inherited branches. In no-compatibility mode,
the original Kotlin implicit enum case already fails; that fixture uses explicit
source selections so the comparison starts from a working application.
Both reference passes add the same five explicit selections: the equipment
`getTypeClass` default and four integration enum `getCategory` defaults.

The baseline full Gradle build passes in 10m 7s;
the fresh full Gradle build passes in 5m 21s:
**110 actionable tasks: 80 executed, 30 from cache**. Hastings imports all **9,113 records** (50 + 9,063),
followed by **105.8 seconds** without errors or stack traces after
`Done loading dev resources [customers/hastings/hastings-dev.csv]`.
The uniquely marked disposable application is stopped after observation.
No application source was hand-fixed.
The baseline variant independently imports all 9,113 records and completes
105.7 seconds of clean observation before it is stopped.

All eight focused fixture checks pass with the frozen executable. All four
standalone samples compile; formatting and default-target clippy pass.
The Rust sweep remains red: **1,578 pass, 72 fail,
138 ignored**, with no previously unseen failing names.
There are 35 new-owner Java variants between the baseline and fresh
passes; their raw diffs are recorded in `baseline-vs-fresh-java.json`.
The 37 changed files are audited: generated temporary names, declaration
annotations, same-package interface-super qualification, and an existing DTO's
Lombok Value/Data choice. That DTO has 17 explicit final fields and an explicit
constructor; its remaining behavioral source agrees. Both complete outputs
pass their respective builds.

Evidence is under `target/custom-decoder-frontier-v3-20261031/`, using
`notlin-custom.exe`, `incremental`, and fresh `clean`. Its SHA256 is
`ee460f96f50ed10abf89ffc8c5e13872f470211f8f0d9e2a03e1673cb504efd5`. `acceptance.json` records frozen source and input identities,
coverage, builds, tests, output comparisons and runtime observation.
The rejected runtime candidate is preserved under
`target/custom-decoder-frontier-v2-20261031/`: both builds passed, but Hastings
exposed the retained enum's conflicting default getters. The earlier rejected coverage candidate is preserved under
`target/custom-decoder-frontier-20261031/`; it lost one previously translated
interface. Work continues on established map algebra and the retained frontier.

## Previous checkpoint — 2026-10-31

The standard fresh conversion of reference commit `179bb912` produces
**3,954 Java files**, retaining 2,417 of 6,371 original Kotlin owners:
**+10 / -0** against the accepted 3,944-output baseline. The gains are
`APIKeyAuthenticationDto`, `BasicAuthenticationDto`, `LightDeviceInstruction`,
`ItemLocationDto`, `ItemPositionDto`, `ItemUnitPositionDto`, `FacilityDto`,
`OrganizationDto`, `UIAPIKeyCredentialsDto`, and `UIOAuthCredentialsDto`.
Both conversions gain the same ten owners. Three assignment DTOs have existing
context-dependent Lombok annotation and getter-qualification differences;
their final fields, constructors and default-instantiation helpers agree.
Both complete variants pass their respective full builds.

Jackson constructor defaults use the ordinary constructor's property binding
through a generated instantiator. Omitted defaults evaluate once, in declaration
order, while supplied values and explicit nullable nulls remain distinct.
Parameter deserializers, mapper naming, aliases, inherited contracts and
polymorphism stay attached to Jackson's standard binding. The constructor owns
body-property and init-block execution. Recursive DTOs reuse the incomplete
binding delegate per thread and deserialization context, including
ObjectReader prefetch before per-call attributes exist. The registry is cleared
after resolution. The former literal/enum
builder restriction is removed; ordinary lowerings establish default expressions.
Custom binding and generic DTOs retain their existing guards.

Constructor overloads evaluate successive defaults through the next larger
constructor, so dependent expressions never read absent arguments or run twice.
String concatenation selects the left receiver before right-hand collection or
custom-class operators. Cast types omit declaration-only nullability annotations;
the rejected candidate's full Gradle build exposed that incompatibility, which
now has native coverage using the older vendored JetBrains annotation API.

The baseline full Gradle build passes in 7m 51s;
the fresh full Gradle build passes in 4m 42s:
**110 actionable tasks: 55 executed, 55 from cache**. Hastings imports all **9,113 records** (50 + 9,063),
followed by **105.8 seconds** without errors or stack traces after
`Done loading dev resources [customers/hastings/hastings-dev.csv]`.
The disposable application is stopped after observation. No application source
or runtime configuration was hand-fixed.

All seven fixture checks pass with the frozen executable, including native
Kotlin-versus-Java comparisons in both Lombok modes. Coverage includes mutable
collection defaults, dependent defaults, nullable values,
required-value rejection, side effects, recursion, instance initialization,
custom parameter handling and constructor calls from retained Kotlin.
All four standalone samples compile. Formatting and default-target clippy pass.
The Rust sweep remains red: **1,571 pass, 73 fail,
137 ignored**, with no previously unseen failing names. The known
Windows vanished-source deletion race resurfaced in this sweep; its isolated
rerun passes. The other 72 failing names match the accepted baseline.

Evidence is under `target/defaults-frontier-v3-20261031/`, using
`notlin-defaults.exe`, `incremental-final`, and fresh `clean`. Its SHA256 is
`ac4ba6cc72d268fdd49c9df9b7d762d874d9deb9a8c0b5b7fb9ae93cdb105cf2`. `acceptance.json` records the frozen source,
input identities, ownership comparison, builds and runtime results. The rejected
annotation-cast candidate remains under `target/defaults-frontier-20261031/`;
the prefetch-context failure is under `target/defaults-frontier-v2-20261031/`.
Work continues on the retained frontier, including effective custom-deserializer
contracts and established map algebra.

## Previous checkpoint — 2026-10-30

The standard fresh conversion of reference commit `179bb912` produces
**3,944 Java files**, retaining 2,427 of 6,371 original Kotlin owners:
**+4 / -0** against the accepted 3,940-output baseline. The gains are
`UpperCaseStrategy`, `LookupIdentifierDto`, `LookupEntityDto`, and `DisplayAction`.
The baseline trial independently translates all three requested examples.

Safe calls share a selector-level null guard across established receiver types.
Expressions use a ternary; statements and assignments use a temporary and `if`.
Effectful receivers evaluate once, arguments stay lazy, nested safe calls compose,
and nullable Unit results and Elvis fallbacks preserve their behavior. Property
writes use established fields or setters, including plain Lombok setters.
Ordinary nullable-receiver extensions remain outside an earlier selector's guard.

Separator-only array and vararg joining uses `String.join` for proven string
elements and Kotlin's array operation otherwise, preserving ordering, default
separators, empty arrays, primitive arrays and null elements. Secondary
constructors retain Java varargs and reuse the ordinary established member
lowerings. Returned-reference string calls reuse ordinary override selection;
return facts follow inherited nullability without letting an unannotated override
erase an annotated ancestor. Checked exceptions and unproved contracts retain.
Imported Java static calls also use complete type identity, while local and
parameter bindings take precedence over classifier spelling.

The baseline full Gradle build passes: **110 tasks**, 80 executed and 30 from
cache. The fresh full Gradle build passes in 5m 43s: **110 actionable tasks: 80 executed, 30 from cache**.
Hastings imports all **9,113 records** (50 + 9,063), followed by
**105.8 seconds** without errors or stack traces after
`Done loading dev resources [customers/hastings/hastings-dev.csv]`.
The disposable application is stopped after observation. No application source
or runtime configuration was hand-fixed.

The frozen CLI passes Kotlin-versus-Java runtime comparisons in both Lombok
modes, including null propagation, evaluation counts, lazy arguments, property
writes, locale behavior, Unit results, foreign return types, inherited getter
chains and array formatting. All four standalone samples emit Java and compile;
`Nullspotting` now migrates. Formatting and default-target clippy pass.
The full Rust sweep remains red: **1,572 pass, 72 fail,
137 ignored**, with no new failing names against the recorded
baseline. The intermittent file-deletion race passes this run; it was not fixed.

Evidence is under `target/safe-frontier-20261030/`, using `notlin-safe.exe`,
`incremental-v3`, and fresh `clean-v2`. The executable SHA256 is
`b3c1969e3b654f334d5899b3f4d73d43871d28e25aa031f0b07a941f73fdc835`. `acceptance.json` records the frozen source and
input identities, coverage comparison, build, runtime, native and Rust results.
Rejected prototypes and the stale override-retention expectation are preserved
in their earlier logs; the final Rust comparison is `rust-comparison-final.json`.

## Previous checkpoint — 2026-10-30

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
