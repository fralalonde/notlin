# Partial translation architecture

The Rust pipeline prepares a complete migration before application:

**snapshots → semantic facts → compatibility preparation → ownership plan → accepted Java syntax trees → rendering → optional mixed compilation → migration**

Classes are ownership boundaries. Top-level functions and properties are independently selectable. Rejected declarations remain Kotlin; no method extraction into helper classes is performed.

## Identity and facts

`SymbolId` contains module, declaring file, package, enclosing declarations, kind, name, receiver and normalized parameter types. Bodies, whitespace and snapshot locations are separate. `DeclarationId` and `SourceLocation` carry source hashes and byte ranges; stale snapshots cannot authorize edits. Generated symbols have explicit mappings to their origins.

`SemanticProvider` supplies structured Kotlin types and explicit confidence: established, inferred, ambiguous or unknown. The syntax provider uses source declarations and the existing workspace facts. It is not Kotlin compiler analysis. Required unknown facts and ambiguous bindings retain their owner. The serializable version-1 analysis contract describes sources, module configuration, symbols, types, references, JVM signatures, diagnostics and capabilities for a subsequent JVM provider.

Ownership fixpoint sets contain symbols. Resolved inheritance edges determine retention; names remain diagnostic labels and conservative fallback information for unresolved references. When ownership repeats in a cycle, the planner retains every declaration whose membership varies across the complete cycle and records an `OwnershipCycle` reason. It then replans those declarations and their dependants; stable unrelated declarations remain eligible. Bounded passes still reject nonconvergence. Speculative source repairs operate on virtual snapshots.

Within one snapshot, retention queries populate symbol identities once per file.
Ownership preparation records both positive and negative retention queries;
later passes reuse a file only when none of those answers changed. A new
speculative snapshot starts with fresh preparation caches and recomputes
ownership from current facts. Previous retained owners only prioritize work;
seeding ownership with them can preserve a getter cycle after its blocker has
disappeared. Property ABI repair
operations preserve their caller contracts across those snapshots, even after
the original property has become an accessor method.

Map factory contexts can use uniquely bound, non-generic source aliases of
Kotlin `Map` and `MutableMap`. The provider follows each alias in its declaring
file's scope and rejects ambiguous, projected, cyclic or unproven bindings.
Supported built-in arguments become fully qualified boxed Java types, so the
consumer's imports cannot rebind them. Signature lowering rewrites Java type
uses; an alias spelling inside a literal or a member name is not a type use.

Pure getter-contract queries are memoized separately, including empty results
and failures. Their cache keys are unique symbol identities. Scoped read tracking
records consulted files and name lookups, including missing names and cached
subqueries. An overlay preserves an entry only when those dependencies and its
owner identity remain valid. Ownership and retention verdicts are never stored
in this cache. Declaration, alias and member namespace changes are counted
separately, so body edits do not invalidate unrelated negative lookups and
same-name additions cannot cancel removals in another lookup category. Each
snapshot is rebuilt from the original overlay base, then seeded with compatible
facts from the latest analysed snapshot. Read tracking is thread-local; getter
traversals stay on the calling thread so every consulted dependency is captured.
`NOTLIN_PROFILE=1` reports traversal time, cache hits and misses,
and overlay reuse and invalidation alongside the other planning phase timings.

The indexing parse also records method-generic headers and inline type-parameter
bounds. Java owners retain a single nominal bound or the implicit Object bound;
annotated, generic and intersection bounds remain unproven. Getter checks read
these snapshot facts instead of reparsing complete
files. They survive index serialization and are rebuilt for replaced source
snapshots; ambiguous owner names and unsupported signatures remain unproven.

Before application, snapshot verification checks every consulted file against
disk, then uses an indexed snapshot map and one digest per edited file to
validate edit hashes, ranges and overlaps. Alias paths resolve to the same file
identity. This avoids repeated filesystem lookups for each declaration edit.

## Planning and emission

`TranslationPlan` records declaration ownership, structured retention reasons, dependency provenance, bridges, caller repairs, generated outputs and checked source edits. Compatibility preparation collects eligibility decisions before acceptance. Semantic loss and unresolved approximation assumptions retain declarations by default. `--allow-approximations` explicitly enables approximate transformations; informational differences do not require this option.

