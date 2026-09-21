# DEV.md — how notlin works and how it's tested

Developer documentation. For user-facing usage see README.md.

## What it is

notlin transpiles Kotlin source to Java (27+). `.kt` in, `.java` file(s) out.
Nullability is annotated (`@Nullable`/`@NotNull` from vendor/jetbrains-annotations.jar
by default), not enforced. Assumed on the receiving Java classpath: Lombok,
commons-lang3, StreamEx — when hand-rolling getters/setters/statics, prefer
emitting Lombok annotations instead.

Behavior contract:

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

```
src/
  main.rs       CLI plumbing (clap) + in-place semantics
  cli.rs        arg definitions
  lib.rs        crate root
  diagnostics.rs  N001/N002/N003 compiler diagnostics (spans, colors, counts)
  transpiler/
    mod.rs      RunEngine: transpile() — parse, iterate top-level nodes, emit
    unit.rs     classes/functions/properties: signatures, params, type params,
                receivers, sealed pre-pass, interface default bodies (~1200 lines)
    stmt.rs     statements: if/while/for/try, single-statement bodies
    expr.rs     expressions: operators, calls, navigation/member rewrite,
                extension call-site rewriting, type inference (~900 lines)
    types.rs    type mapping tables (map_type_name, boxed_name)
    kt.rs       tree-sitter helpers + java_type()
    java.rs     JavaOut: output buffer/emit helpers
vendor/
  jetbrains-annotations.jar   nullability annotations (vendored: /tmp is volatile)
samples/        source corpus; every file must transpile AND javac-compile
tests/          Rust integration tests (assert transpile output + diagnostics)
tests/fixtures/audit/*.kt   probe corpus from the silent-drop audit (regression)
tools/e2e.sh    the integration gate (see below)
.github/workflows/check.yml    CI: fmt, clippy, test, e2e gate
.github/workflows/release.yml  tag-triggered binary matrix + GitHub release
release.sh      version bump/tag (user-reserved; never run from automation)
```

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

5 suites / 15 tests: e2e_samples asserts every `samples/*.kt` transpiles with
zero errors, non-empty output, balanced braces; other suites cover
diagnostics, type mapping, in-place behavior.

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
