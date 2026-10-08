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
speculative snapshot starts with fresh preparation caches. Property ABI repair
operations preserve their caller contracts across those snapshots, even after
the original property has become an accessor method.

Before application, snapshot verification checks every consulted file against
disk, then uses an indexed snapshot map and one digest per edited file to
validate edit hashes, ranges and overlaps. Alias paths resolve to the same file
identity. This avoids repeated filesystem lookups for each declaration edit.

## Planning and emission

`TranslationPlan` records declaration ownership, structured retention reasons, dependency provenance, bridges, caller repairs, generated outputs and checked source edits. Compatibility preparation collects eligibility decisions before acceptance. Semantic loss and unresolved approximation assumptions retain declarations by default. `--allow-approximations` explicitly enables approximate transformations; informational differences do not require this option.

The preparation lowerers currently use text adapters for candidate syntax. `AcceptedTranslationPlan` strictly parses those candidates into Java syntax trees with declaration, type, expression, statement, annotation and import categories. Accepted output contains tokens and structured children, with no raw expression or statement fragment variant. Calls and member accesses can carry target identities. Node constructors group expressions by precedence and escape Java literals. The final emitter only renders accepted nodes and never changes ownership. Invalid generated syntax aborts migration before writes.

The library owns workspace speculation and migration proposals. It preserves ownership decisions from earlier rounds even when the original Kotlin declaration has already disappeared from a virtual snapshot. Typed accessor contracts recorded by property repairs also survive later rounds, with generic substitution along inheritance edges and exact, unambiguous owner matching. Caller repairs distinguish these Kotlin accessor contracts from ordinary Java properties so existing smart-cast bindings remain valid. Selected files do not imply Java ownership: a retained Kotlin property ancestor can require retention of a covariant child. Repairs also preserve property overrides when the Java getter inherits that property from a Kotlin ancestor on the same path. Application checks the consulted inputs and output destinations against the original snapshots.

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
