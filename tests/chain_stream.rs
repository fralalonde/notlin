use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin `xs.stream().filter { p }.findFirst()` — a PRE-STREAMED receiver
/// with a trailing terminal member — must emit ONE `.stream()` and keep the
/// chain open (`.filter(...)` without a collect), so the outer `.findFirst()`
/// applies to the Stream, not to a reified List.
#[test]
fn chained_stream_filter_keeps_open_chain() {
    let root = Path::new("tests/tmp_scratch_chain");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package c\n\nimport java.util.Optional\n\nclass Hold {\n    val units: List<UnitRec> = emptyList()\n\n    fun getBase(): Optional<UnitRec> {\n        return units.stream().filter { u -> u.q == 1 }.findFirst()\n    }\n}\n\ndata class UnitRec(val q: Int)\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let mut out_java = String::new();
    if let Ok(entries) = fs::read_dir(root) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("java") {
                out_java.push_str(&fs::read_to_string(e.path()).unwrap_or_default());
            }
        }
    }
    assert!(
        !out_java.contains(".stream().stream()"),
        "must not double-stream:\n{out_java}\nstdout:\n{stdout}"
    );
    assert!(
        !out_java.contains("collect(java.util.stream.Collectors.toList()).findFirst()"),
        "must not collect before a stream terminal:\n{out_java}\nstdout:\n{stdout}"
    );
    assert!(
        out_java.contains(".findAny()") || out_java.contains(".findFirst()"),
        "chain should stay open with a terminal member:\n{out_java}\nstdout:\n{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}