The preparation lowerers currently use text adapters for candidate syntax. `AcceptedTranslationPlan` strictly parses those candidates into Java syntax trees with declaration, type, expression, statement, annotation and import categories. Accepted output contains tokens and structured children, with no raw expression or statement fragment variant. Calls and member accesses can carry target identities. Node constructors group expressions by precedence and escape Java literals. The final emitter only renders accepted nodes and never changes ownership. Invalid generated syntax aborts migration before writes.

The library owns workspace speculation and migration proposals. It preserves ownership decisions from earlier rounds even when the original Kotlin declaration has already disappeared from a virtual snapshot. Typed accessor contracts recorded by property repairs also survive later rounds, with generic substitution along inheritance edges and exact, unambiguous owner matching. Caller repairs distinguish these Kotlin accessor contracts from ordinary Java properties so existing smart-cast bindings remain valid. Selected files do not imply Java ownership: a retained Kotlin property ancestor can require retention of a covariant child. Repairs also preserve property overrides when the Java getter inherits that property from a Kotlin ancestor on the same path. Application checks the consulted inputs and output destinations against the original snapshots.

Default getter planning substitutes structured type arguments along each inheritance
path and compares canonical declaration identities. It checks the declaration's JVM
erasure separately: `Provider<T>.getId(): T` instantiated with `StoredId` still
erases to `Object`, unlike a getter declared to return `StoredId`. A concrete
accessor can therefore be required even when the instantiated Kotlin returns agree.
Accepted bridges carry a structured return type, the selected direct provider and
the originating declaration. Retained Kotlin classes receive an explicit delegating
method only when the participating contracts permit a Kotlin method override. A
retained Kotlin property contract instead keeps its provider Kotlin. These repairs
use the same snapshot checks and origin mappings as other migration edits; unknown
or ambiguous hierarchy facts do not authorize a bridge.

A Kotlin method returning an owner type parameter can prove covariance through
one explicit nonnullable reference bound. The generic signature remains intact;
its bound is not substituted into emitted return types. Concrete descendants
still require compatible logical returns and independently checked JVM erasures.
Java bound facts keep those bridge decisions available after a provider moves
to Java in an earlier speculative round.

A retained Kotlin interface may serve as the direct super-call qualifier when
it inherits the exact accepted Java default. The declaring default must move to
Java, while the qualifier may remain Kotlin for unrelated unsupported methods.
Overrides, reabstraction and ambiguous default routes still fail the hierarchy
proof; retained Kotlin property contracts still block a method-shaped repair.

Eligibility and concrete bridge planning share the same getter-free external
contract facts: `Comparable<T>` with one argument, and the supported marker
interfaces. Explicit import aliases are accepted only with a unique binding;
source collisions, foreign contracts and raw `Comparable` remain unproven.

Untyped getters that delegate directly through a declared generic receiver bound
share the property ABI pass's exact return-type proof. An inherited override
contract alone is insufficient because Kotlin can infer a narrower return from
the getter body. Proven inferred returns participate in both logical return
comparison and JVM erasure checks, including their Kotlin property ownership.
The proof preserves nullability and lexical type parameters. Positive and
negative inferred returns are memoized within their source snapshot; cache hits
replay their dependencies into any enclosing bridge query.

## Exact eager collection operations

Proven Kotlin `Iterable.map` and `filterNot` calls carry snapshot locations,
ownership identities, and available element types into the accepted plan.
Planning assigns private helper identities and collision-free Java names;
acceptance inserts their loop bodies as structured Java declarations. Each
operation materializes a mutable `ArrayList` before a subsequent stage starts.
`map` reads a collection's size before creating its iterator, while `filterNot`
does not. Neither operation uses stream fusion or rejects null result elements.
Primitive element bindings preserve Kotlin's unboxing inside the lambda.

Direct `map { it.property }` projections carry the resolved Kotlin property,
ordinary getter ABI, and structured result type. Planning allocates a unique
lambda parameter so acceptance can attach the same property identity to the
Java getter invocation. Nullable results can feed subsequent eager stages.
Generic getter results require an exact expected element type; Kotlin's nested
collection covariance cannot authorize a Java generic widening implicitly.
Nullable or generic element receivers, record component accessors, and
value-class getter results require additional proofs and remain unplanned.
For a final ordinary Kotlin class, a field or renamed accessor may instead use
an explicit public `getX()` whose entire body reads that property. The provider
records this equivalence during the existing parse and verifies both signatures
and snapshots. The plan depends on the property and method separately; accepted
Java invocations retain the method target. Annotated or open owners, uncertain
annotation bindings, and differing method bodies cannot grant this capability.

