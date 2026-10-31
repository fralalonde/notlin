# DEV.md — how notlin works and how it's tested

Developer documentation. For user-facing usage see README.md.

## What it is

notlin transpiles Kotlin source to Java (27+). `.kt` in, `.java` file(s) out.
Nullability is annotated (`@Nullable`/`@NotNull` from vendor/jetbrains-annotations.jar
by default), not enforced. Optional integrations on the receiving Java classpath
include Lombok and StreamEx; when Lombok is enabled, prefer emitting its annotations
instead of hand-rolling getters, setters, and statics.

Behavior contract:

- **Source migration.** Recompile callers; the application must compile and run
  the same after conversion. Kotlin-only constraints/ABI differences can be
  diagnosed; unchanged precompiled Kotlin compatibility is not the target.
- **Reference build gate.** Acceptance requires exporting the clean reference
  commit, running `notlin --in-place --lombok .` from its root, and passing the
  complete Gradle `build` on the resulting tree. Incremental translation trials
  are useful for iteration but do not establish that a full conversion builds.
  Do not hand-fix generated sources in the acceptance copy. Run Hastings dev
  startup after the build and inspect its runtime log for stack traces. Wait
  for `Done loading dev resources [customers/hastings/hastings-dev.csv]`;
  Spring's started marker and the first completed import do not prove that
  the complete development dataset loaded successfully.
- **Collections.** Prefer suitable immutable Java equivalents. If none fits,
  mutable fallbacks are allowed; mutability/read-only enforcement differences
  alone do not block conversion. Preserve values, relevant order, overloads,
  null/duplicate handling and evaluation effects. Keep explicitly mutable
  collections mutable.
- **Current work.** See `docs/translation-handoff.md` for accepted cases, exact
  verification provenance and parked constructor/empty-enum work.

- **Compiler-style diagnostics.** Parse errors → N003, untranslatable constructs
  → N001 (error or warn per `--untranslatable`), lossy/approximated translation
  → N002 warn. A transpilation with errors exits non-zero; warnings don't by
  default (`--deny-warnings` makes them fatal).
- **In-place migration.** `--in-place`: translated code is removed from the `.kt`
  file; a fully-translated `.kt` is deleted. This is how Kotlin is ground down
  file by file.
- **No silent drops.** Every construct either translates properly or produces a
  diagnostic. If you find output that differs from input semantics with no
  warning, that's a bug — file it with the probe file.

## Architecture

The run has five boundaries:

1. `workspace.rs` discovers and caches source snapshots and declaration identity.
2. `semantics.rs` indexes source facts and optionally shares one JVM helper
   runtime. Modules under `semantics/` prove particular language contracts;
   `facts/` owns the helper process, wire data, and hash-guarded observations.
3. `transpiler/fixpoint.rs` converges workspace ownership. `transpiler/mod.rs`
   builds each `TranslationPlan`; independent syntax lowerings share one walk,
   while eager collection chains use an inner-first pass.
4. `transpiler/unit/`, `expr/`, `stmt.rs`, and `types.rs` prepare Java candidates
   using the recorded plan. `planning.rs` checks source/receipt agreement and
   strictly parses candidates into `java_ir.rs` before accepting output.
5. `migration_pipeline.rs` stages repairs and replans; `migrate.rs` applies the
   accepted transaction. Optional `jvm_validation.rs` compiles the staged tree.

`main.rs` and `cli.rs` provide the CLI. `translation_plan.rs` holds decisions,
retention, and proof obligations; `coverage.rs` and `translation_advice.rs`
report them. See `docs/partial-transpiler-architecture.md` for proof conditions.

Tests live in `tests/`, language samples in `samples/`, and JVM helpers in
`jvm-facts/`. CI (`.github/workflows/check.yml`) runs formatting, clippy, Rust
suites, and `tools/e2e.sh`. Releases are separate; `release.sh` is user-reserved
and must never run from automation.

## Grammar notes (tree-sitter-kotlin-ng)

