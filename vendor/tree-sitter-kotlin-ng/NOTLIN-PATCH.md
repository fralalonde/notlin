# Local scanner fix

This is the unmodified `tree-sitter-kotlin-ng` 1.1.0 crate from upstream commit
`77dd60ea0a9003ce062c9728a513ffe1aaff8c82`, with one scanner change: a closing
brace can terminate the preceding class member without a newline or semicolon.
This applies to classes, interfaces, objects and enum members. The generated
parser and its grammar are unchanged. `UPSTREAM.json` records the original file
hashes, and `LICENSE` preserves the upstream MIT license.

The Cargo patch makes every existing parser consumer use the same rule. It does
not rewrite source text or alter byte offsets. `tests/compact_members.rs` checks
member boundaries, nested comments, original spans and malformed input. The
compact contract fixture in `tests/retained_constant_defaults.rs` compares real
Kotlin and translated Java behavior in all JVM-default modes and both Lombok
modes, including a Kotlin value-class consumer.
