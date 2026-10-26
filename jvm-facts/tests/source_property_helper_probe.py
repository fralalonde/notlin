#!/usr/bin/env python3
"""Real-helper regression for source-property reference evidence.

Runs the *full* K2 helper jar (no Cargo, no fakes) against a generic fixture and
asserts two contracts of the additive ``references`` batch:

1. Requested-file PSI freshness (fail closed).
   A same-width rename of a class member turns a top-level read into a member
   read without changing file length or the read's offset. The helper's PSI for
   the *requested* file is what resolved the symbol, so a request that arrives
   after the bytes on disk changed must not answer with the previous revision's
   binding. A positive control in the same session proves the site was a real
   top-level read before the mutation, so an empty journal cannot satisfy the
   negative.

2. Declared-type evidence (``target_jvm_descriptor`` / ``target_nullable``).
   The descriptor is the resolved top-level property's *declaration* return
   type, not the type of the reading expression: a `val WIDE: Any = "s"` read
   must report ``Ljava/lang/Object;``. Nullability must be false only for a
   provably non-null declaration; ``val MAYBE: String?`` must not answer false.
   Descriptors are cross-checked against ``javap -s`` output of the fixtures
   compiled by the real Kotlin compiler.

Every assertion is fatal: there is no skip path when the toolchain or jar is
missing. Usage:

    python3 jvm-facts/tests/source_property_helper_probe.py [--jar PATH]
"""

import argparse
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

JAVA = Path(os.environ.get("NOTLIN_JAVA") or shutil.which("java") or "java")
JAVAP = Path(os.environ.get("NOTLIN_JAVAP") or str(JAVA.with_name("javap")))
KOTLINC = Path(os.environ.get("NOTLIN_KOTLINC") or shutil.which("kotlinc") or "kotlinc")
DEFAULT_JAR = Path(__file__).resolve().parents[1] / "build" / "notlin-jvm-facts-full.jar"

STATE_BEFORE = """package probe

const val TOKEN: String = "global"

class Reader {
    val OTHER: String = "member"

    fun read(): String = TOKEN
}
"""

# Same-width rename: the reference text and its byte offset are unchanged, only
# the binding (top-level TOKEN -> class member TOKEN) moves.
STATE_AFTER = STATE_BEFORE.replace("val OTHER:", "val TOKEN:")

DECLARED = """package probe

import java.util.*

val MAYBE: String? = null
val WIDE: Any = "s"
val COUNT: Int = 1
val NAMES: ArrayList<String> = ArrayList()

fun uses() {
    println(MAYBE)
    println(WIDE)
    println(COUNT)
    println(NAMES)
}

fun shadowed(): Int {
    val TOKEN = 7
    return TOKEN
}

class Holder {
    val TOKEN: String = "member"

    fun readMember(): String = TOKEN
}
"""


def child_env(sources: Path) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith("NOTLIN_")}
    env["JAVA_HOME"] = str(JAVA.parent.parent)
    env["NOTLIN_KOTLIN_SOURCES"] = str(sources)
    return env