Things that have bitten us. Check here before guessing AST shape:

- `if` statement conditions live in the **`condition` field** (`kt::field(stmt, "condition")`);
  the parenthesized-expression sibling is NOT the condition.
- `= default` values and `parameter_modifiers` (`vararg`) are **SIBLINGS** of
  `parameter` inside `function_value_parameters`, not children.
- Extension receiver: a fieldless `user_type` node positioned before the `name`
  field on `function_declaration`. Return/param types are named user_types too —
  the index check against `name` is what disambiguates.
- Supertypes are wrapped in `delegation_specifier`: `constructor_invocation` →
  `extends`, bare type → `implements` heuristic.
- Class type params go **after the name** in Java (`class Gen<T>`), before it in
  the AST.
- Destructuring component types are unavailable: (`val (x, y): Pair<Int,Int>`)
  is a parse error in the grammar; components always emit `Object` + N002.
- Sealed classes need a pre-pass over the file: `permits` requires knowing the
  subclasses, and Java requires direct subclasses to be `final` (or
  `sealed`/`non-sealed`).

## Translation semantics

Prefer translation from language and JVM contracts over rules for individual
methods or annotations. Before adding a special case, identify the missing fact
(binding, type, dispatch, nullability, evaluation order, annotation target, or
generated ABI) and extend the shared semantic model. Lowerers consume that fact;
they must not independently recognize the same spelling and guess its meaning.
Unknown required facts continue to retain Kotlin.

Ordinary external calls use the shared signature applicability model in
`src/semantics/platform_members.rs`: canonical declaring type, dispatch,
parameter types and result classifier. Its bounded library declarations supply
classpath facts, not per-method translation algorithms. Constructor binding,
argument parsing, receiver evidence and chained results are shared. Expand this
declaration model when more library evidence is available; do not add another
`established_<method>_call` path. A signature alone does not prove nullability,
checked exceptions, Kotlin extension binding or evaluation semantics.

Annotation support should likewise describe resolved identity, use-site target
and its effect on the JVM contract. A new annotation name is not itself a new
translation rule. Library behavior that observes Kotlin metadata requires an
explicit capability proof; recognizing an annotation is insufficient.

- Java member-neutral metadata has a shared, source-bound annotation fact.
  Resolved Jackson and standard metadata can preserve declared getters and field
  absence; custom processors, conflicting/nested annotation types, unknown star
  packages and nullness defaults still block those proofs. Nullness is separate:
  Java platform and established non-null getter results permit an ordinary
  dereference; an explicitly nullable result requires a guard.
  Kotlin metadata/constructor requirements remain independent. Source packages
  are indexed, Java annotation types participate in binding, and the index cache
  advances to version 33. See `tests/java_annotation_effects.rs` and the native
  original-vs-converted fixture in `tests/java_metadata_jvm.rs`, plus the platform
  and nullable receiver matrix in `tests/property_receiver_scope.rs`.
- Explicit method overrides share the default-getter planner's source reference
  subtype proof. Retained Kotlin ancestors do not block ordinary reference
  covariance: javac supplies bridges for the wider JVM return. Generic return
  substitutions carry the source file where each concrete argument was bound;
  method type parameters cannot be mistaken for same-named source classifiers.
  Primitive/value-class representations and property ownership keep their
  separate checks. The index cache records method type parameters (version 32).
- Standard Map+Map operators and explicit `plus` calls share source-bound operand
  proof and lower to `kotlin.collections.MapsKt.plus`. An explicit Map argument
  selects the overload for collector expressions. Preserve key order, right-hand
  replacement, null entries, independent results and single operand evaluation.
  Java static fields carry declared types, including implicit interface statics;
  their reads do not depend on capitalization. Source properties outrank
  collection intrinsics with the same spelling. Covariant collection parameters
  assigned to typed locals keep their reference through an erased cast.
- Kotlin `==` is structural equality: emitted as `Objects.equals(a, b)` when an
  operand is a known non-primitive (via `var_types` tracking); `!=` likewise.
  Primitives keep `==`.
