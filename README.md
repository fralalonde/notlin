# notlin

Kotlin to Java, ground down file by file.

notlin translates the Kotlin it can prove translates correctly and leaves the
rest Kotlin. The output is a mixed source tree that compiles and runs — a
partial migration, not an all-or-nothing sweep. Re-run it as the engine
improves or as you edit blockers away; coverage climbs, the Kotlin shrinks.

One engine makes every decision. A single file, a directory, or a whole repo
reach the same verdict: there is no "fast mode" that splits a Java child from
its Kotlin parent.

## Install

Linux/macOS (bash, zsh, fish):
```sh
# installs to `~/.local/bin/notlin`
curl -fsSL https://github.com/fralalonde/notlin/releases/latest/download/install.sh | sh
```

Windows PowerShell:
```powershell
# installs to %LOCALAPPDATA%\Programs\notlin\notlin.exe and adds it to your user PATH
irm https://github.com/fralalonde/notlin/releases/latest/download/install.ps1 | iex
```

From source: `cargo install --path .`

## Use

```sh
notlin src/main.kt                     # Java written next to the input
notlin -o build/java src/main.kt       # Java to a directory
notlin -o build/java src/kotlin/       # whole tree at once
notlin --in-place src/                 # migrate: remove translated code from .kt,
                                       # delete the file when fully translated
notlin --untranslatable=error file.kt  # fail instead of warn on untranslatables
notlin --lombok --in-place .           # keep uncertain declarations in Kotlin,
                                       # use Lombok on the Java side
notlin --allow-approximations --in-place .   # accept diagnosed lossy transforms
notlin --validation-config validation.json --in-place .  # compile the staged
                                       # mixed result before touching anything
notlin --coverage-report cov.json --lombok --in-place .  # structured report
notlin --advice --in-place .           # source edits that would unlock more
```

Diagnostics are compiler-style: N-codes for untranslatable constructs and
approximations, parse errors are N003. Errors exit non-zero; warnings do not
(`--deny-warnings` makes them fatal). Nothing is written unless the whole plan
is accepted — a failed run leaves every input untouched.

Coverage counts original Kotlin declarations for this invocation, not Java
files: a class (with its members) is one unit, top-level functions and
properties count individually. Earlier migrations' Java is excluded — the
percentage is not cumulative project coverage. `--coverage-report` saves the
same counts with accepted output paths, stable declaration identities, and
structured retention reasons; reason categories can overlap because one
declaration can have several blockers. Add `-v` for elapsed time on log lines,
`-vv` for planning detail, `-vvv` for traces. `--advice` prints ranked source
changes that would unlock more translation (`U001` competing inherited getter
contracts, `U002` constructor overloads colliding after erasure); counts are
hints, not promises — review, rerun, validate. `--max-pass` caps planning
iterations; non-convergence fails the run without writing.

## What holds declarations back, and why the design looks like this

The honest answer: **notlin has no Kotlin type checker.** The semantic provider
extracts declaration, inheritance, and companion facts from the syntax — one
parse per file, cross-file resolution by indexed identity — and grades every
fact established, inferred, ambiguous, or unknown. Required unknowns retain
their declaration. That single constraint explains everything else:

- **Ownership is a fixpoint, not a per-file choice.** Translating a class
  changes what its parents, children, and callers can do. The planner iterates
  ownership over the indexed workspace until it converges, emits Java only for
  the accepted set, and renders from strictly parsed Java syntax trees — an
  invalid generated file aborts migration before a write.
- **Hierarchy closure.** A retained Kotlin descendant cannot implement a
  translated-away parent's ABI (enum entries, `DefaultImpls`, KClass bridges);
  a declaration cannot move to Java ahead of a Kotlin parent that stays,
  except through proven getter/method-ABI bridges. Parents and children move
  together or stay together.
- **Uncertain declarations stay Kotlin.** `suspend`/coroutines have no Java
  lowering and are retained outright. `value class`, `Nothing`, Kotlin
  constructor reflection (`callBy`), Jackson missing-vs-null, boxed numeric
  identity all retain by default. Each retention records a structured reason.
- **Approximations are opt-in.** What Java can express with a semantic
  difference — elvis as a ternary, `by lazy` as eager init, `when` as switch —
  is diagnosed (N002) and retained unless you pass `--allow-approximations`.
- **Repairs are proven before they are trusted.** Default-getter bridges,
  caller rewrites, and `@JvmWildcard` contract edits apply to virtual
  snapshots; only the converged plan commits. Edits carry source hashes; a
  stale snapshot cannot authorize a write at application time.
- **The compiler is the gate.** `--validation-config` stages the planned mixed
  tree, runs kotlinc then javac against it; failure aborts before any original
  file is rewritten.

So expect: high coverage on data/model/service shapes, interfaces and classes
that translate as families, and an exact list of every declaration that didn't
move with the reason it stayed. Don't expect: coroutine code, `inline value
class` wrappers, or inferred types — those need the JVM-compiler analysis
stage the architecture reserves a serializable contract for.

## Verification

Three gates, cheapest first: `cargo test` (200+ suites), `tools/e2e.sh`
(transpile every sample, javac the result — the contract is *emitted Java
compiles*), and the JVM comparison harness (`cargo test --test jvm_validation
-- --ignored`), which runs original Kotlin against mixed Kotlin/Java execution
with real compilers. `DEV.md` has the environment variables and corpus layout.

## Docs

- `docs/partial-transpiler-architecture.md` — the analysis contract, identity
  model, ownership fixpoint, and each rule's exact proof conditions.
- `DEV.md` — how the engine works, grammar pitfalls, test pipeline.

## Status

In development. JDK 25 is the CI baseline (27 locally). Lombok, commons-lang3
and StreamEx may live on the receiving classpath; notlin uses them.

## LICENSE

MIT
