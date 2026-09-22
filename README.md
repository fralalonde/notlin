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

## LICENSE

MIT