Physical Java synthetic properties have a separate snapshot-scoped read
contract: the exact public no-argument method, qualified result type,
reference nullability, JVM primitive status, and dependency identities. Java
platform references remain potentially null; a boxed numeric result does not
inherit a primitive return's guarantees. Stream/Optional and direct eager-map
projections consume this contract after an element class has become Java.
Null results, getter side effects, unchecked failures, primitive floating-point
comparisons, and boxed integer comparisons are covered by JVM comparisons.
Numeric referential comparisons require a separate boxing/identity plan and
remain Kotlin, including boxed getter results and primitive SAM parameters.
Inherited Java getters use the complete source-backed hierarchy. Selection
deduplicates a shared diamond declaration and accepts a unique overriding
declaration only when its return and nullability contracts are compatible.
Covariant ordinary reference returns require a proven subtype path. The read
keeps the selected declaring method's identity, source snapshot and annotation
scope, plus dependencies on the receiver and consulted ancestors. Argument
overloads cannot hide an inherited no-argument getter. Parent Lombok getters
use the same selection and preserve their original generated identities.
Parse-time uncertainty facts prevent an annotated descendant with no proven
getter contract from falling back to an ancestor's contract. This also covers
unproven Lombok profiles, configured accessor prefixes and boolean getters.
Visible field collisions, uncertain annotations/defaults, checked throws,
generic substitution, cycles and unrelated competing declarations require
further facts.
Declared getters on recognized Lombok owners use the same physical-method
contracts. Accepted Lombok outputs also carry generated-getter contracts with
the originating property identity, actual JVM signature, structured result type
and exact Java source hash. Later planning rounds consume these contracts only
with `--lombok` and a snapshot-checked proof that no ancestor `lombok.config`
overrides the default accessor rules. Changed or removed outputs invalidate
their contracts. Existing configuration, generic owners, accessor overrides,
ambiguous bindings and unsupported field types leave these facts unknown.
Existing Java inputs can supply the same narrow getter contracts when
`--lombok` and the snapshot-checked default configuration establish the
processor profile. Their generated getter identities derive from the exact
Java field, independently of similarly named Kotlin properties. Accepted
outputs retain their original Kotlin lineage instead of acquiring a second
source-derived identity. Changed snapshots discard these facts; annotations
alone never establish the active processor profile. Retained Kotlin-origin
metadata currently lasts for one migration invocation.

Unknown callable binding, nullable receivers, extension shadows, unsupported
lambda bodies, and modified local captures cannot authorize this lowering.
Container lambdas can carry bounded property chains. Each hop records its
snapshot location, structured receiver and result types, exact getter target,
and ordered getter path from the allocated lambda parameter. Emission follows
the planned receiver tree without caching or duplicating getter evaluations.
Accepted Java invocations retain targets by their complete getter path, so
identical getter names reached through different properties remain distinct.
Unknown hops, explicitly nullable intermediate receivers and unsupported
receiver expressions retain Kotlin. Java platform receivers preserve normal
dereference failures. Rejection cannot fall back to a broader legacy Stream
member rule.
Property result types resolve in their declaring file before crossing into a
caller. Context checks compare canonical types and retain the return-type and
inheritance identities used to accept a flat upcast. Reified bounds and mapped
collection results share the same bounded inheritance-path proof. Nested
generic results still require an exact contextual element type.
Nullable floating-point equality also stays Kotlin until the expression
lowering preserves IEEE signed-zero and NaN behavior.
Those declarations retain Kotlin unless approximation mode separately permits
the legacy lowering. Lazy Stream and Optional calls have independent facts.
Their platform results preserve Kotlin null checks at explicit result and
argument boundaries, including uniquely selected Kotlin constructor parameters.
Java parameter enhancement and uncertain constructor overloads require richer
facts and remain Kotlin.

Stream and Optional facts also carry bounded structured payloads from explicit
collection parameters, properties, and resolved nongeneric Kotlin return
signatures. Each supported lambda has its own snapshot-bound parameter, body,
and resolved property accesses. Bindings cannot escape into nested lambdas or
fall back to an outer parameter with the same spelling. Emission consumes the
allocated Java names and restores the enclosing scope afterward. Payload aliases,
variant arguments, nullable elements, and unsupported scopes remain unresolved.
Proven primitive equality explicitly unboxes operands at the comparison: a
boxed Java SAM parameter must not turn Kotlin numeric equality into reference
identity. Unused parameters remain boxed, preserving constant-lambda behavior
on collections polluted with nulls.
Generic property projections require an exact expected container payload to
avoid implicit nested generic widening. A proven `Optional<String>.orElse`
with a non-null String literal fallback does not need a surrounding parameter
nullability contract; this independent proof does not authorize null fallbacks
or infer unknown Java signatures.

