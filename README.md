# notlin

A Kotlin to Java converter.

# Usage

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

# Install

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

# Why 

Because, believe it or not, Kotlin sucks. 

Despite claims to the contrary, it makes your code more complicated than if you had stuck with Java.

- It is slow to compile
- It was not developed in the open, it is the ego project of a single company whose purpose is to obviously to lock you in
- It doubles down on Object Orientedness when what the world needs is less OO.
- Property-first classes, who the fuck asked for that?
- A million ways to declare constructors. I mean, really. Wat Da fuck.
- Promotes usage of Gradle which sucks even more
- Syntax shortcuts aren't worth it

# Something something coroutines

The async/await mind virus was invented by JS weenies because they didn't have threads.
And it's now a source of pain in every language that forces regular code to care about continuations.

Go ahead, prove to me async is faster than blocking IO. _With your actual prod workload._ 

Hint: It's not.

# But, but, null-checking (mumbles, drools)

Null isn't a problem in Java like it is in C. Just don't use null to represent absence of values in your code. 
Use marker values or Optional. Know where Java stdlib  uses null (Map.get(), etc.) In short: **git gud**.

# My dad told me Java sucks!

It doesn't. Also, your dad's a wuss.

# Lombok is a hack!

Lombok is a perfectly workable hack for a noble language with minor aging issues.

Kotlin is 100% hack for hype-driven resume-filling coders. Jokes on you, there are no jobs anymore. 

# Bruh, this is all vibe-coded

You really thought I'd spend precious hours of my life dealing with "superior intellect" dick-measuring engineering macho fart deconstruction? 

Hell no, I just open the bathroom door, let it all vent out and let nature deal with it. 

But I assure you this README is 100% hand typed and full of spite, but that one you figured out already. You're not _that_ dumb.

# I hate you!

Didn't ask. Don't care. Fuck off.
