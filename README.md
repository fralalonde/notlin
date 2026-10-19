# notlin

Brutal Kotlin to Java converter.

## Usage

```sh
# Transpile a single file (Java written next to the input):
notlin src/main.kt

# Write output to a directory:
notlin -o build/java src/main.kt

# Untranslatable constructs: fail instead of warn
notlin --untranslatable=error src/main.kt

# Migrate in place: removes translated code from the .kt file,
# deletes the .kt entirely when everything translates
notlin --in-place src/main.kt

# Whole tree at once:
notlin -o build/java src/kotlin/

# Keep uncertain or lossy declarations in Kotlin (the default):
notlin --lombok --in-place .

# Explicitly permit diagnosed approximate transformations:
notlin --allow-approximations --in-place .

# Compile the staged mixed sources before applying migration:
notlin --validation-config validation.json --in-place .

# Save declaration coverage and structured retention reasons for this run:
notlin --lombok --in-place --coverage-report coverage.json .
```

The summary measures accepted Kotlin declarations, separately from Java output
files. A whole class (including its members) counts once; top-level functions and
properties count independently. Coverage partitions the original Kotlin input
of this invocation into translated and retained declarations, including outputs
accepted in earlier speculative passes. Existing Java from previous migrations
is excluded; this percentage does not claim cumulative project coverage.
`--coverage-report <path.json>` saves the same counts, accepted output paths,
stable declaration identities, structured retention reasons and recorded semantic
dependencies. Dependency information is currently partial. Reason category counts
can overlap because one retained declaration can have several blockers.
The default retention summary uses those exact original-owner counts and lists
per-reason owner counts. The older name-joined diagnostic-site graph is available
only at `-vv`; it is partial and does not predict translation unlock counts.

The semantic provider extracts declaration, inheritance, and companion facts in
one syntax parse per file. It resolves cross-file relations after assigning module
identities and indexing all files, without retaining the parsed trees.

Inherited getter checks compose type arguments along each inheritance path and
compare resolved declaration identities. Matching type-parameter names alone
are insufficient. Unknown, ambiguous, raw, cyclic, or incompatible paths keep
the declaration in Kotlin. A default method with a proven matching Java return
erasure can translate while its concrete descendants remain Kotlin.

Public no-argument method interfaces with supported scalar/reference return
signatures can also move to Java while descendants remain Kotlin. This uses the
same checked JVM boundary as getter-shaped methods; it does not apply to methods
with arguments, collection returns, or names requiring a Java spelling change.

A Java child interface can also extend a retained Kotlin interface with a proven
abstract method contract, even if an unsupported companion keeps that parent in
Kotlin. Parent default bodies require a separate JVM-default bridge plan: legacy
Kotlin compilation can otherwise call a missing Java-child `DefaultImpls` class.
Plain `super.method()` calls with no arguments resolve against indexed parent
identities, checking every inheritance branch. Unknown or competing targets
retain the caller. Java superclass calls use `super.method()`; interface defaults
use `Parent.super.method()` and still require a Java-visible implementation.
Parallel property contracts remain Kotlin when repairing them would break a
property implementation inherited from a retained Kotlin base class.
Parallel read-only properties with the same nonnullable Kotlin `String` or
primitive return type can share one getter on a retained implementation. The
planner requires matching resolved types and JVM descriptors across both
interfaces and the implementation; nullable, boxed, aliased, or conflicting
contracts do not use this rule.

Simple `if` expressions preserve smart casts of checked locals and `this`
within the proven branch, including the else branch of `!is`. Distinct nested
refinements remain Kotlin until their intersection type can be established.

Constructor defaults use the exact planned overload when any omitted default
needs the declaration's scope. Classes exposed through an indexed bounded
factory to Kotlin constructor reflection (`primaryConstructor` or `constructors`
with `callBy`) remain Kotlin: Java overloads do not preserve Kotlin constructor
metadata. This proof currently covers a direct `Class<T>` holder and an explicitly
typed factory returning that holder under a resolved bound. The loss can be
selected explicitly with `--allow-approximations`.

Enum `when` expressions can translate without approximation when the planner
proves a nonnullable enum subject, exact case identities, and an explicitly typed
function return context. The Java switch evaluates the subject once and preserves
Kotlin's unmatched-branch exception for exhaustive cases. Nullable subjects,
ambiguous names, and unknown wildcard imports retain the declaration.

Proven stdlib `TODO()` calls in throwing function bodies and typed expression
bodies preserve `kotlin.NotImplementedError` and its message. A custom `TODO`
declaration or unknown wildcard import keeps the caller in Kotlin. Known JDK
package wildcard imports can be accepted when scope checks exclude a shadow.

An explicitly typed nonnullable source reference can use the inherited
`Object.toString()` contract, including in secondary constructor arguments.
Its String result can support further String calls. Nullable or unresolved
receivers, arrays, value classes, and unconstrained type parameters need a
separate proof. Kotlin `Nothing` and `Nothing?` return contracts and callers
that depend on them remain Kotlin by default; Java `Void` cannot express that
contract.