- Ordered comparisons on non-primitives → `a.compareTo(b) > 0` style.
- `Array<T>` → `T[]`; array receivers use `.length`, not `.size()`.
- Generic type args box primitives: `List<Int>` → `List<Integer>`.
- Extension functions → static methods with the receiver as first param
  (`__receiver__`); call sites `x.f(...)` rewrite to `f(x, ...)`.
- Delegated properties: `by lazy {}` → eager initializer + N002; other
  delegates → N001.
- Instance `lateinit var` properties use a backing field and explicit accessors.
  The getter reads the field once and throws `IllegalStateException` before
  initialization; the setter rejects null. Private member reads also use the
  checked getter. Static forms remain Kotlin until their callers use accessors.
- Annotation class literals resolve in the declaring file's scope. Explicit
  dependency imports support nested classes; import aliases become qualified
  Java names. Unknown names, ambiguous imports and unresolved Kotlin type
  aliases remain Kotlin rather than borrowing an unrelated source declaration.
- Enum companions share the ordinary companion member emitter. Maps and other
  supported initializers keep their declared types, and Companion getter/setter
  bridges forward to the same static storage. First-class enum companion values
  still require a separate object representation.
- A resolved ordinary source instance method takes precedence over a builtin spelling.
  The plan records the target and source span for ordinary positional calls;
  emission uses that identity instead of applying stdlib rewrites by name.
  Companion members keep their separate namespace rules.
- Secondary-constructor receiver method lookup includes indexed Java types and
  their inherited methods, using the same scoped type resolver as other calls.
- Visible top-level string constants can be inlined across files, including
  explicit, wildcard and aliased imports. Binding uses the consumer's scope;
  literal/type validation uses the declaration's snapshot. Shadowed reads,
  receiver lambdas and unsupported literal spellings remain conservative. The
  constant declaration can stay Kotlin while its Java consumers move.
- Safe calls lower at their selector boundary, before the ordinary property or
  method emitter. Expressions use a null-check ternary; effectful receivers bind
  once through a Java pattern variable. Statements and safe assignments use a
  temporary and `if (receiver != null)`. Arguments stay inside the guard, nested
  safe calls compose, and ordinary nullable-receiver extensions remain outside
  an earlier selector's guard. Unit-valued expressions preserve a nullable Unit
  result. These pattern bindings use Java 21+ syntax (CI uses JDK 25).
- Separator-only `joinToString` on array and vararg parameters uses `String.join`
  for proven string elements, or Kotlin's array operation otherwise. Both preserve
  the default `", "` separator and null elements. Transforms and extra formatting
  options remain outside this lowering.
- Secondary constructors retain Java varargs and reuse established standard
  member lowerings for delegation arguments. Getter chains use receiver syntax
  and scoped declarations, rather than a second interpretation of dotted text.
- Returned-reference string calls reuse ordinary override selection. Return
  facts follow inherited slots: an explicit non-null override can narrow the
  contract, while an unannotated Java override cannot erase an ancestor's
  nullability. Unproved annotations and checked exceptions remain conservative.
- Imported Java static methods resolve through the declaring type's complete
  identity. A same-named parameter or local value cannot become a static type
  qualifier. Guarded writes also recognize plain Lombok setters under an
  established default accessor configuration; access overrides and suppression
  remain conservative.
- Simple inherited default getter chains can belong to generic interfaces when
  the chain itself has concrete types. Java child compatibility uses the shared
  instantiated type graph to check every inherited getter against the child's
  own methods/properties and accepted bridges. Extra defaults with arguments,
  unresolved type substitutions and unsupported JVM shapes still retain Kotlin.