## Jackson constructor defaults: proposed lowering

Defaulted constructors with observable Jackson contracts remain Kotlin. A Java
creator receiving `null` cannot by itself tell whether the property was omitted.
Replacing both cases with a default changes nullable properties and can hide
invalid explicit nulls. Constructor overloads and Lombok do not supply that
missing presence information.

A future lowering must first record an ordered constructor contract: stable
owner and parameter identities, structured types and nullability, defaults and
their dependencies on earlier parameters, and resolved annotation targets with
inheritance provenance. An accepted creator plan must specify each parameter's
missing, supplied and explicit-null behavior, default evaluation order, and the
source snapshots authorizing the generated bridge. The existing broad inherited
annotation check is a retention signal, not proof of Jackson's annotation
precedence.

The first candidate is a concrete class with one known primary constructor,
simple supported defaults, and no inheritance, polymorphism, custom creators or
property customization. An explicit deserialization bridge could preserve field
presence, convert supplied values, evaluate missing defaults in parameter order,
and call the all-arguments constructor while keeping fields final. This remains
a design candidate: mapper null/coercion policies, unknown and duplicate fields,
contextual type conversion, error behavior and existing deserializer conflicts
must be proven before a class becomes eligible. Buffering an object tree must
not silently discard observable parser behavior. No mapper configuration or
module registration changes are implicit in translation.

The execution gate compares original Kotlin with Java for `readValue` and
`convertValue`: missing values, supplied values, explicit nulls, dependent
defaults, evaluation order and final fields, with and without Lombok. Inherited
annotations and polymorphic reads remain separate later slices. Until those
contracts are supported, the current retention and mixed-language JVM tests
remain the required fallback.

## Configured compilation gate

Pass `--validation-config path/to/validation.json` to require compilation of the complete planned Kotlin/Java result before writing original files or Java outputs. Validation preserves source filenames in a fresh staged workspace, runs Kotlin with Java sources for resolution, then Java against Kotlin classes. Compiler failure leaves originals untouched and reports compiler diagnostics. Ordinary migration never runs application entry points.

The version-1 JSON configuration uses paths relative to the config file:

```json
{
  "version": 1,
  "kotlinc": "tools/kotlinc",
  "javac": "tools/javac",
  "java": "tools/java",
  "kotlin_sources": ["src/Remaining.kt"],
  "java_sources": ["src/Support.java"],
  "classpath": ["lib/kotlin-stdlib.jar"],
  "kotlinc_args": ["-jvm-target", "17"],
  "javac_args": ["--release", "17"]
}
```

Additional source inputs provide module dependencies outside the selected migration. Configured source lists supplement the indexed workspace; planned replacements take precedence over on-disk originals. Output/classpath overrides in compiler arguments are rejected. Without a configuration, compilation remains optional. Project discovery and Gradle integration are outside this interface.

## Verification

The Rust tests cover identity stability, overloads and ambiguity, structured types, safe-default retention, accepted Java nodes, snapshot rejection, output conflicts and validation failures. The runtime harness compares original Kotlin execution with mixed Kotlin/Java execution; it is separate from CLI migration.

Set `NOTLIN_KOTLINC`, `NOTLIN_JAVAC`, `NOTLIN_JAVA`, `NOTLIN_KOTLIN_STDLIB`, `NOTLIN_LOMBOK` (the Lombok jar path), and `NOTLIN_KOTLIN_LOMBOK_PLUGIN` (the matching Kotlin Lombok compiler plugin). The Lombok comparisons use the same Kotlin plugin and Java annotation processing as a mixed Lombok module. Then run:

```text
cargo test --test jvm_validation -- --ignored
```

On Windows, sample Java compilation uses explicit approximation mode:

```powershell
./tools/e2e.ps1 -Javac C:/path/to/jdk/bin/javac.exe
```

Full Kotlin resolution, compiler-plugin semantics and authoritative JVM ABI facts remain the subsequent JVM-analysis stage. The syntax provider reports its capabilities and uncertainty rather than claiming that fidelity.