Secondary constructors can delegate through the proven zero-argument
`java.util.UUID.randomUUID()` factory. The same source-scoped platform fact
checks imports and shadowing before admitting the call as a Java constructor
argument; a custom method with that name does not receive this allowance.

Secondary constructors can also delegate to another secondary constructor when
the planner proves a unique target by its complete positional argument shape.
The plan records the owning symbol, target declaration and source snapshot.
Constructor defaults, competing overloads, generic signatures, and reordered
named arguments need further resolution before they can use this path.
The planner reuses workspace builtin type proofs only when every contributing
source snapshot matches. Constructor parameters keep their lexical binding
when their names overlap record components or inherited properties.

Generic getter chains can use an explicit nonnullable upper bound to resolve the
next no-argument member call. The bound supplies member availability only: a
declared return type `T` remains `T`. Named generic bounds such as `Entity<I>`
preserve their arguments as evidence without substituting the bound's own
generic return types. The proof survives a Kotlin owner becoming
Java and rejects method type-parameter shadowing, aliases, unresolved ancestry,
and unsupported bound shapes. Exact qualified or explicitly imported
`Serializable` and `Cloneable` markers add no unknown callable members.

In-place migration can add an explicit `@kotlin.jvm.JvmWildcard` to a retained
Kotlin interface's read-only `List` element type when a proven narrower Java
implementation requires it. The parent keeps its Kotlin property and physical
getter. These snapshot-checked contract edits are accepted only together with
the translated implementation; unsuccessful attempts leave no annotation behind.

Receiver chains can follow explicitly typed Kotlin properties and physical public
Java `getX()` getters. Each type resolves in its declaring file, including across
method-return and nested-property chains. Nullable or ambiguous hops, hidden
Lombok members, and properties requiring unproven generic substitution remain
unresolved. Java getter lookup also excludes static/private methods, known
nullable getters, and visible fields that could change property resolution.

Resolved public Kotlin source methods with no arguments can retain their Java
call spelling even when their owner stays Kotlin. This requires an explicit,
supported return type and a stable JVM name. Method type parameters, visibility
or JVM-name changes, value-class signatures, aliases, and uncertain symbols do
not establish this fact. Methods with `@Throws`, including aliased forms, need
a separate Java exception-handling plan and do not receive this allowance.

Canonical `java.math.BigDecimal` receivers support proven single-argument
arithmetic/comparison calls and zero-argument normalization calls. Arithmetic
arguments must themselves have established decimal types; a matching method
name alone does not establish a JDK call.

Use `--advice` during workspace migration to report targeted source changes that may unlock more
translation. `U001` identifies competing inherited getter contracts on a retained
Kotlin implementation; `U002` identifies the actual constructor overloads that
collide after erasure, with resolved caller locations when available. Each
recommendation explains the constraint and a possible manual change. Parser
errors, unknown types and conservative secondary-constructor checks do not
produce source-change advice.

Use `--max-pass <count>` to cap workspace planning passes. The default remains the
automatic limit of twice the indexed Kotlin declaration count, with a minimum of
16. If planning does not converge within the limit, migration fails without
writing translated sources. The separate `--max-retention-passes` limit controls
internal ownership resolution within each workspace pass.

Recommendations are ranked by related retained declarations using resolved
symbol references, with at most ten shown per run. These counts are not promises
of successful translation: review the suggested edit, rerun Notlin, and validate
the mixed result. Locations refer to the input snapshots before migration edits.
The library exposes the complete structured report through
`translation_advice::analyze`.

Advice is disabled by default and does not change the translation plan. Add `-v`
to show elapsed time since launch on log and progress lines
(`-vv` adds file planning, diagnostic and skipping details; `-vvv` adds trace details). For example:

```sh
notlin --lombok --in-place --advice -v .
```

## Install

Linux/macOS (bash, zsh, fish):
```sh
# installs to `~/.local/bin/notlin`
curl -fsSL https://github.com/fralalonde/notlin/releases/latest/download/install.sh | sh
```

Windows PowerShell:
```powershell
# installs to %LOCALAPPDATA%\Programs\notlin\notlin.exe and (unless -NoPath)
# adds that dir to your user PATH via the registry — Windows has no default
irm https://github.com/fralalonde/notlin/releases/latest/download/install.ps1 | iex
```

From Source (Rust)
```
git clone git@github.com:fralalonde/notlin.git
cargo install --path .
```

## Status

In development

The Rust planner keeps ownership, bridges, caller repairs and checked edits
explicit before accepting structured Java output. See
[the partial translation architecture](docs/partial-transpiler-architecture.md)
for the analysis contract, validation configuration and JVM comparison tests.

## LICENSE

MIT
