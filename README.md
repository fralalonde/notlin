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
```

Workspace migration also reports targeted source changes that may unlock more
translation. `U001` identifies competing inherited getter contracts on a retained
Kotlin implementation; `U002` identifies the actual constructor overloads that
collide after erasure, with resolved caller locations when available. Each
recommendation explains the constraint and a possible manual change. Parser
errors, unknown types and conservative secondary-constructor checks do not
produce source-change advice.

Recommendations are ranked by related retained declarations using resolved
symbol references, with at most ten shown per run. These counts are not promises
of successful translation: review the suggested edit, rerun Notlin, and validate
the mixed result. Locations refer to the input snapshots before migration edits.
The library exposes the complete structured report through
`translation_advice::analyze`.

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
