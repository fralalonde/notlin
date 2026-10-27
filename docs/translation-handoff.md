# Translation handoff — 2026-10-26

## Resume here

The requested #5 slice is finished and merged into `main`. Stop this delivery
cycle here; do not automatically reopen the rejected fact infrastructure or start
#2/#6. Those are the next capability choices for a subsequent session.

- Feature: `dab4e891647dc9c6358ce5cdf9ce73ffa8b0ca2b`.
- Main integration: `731065bd74b345bda6000848ac98f90deb9e8356`.
- Accepted getter-binding repair: `3e0e4e27bb42e674c58733edafaac0ba913c956e`.
- Main integration also includes the earlier verified branch history, not just
  the two-file enum feature. Merge parents are the previous main `8a5729e5` and
  the feature commit. No conflicts were resolved.
- The integrated tree differs from the tested feature only in the existing
  main package version: Cargo.toml/Cargo.lock retain **0.5.0** instead of 0.4.4.
  No release script, version bump or tag operation was performed.

## Acceptance policy

This is **source migration**: recompile callers. The application must compile and
run the same after conversion. Preserve values, relevant ordering, overload
selection, evaluation effects and working source callers. Diagnose applicable
Kotlin-only API/constraint changes; do not promise unchanged precompiled Kotlin
binary compatibility.

For all collection conversions, prefer a suitable Java immutable equivalent.
When no suitable immutable equivalent exists, a mutable fallback is allowed.
Mutability/read-only enforcement differences alone are not landing blockers.
This does not waive changed values, null/duplicate handling or relevant order.
Keep explicitly mutable collections mutable.

## Cases 1, 2, 5 and 6

| Case | Status | Boundary / next step |
|---|---|---|
| 1 — coordinated properties/getters | Accepted scoped repair, integrated | Canonical receiver identity and retained-getter repair. Not universal hierarchy/default-inheritance support. Wider candidates lost non-null entry checks; do not re-enable them without the actual contract. |
| 2 — constructor overloads / Jackson defaults | Parked, not accepted | Generic native defaults prototype exists, but qualified/nested aliases emitted invalid Java. Fix canonical constructor-slot type rendering and verify real Jackson omitted/supplied/explicit-null behavior before admission. Jackson 3 is an option, not an applied upgrade. |
| 5 — enum companion groups | Finished bounded slice, integrated | Ordinary Java statics with declared Set/Collection type, shared ordered contents, singleton Set.of, existing Java companion-getter source compatibility. Actual first-class companion-object consumers retain Kotlin. |
| 6 — zero-constant enums | Parked, not accepted | No dummy constants. Need whole-owner physical/inferred type correctness, including an enum named String capturing an inferred kotlin.String accessor return. Canonical annotation/type probes alone are not admission. |

### What #5 actually supports

The bounded lowering recognizes literal owning-enum-entry groups initialized by
`setOf` in an eligible companion. It emits a `public static final` field and an
accessor. A singleton uses `Set.<EnumType>of`; multi-entry groups use an ordered,
deduplicating `LinkedHashSet` representation. Declared `Collection` types must
not be narrowed to `Set`: that can change Java overload selection.

A nested **static getter namespace** supports existing Java source such as
`Kind.Companion.getGROUP()`. It is not a fabricated companion singleton and does
not establish full companion-object ABI or reflection equivalence. Unsupported
object-value consumers retain the enum and affected Kotlin consumers.

The implementation is in `src/transpiler/unit/class.rs`; the eight generic
regressions are in `tests/enum_companion_group.rs`. No reference-project
identifiers or sources were added to those tests.

## Measured acceptance

Same immutable input and helper on both sides:

| Measure | Accepted getter-binding baseline | #5 feature |
|---|---:|---:|
| Source inputs | 6,444 | 6,444 |
| Normalized original owners | 2,681 | 2,681 |
| Translated owners | 20 | 21 |
| Gained / lost | — | **+1 / -0** |
| Migration wall seconds | 272.358 | 263.275 |

