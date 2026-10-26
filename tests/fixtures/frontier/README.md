# Translation-frontier native characterization

These generic fixtures characterize capability boundaries; they do not assert that every emitted candidate is correct. No private reference-project identifiers, source or dependencies are included.

## Run

Provide absolute paths for a verified notlin executable, JDK 25+, Kotlin 2.4 compiler and its standard library. `TMPDIR` must name an owned scratch directory **outside the repository**.

```sh
python -B tests/fixtures/frontier/reproduce.py \
  --notlin "$NOTLIN" \
  --java-home "$JAVA_HOME" \
  --kotlinc "$KOTLINC" \
  --stdlib "$KOTLIN_STDLIB"
```

To reproduce the fact-enabled constructor boundary, additionally pass:

```sh
  --full-helper "$FULL_HELPER"
```

`FULL_HELPER` is the full facts jar with its manifest-relative dependency layout intact. This is an additional argument to the command above, not a standalone shell command.

The runner creates and prints a fresh evidence directory under `TMPDIR`. It writes compiler output, runtime output, full `javap -p -s -v` inventories, coverage reports and `summary.json`. It does not rebuild notlin or the helper, change the fixtures, touch the private corpus, or delete previous evidence.

A successful runner exit means the original controls compiled/ran and characterization completed. **Candidate compilation failures, runtime failures and differing outputs are recorded evidence, not passing correctness gates.** Check `summary.json` and the corresponding logs.

## Generic provider

`generic_owner_fixed_return/` contains Marker, Wide, a retained generic Provider, Subject and an original Java caller. The original is compiled under `-jvm-default=disable`, `enable`, and `no-compatibility`.

The isolated accepted baseline translates Wide and retains the other three declarations. Subject is blocked by the retained Provider; this fixture does not reach a conflicting covariant-getter forwarding proof. Original Subject's declared read() stub differs by default mode, so a Java method body alone is not physical ABI evidence.

An early exploratory run selected four inputs but indexed 41 Kotlin files from surrounding variants; its Subject emission was **not an isolated result**. The runner now sets cwd to the exact source root, keeps all variants/output trees elsewhere, checks indexed Kotlin/Java file totals, and requires candidate class files before constructing any classpath containing original classes.

The workload-derived fixed-return provider also has additional generic ancestor and getter-metadata obligations. Do not simplify those away or remove the generic-owner refusal based on this smaller control.

## Secondary constructors and mapped collection ABI

`secondary_constructor/src/` is the original Kotlin Bag with no primary constructor and three secondary constructors: empty, collection copy and vararg copy. Element and the empty Marker interface are already Java. JavaDriver checks null-container versus null-element behavior, element identity/order and exactly-once collection copying.

On the accepted baseline:

- Helper off: Bag remains Kotlin for unknown addAll/toList call facts.
- Full helper: all three sites resolve; Bag remains Kotlin for the assignment-only constructor-body rule (NE5A7). More resolution is not a lowering contract.
- `nobody/Bag.kt` is an independently compiled control with empty constructor bodies. Its emitted Java misclassifies ArrayList as an interface and collapses the vararg formal to a scalar; javac rejects it.
- `manual/Bag.java` is explicitly a **manual reference, not emitter output**. It passes the same 19 behavioral checks, yet omits 12 Kotlin mapped-collection specialization members from this fixture's original class.
- `caller/MappedCaller.java` is compiled against the original Kotlin ABI. It prints `1` with the original, but fails with `NoSuchMethodError(getSize)` against the manual Java class. This demonstrates a real unchanged-caller failure, not only a reflection difference.

Do not infer that every collection subclass has exactly these 12 methods. Inventory the concrete owner, element type, compiler mode, flags, generic signatures and inherited members. A body-argument or vararg correction alone does not make this owner eligible.

## Follow-on work

1. Preserve the complete physical owner contract before widening inherited/default-method or collection-constructor support.
2. Use resolved source/external class kind for bare supertypes; never guess `implements`. Treat a standalone correction as safety work, not a measured translation gain, and preserve the existing compatibility policy.
3. Investigate bodyless marker annotation declarations separately: they avoid constructor and generated-bridge obligations but require explicit target, retention, meta-annotation and Kotlin-metadata handling. That probe is a separate scope, not a conclusion of this runner.

Every claimed workload improvement still requires a fresh normalized gained/lost comparison against 20/2,683, zero losses, native correctness checks and a passing reference build on one frozen manifest.
