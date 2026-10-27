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