- Jackson-observed data classes can supply ordinary constructor defaults through
  a generated creator instantiator when the source set proves the Jackson API
  is available. Jackson binds the canonical constructor's parameters, including
  parameter annotations, aliases and mapper naming; the instantiator distinguishes
  omitted values from explicit nulls. Defaults run once in declaration order,
  then the ordinary constructor performs property and instance initialization.
  Recursive DTO resolution shares the incomplete delegate per thread and context;
  it also works during ObjectReader prefetch, before per-call attributes exist.
  Generic data classes and unproved custom creator binding remain Kotlin.
  Type-level custom deserializers own construction instead: direct and inherited
  `using` contracts allow ordinary constructors without automatic creator helpers.
  A nearer annotation resets inherited binding, including `JsonDeserializer.None`;
  parameter/field deserializers retain ordinary constructor-binding safeguards.
  Declaration nullability annotations are omitted from cast types for older jars.
- Memberless interfaces can forward abstract Kotlin getters/setters under declared
  modern JVM defaults, including generic and mutable properties. Concrete getters,
  legacy `DefaultImpls` modes and unproved compiler settings retain existing guards.
  Gradle configuration snapshots are verified before migration writes. Native
  fixtures compare default, legacy and interface-only modes with retained callers.
  Retained enums also receive explicit selections through the same first default
  branch used by Java enum emission. Inferred Kotlin returns preserve platform
  nullability, primitives and generic results; remaining Kotlin property contracts
  still prevent method bridges. Native cases reverse provider order.
- Constructor overloads delegate through successive default arguments, so a
  later default can read an earlier argument without re-evaluating its expression.
  String concatenation dispatch follows the left receiver even when the right
  operand is a custom class or collection.
- Kotlin stdlib member mapping (`kotlin_member_to_java` in expr.rs +
  property-read table): `uppercase`→`toUpperCase`, `keys`→`keySet()`, etc.
  Unmapped stdlib members warn N002 and pass through verbatim.

## Test pipeline

Three levels, cheapest first:

### 1. Rust test suites

```sh
cargo test
```

Library tests and integration suites cover semantic facts, retention, planning,
source/receipt agreement, Java emission, and migration. `e2e_samples` checks
that each sample transpiles; JVM tests requiring native compilers are ignored
by default. Shell-backed helper fixtures also require a working POSIX `sh`.

### 2. javac e2e gate (the real integration test)

```sh
tools/e2e.sh                  # default javac: ~/.local/java/jdk-27/bin/javac
tools/e2e.sh /path/to/javac   # or pass one
```

Transpiles every `samples/*.kt`, then **compiles the emitted Java** with
javac-27 against the vendored annotations jar. A sample passes only when the
`notlin` process exits 0 AND javac accepts its output. Output:
`OK <Sample>` per file, exit 0 overall. Anything red here that isn't red in
`cargo test` means the emitted Java is broken but the string checks passed.

### 3. Audit regression corpus

fixtures from the silent-drop audit live in `tests/fixtures/audit/`. They are
not wired into a runner; probe them by hand:

```sh
mkdir -p /tmp/probe && cp tests/fixtures/audit/probe13.kt /tmp/probe/
target/debug/notlin -o /tmp/probe /tmp/probe/probe13.kt
~/.local/java/jdk-27/bin/javac -cp vendor/jetbrains-annotations.jar /tmp/probe/*.java
```

Several probes deliberately contain untranslatables (suspend, coroutines,
destructuring): their expected result is a **warning, not a failure** — see
`docs/audit-2026-09-21.md` (C1–C16) for the expected outcome of each probe id.

### Full sweep before touching main

```sh
cargo fmt && cargo test && tools/e2e.sh
```

## Debugging tips

```sh
# Dump the tree-sitter AST — verify node kinds/fields before writing a match
target/debug/notlin --dump-ast probe.kt | less

# Zero RUSTFLAGS noise, fast incremental build
RUSTFLAGS='' cargo build

# See diagnostics in full detail
RUST_LOG=debug target/debug/notlin probe.kt 2>&1 | less
```

When a probe behaves wrong, reproduce it from a `/tmp` copy first
(`tests/fixtures/audit/*.kt` are tracked corpus — don't edit in place), verify
the AST shape with `--dump-ast`, then patch. `cargo fmt` before committing.