The full migrated reference Gradle build passed: **106 actionable tasks, all
106 executed**, without cached/up-to-date main compilation. Gradle wall time was
145.423 seconds. Original source bytes were preserved. These counts are not
comparable to the older 6,443-file / 2,683-owner marker-annotation experiment.

- Input SHA256: `887b339c0f8be7d7aee641aa4d571e88d049424160c263c7a76df82c7c035830`.
- Accepted full-build CLI SHA256: `260b91cc4738795f85cd33c822c7f10516ede0b7320f00abac6acd272db72b6a`.
- Unchanged helper SHA256: `1803859bbab335187bfeb02ff6419032455c8abfcb018a5513572922994a93e2`.
- Main rebuilt CLI SHA256: `011e4a24523104589a1766490c6b420b24648f25ded6e45fb4498672b4df358d`.
  The full reference run used the accepted feature executable, not this
  version-metadata-only rebuild. All Rust sources match exactly.

Verification:

- fmt, default-target clippy with warnings denied, build and ordinary tests pass.
- **1,630 passed / 0 failed / 133 ignored**, 237 result groups; repeated after
  main integration. Ignored tests are not counted as executed native tests.
- Five separately valid original fixtures plus migrated mixed kotlinc/javac
  compilation and runtime checks: ordered group, singleton, declared Collection
  alias, retained companion value, and existing Java companion getter caller.
  Candidate class origins, values and shared identity checked.
- Sample e2e: three emitted samples independently javac-compiled; one sample
  intentionally retained, not mislabeled as a successful Java compilation.
- Independent two-file review passed with exact source hashes and empty blocking
  arrays. Nonblocking test/performance suggestions remain suggestions.
- The first ordinary run hit an existing fake-compiler executable-busy setup
  failure. Its log is retained; the exact test and full-suite retries passed.

## Evidence and rejected work

Durable local evidence (private; do not publish raw workload mappings):

`~/.hermes/archives/notlin/case5-2026-10-26/`

- `archive-manifest.json`: verified SHA256 inventory.
- `acceptance/manifest.json`, `review.json`, `PUBLISHED.json`, ordinary/native/e2e
  results and merged gate logs.
- `reference/execution.json`, `comparison.json`, task summary and Gradle log.
- `merged-main-notlin`: rebuilt integrated executable.

Scratch investigation root, if still present:
`~/.hermes/cache/scratch/notlin-generic-defaults/`.
Do not rely on scratch retention for the accepted evidence above.

The unused whole-source-context property-facts producer was removed from this
landing. Its incomplete migration was cancelled after 623.999 seconds versus a
272.358-second baseline. Static tests/review did not override that rejection.
Its archived overlay is `case5-wrap-final1/prechange/` under the scratch root;
it is not a dependency or accepted capability. The superseded final1 reference
run passed but produced no selected-root gain; only final2 closes #5.

## Next-session discipline

1. Inspect current branch, HEAD and dirty/untracked files; do not reset a parked
   candidate or infer state from this dated document.
2. Start new work from current accepted main, not an old trial/experiment ref.
3. Choose one of #2/#6 from a measured blocker. Genericize fixtures completely;
   compile the original before calling the converted result a regression.
4. Keep the reference checkout read-only. Run `notlin --in-place --lombok .`
   from the root of a disposable frozen copy; finish with full `gradle build`
   or an actual application runtime check. Compare complete gained/lost owner
   sets, not only totals. Preserve package-local Lombok configuration.
5. Use the current tool container directly, not `toolbox run`. Set an isolated
   absolute CARGO_TARGET_DIR and scrub NOTLIN_* for ordinary gates. Configure
   native compiler/helper variables explicitly. Keep source writers sequential.
6. Delegate exact-path Git commits; no push, version bump, tags or release
   without the corresponding explicit authorization. Do not commit the reference.