def compile_kotlin(source: Path, out: Path, env: dict) -> None:
    process = subprocess.run(
        [str(KOTLINC), str(source), "-d", str(out)],
        env=env,
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert process.returncode == 0, f"kotlinc rejected the fixture:\n{process.stderr}"


def javap_fields(classes: Path, owner: str) -> dict:
    """Field name -> JVM descriptor, straight from the compiled class file."""
    process = subprocess.run(
        [str(JAVAP), "-p", "-s", "-classpath", str(classes), owner],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert process.returncode == 0, f"javap failed for {owner}:\n{process.stderr}"
    fields = {}
    pending = None
    for line in process.stdout.splitlines():
        stripped = line.strip()
        if stripped.startswith("descriptor:") and pending is not None:
            fields[pending] = stripped.split(":", 1)[1].strip()
            pending = None
            continue
        if stripped.endswith(";") and "(" not in stripped:
            pending = stripped.rsplit(" ", 1)[-1].rstrip(";")
        else:
            pending = None
    return fields


class Helper:
    """One real helper process, one session, many requests."""

    def __init__(self, jar: Path, sources: Path, stderr_path: Path):
        self.env = child_env(sources)
        self.stderr = stderr_path.open("w")
        self.process = subprocess.Popen(
            [str(JAVA), "-jar", str(jar), "--kotlin"],
            env=self.env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
            text=True,
        )
        self.next_id = 0

    def ask(self, file: Path) -> dict:
        self.next_id += 1
        request = {"version": 1, "id": self.next_id, "query": {"ResolveCalls": {"file": str(file)}}}
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        ready = select.select([self.process.stdout], [], [], 180)[0]
        assert ready, "helper did not answer within 180 s"
        line = self.process.stdout.readline()
        assert line, "helper closed stdout without an answer"
        answer = json.loads(line)
        assert answer.get("id") == self.next_id, f"answer id mismatch: {answer}"
        assert "CallSites" in answer.get("answer", {}), f"helper refused the batch: {answer}"
        return answer["answer"]["CallSites"]

    def close(self) -> None:
        try:
            self.process.stdin.close()
        except BrokenPipeError:
            pass
        self.process.wait(timeout=60)
        self.stderr.close()
        assert self.process.returncode == 0, "helper exited non-zero (see stderr log)"


def reference_at(batch: dict, offset: int) -> list:
    return [r for r in batch["references"] if r["byte_offset"] == offset]


def check_stale_requested_psi(jar: Path, work: Path) -> dict:
    sources = work / "stale-src"
    sources.mkdir()
    state = sources / "State.kt"
    state.write_text(STATE_BEFORE)
    env = child_env(sources)
    compile_kotlin(state, work / "stale-before-classes", env)
    state.write_text(STATE_AFTER)
    compile_kotlin(state, work / "stale-after-classes", env)
    state.write_text(STATE_BEFORE)

    offset = STATE_AFTER.index("= TOKEN") + 2
    assert len(STATE_BEFORE) == len(STATE_AFTER), "mutation must preserve file width"

    helper = Helper(jar, sources, work / "stale-helper.log")
    try:
        control = helper.ask(state)
        state.write_text(STATE_AFTER)
        stale = helper.ask(state)
        # A second request must not re-serve a memoized pre-mutation batch.
        stale_again = helper.ask(state)
        state.write_text(STATE_BEFORE)
        restored = helper.ask(state)
    finally:
        helper.close()

    control_refs = reference_at(control, offset)
    stale_refs = reference_at(stale, offset)
    assert len(control_refs) == 1, (
        "positive control is not exercising a top-level read: "
        f"{json.dumps(control['references'])}"
    )
    # The workload root shape: a `const val …: String` read from the same file.
    facade = javap_fields(work / "stale-before-classes", "probe.StateKt")
    assert facade["TOKEN"] == "Ljava/lang/String;", facade
    assert control_refs[0]["target_jvm_descriptor"] == facade["TOKEN"], control_refs[0]
    assert control_refs[0]["target_nullable"] is False, control_refs[0]
    assert not stale_refs, (
        "stale requested-file PSI proved a source property binding after a "
        f"same-width mutation: {json.dumps(stale_refs)}"
    )
    assert not reference_at(stale_again, offset), (
        "a repeated request re-served the pre-mutation binding: "
        f"{json.dumps(reference_at(stale_again, offset))}"
    )
    restored_refs = reference_at(restored, offset)
    assert len(restored_refs) == 1, (
        "restoring the analyzed bytes must make the binding addressable again "
        f"(the refusal must be revision-based): {json.dumps(restored['references'])}"
    )
    assert {k: v for k, v in restored_refs[0].items() if k not in ("target_jvm_descriptor", "target_nullable")} == \
        {k: v for k, v in control_refs[0].items() if k not in ("target_jvm_descriptor", "target_nullable")}, (
        "restored binding metadata must match the pre-mutation control"
    )

    # A fresh session over the mutated bytes must agree the read is a member
    # read: that proves the negative above is about staleness, not about the
    # fixture never having been a top-level read at that offset.
    state.write_text(STATE_AFTER)
    fresh = Helper(jar, sources, work / "stale-fresh.log")
    try:
        fresh_batch = fresh.ask(state)
    finally:
        fresh.close()
    assert not reference_at(fresh_batch, offset), (
        "a fresh session must agree that the mutated read is a member read, not "
        f"a top-level property: {json.dumps(reference_at(fresh_batch, offset))}"
    )
    return {
        "control_refs": len(control_refs),
        "stale_refs": len(stale_refs),
        "stale_repeat_refs": len(reference_at(stale_again, offset)),
        "restored_refs": len(restored_refs),
    }


def check_declared_type_evidence(jar: Path, work: Path) -> dict:
    sources = work / "declared-src"
    sources.mkdir()
    declared = sources / "Declared.kt"
    declared.write_text(DECLARED)
    env = child_env(sources)
    classes = work / "declared-classes"
    compile_kotlin(declared, classes, env)
    facade = javap_fields(classes, "probe.DeclaredKt")
    assert facade["MAYBE"] == "Ljava/lang/String;", facade
    assert facade["WIDE"] == "Ljava/lang/Object;", facade
    assert facade["COUNT"] == "I", facade
    assert facade["NAMES"] == "Ljava/util/ArrayList;", facade

    # Offsets point at the *read* inside uses(), not at the declaration's name.
    offsets = {name: DECLARED.index(f"println({name})") + len("println(")
               for name in ("MAYBE", "WIDE", "COUNT", "NAMES")}
    # `TOKEN` inside shadowed(): the read (not the local declaration's own name).
    shadowed_at = DECLARED.index("fun shadowed")
    class_at = DECLARED.index("class Holder")
    offsets["SHADOWED"] = DECLARED.rindex("TOKEN", shadowed_at, class_at)
    # The last `TOKEN` in the file is the Holder member read.
    offsets["MEMBER"] = DECLARED.rindex("TOKEN", class_at)

    helper = Helper(jar, sources, work / "declared-helper.log")
    try:
        batch = helper.ask(declared)
    finally:
        helper.close()

    observed = {}
    for name, offset in offsets.items():
        found = reference_at(batch, offset)
        assert len(found) <= 1, f"{name}: duplicate references at one offset"
        observed[name] = found[0] if found else None

    for name in ("MAYBE", "WIDE", "COUNT", "NAMES"):
        reference = observed[name]
        assert reference is not None, f"{name}: expected a top-level property reference"
        assert reference["target_name"] == name, reference
        assert reference["target_package"] == "probe", reference
        # Existing binding metadata must survive the additive fields.
        assert reference["target_file"].endswith("/Declared.kt"), reference
        assert reference["end_byte"] - reference["byte_offset"] == len(name), reference
        assert reference["target_end_byte"] > reference["target_start_byte"], reference
        assert reference["target_jvm_descriptor"] == facade[name], (
            f"{name}: helper descriptor {reference['target_jvm_descriptor']!r} disagrees "
            f"with javap {facade[name]!r}"
        )

    assert observed["MAYBE"]["target_nullable"] is True, observed["MAYBE"]
    for name in ("WIDE", "COUNT", "NAMES"):
        assert observed[name]["target_nullable"] is False, observed[name]
    # Declaration, not usage: the reading expression of WIDE is a String literal.
    assert observed["WIDE"]["target_jvm_descriptor"] == "Ljava/lang/Object;", observed["WIDE"]

    assert observed["SHADOWED"] is None, (
        f"a local val must not be reported as a top-level property: {observed['SHADOWED']}"
    )
    assert observed["MEMBER"] is None, (
        f"a class member read must not be reported as a top-level property: {observed['MEMBER']}"
    )
    return {
        "descriptors": {n: observed[n]["target_jvm_descriptor"] for n in ("MAYBE", "WIDE", "COUNT", "NAMES")},
        "nullable": {n: observed[n]["target_nullable"] for n in ("MAYBE", "WIDE", "COUNT", "NAMES")},
        "absences": [n for n in ("SHADOWED", "MEMBER") if observed[n] is None],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    jar = args.jar.resolve()
    assert jar.is_file(), f"missing helper jar: {jar} (run jvm-facts/build.sh --full)"
    assert JAVA.is_file(), f"missing JDK launcher: {JAVA}"
    assert KOTLINC.is_file(), f"missing kotlinc: {KOTLINC}"

    work = Path(tempfile.mkdtemp(prefix="source-property-probe-"))
    try:
        stale = check_stale_requested_psi(jar, work)
        declared = check_declared_type_evidence(jar, work)
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"workdir kept: {work}")
    print(json.dumps({"jar": str(jar), "stale": stale, "declared": declared}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
