//! One run, one helper process.
//!
//! Phase 0 handed every `FactsProvider` its own `FactsClient`, and the planner
//! constructs a provider per planned unit, so a multi-file run started one JVM
//! per file. These tests drive the real binary with the helper replaced by a
//! shell script that appends its argv to a log, so "how many JVMs did this run
//! start" is a file line count, not a guess.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MEMBERS: &str = include_str!("fixtures/classpath_facts/duration_members.json");

fn one_line_members() -> String {
    MEMBERS.lines().map(str::trim).collect()
}

/// Writes the fake helper (a line-protocol responder) and the `java` wrapper
/// that logs every invocation before exec'ing it. Returns the wrapper path.
fn install_fake_java(root: &Path, log: &Path) -> PathBuf {
    let helper = root.join("helper.sh");
    fs::write(
        &helper,
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "{log}.requests"
  case "$line" in
    *'"owner":"java.time.Duration"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"java.time.Duration","members":{members}}}}}}}\n' "$id"
      ;;
    *)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
  esac
done
"#,
            log = log.display(),
            members = one_line_members()
        ),
    )
    .unwrap();
    let wrapper = root.join("java");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexec sh \"{}\"\n",
            log.display(),
            helper.display()
        ),
    )
    .unwrap();
    for script in [&helper, &wrapper] {
        let mut permissions = fs::metadata(script).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        fs::set_permissions(script, permissions).unwrap();
    }
    wrapper
}

fn workspace(root: &Path) -> Vec<PathBuf> {
    let extend = root.join("Extend.kt");
    fs::write(&extend, include_str!("fixtures/classpath_facts/extend.kt")).unwrap();
    let other = root.join("Other.kt");
    fs::write(
        &other,
        "package sample\n\nimport java.time.Duration\n\nfun shift(base: Duration): Duration = base.plusDays(1L)\n",
    )
    .unwrap();
    let plain = root.join("Plain.kt");
    fs::write(&plain, "package sample\n\nclass Plain\n").unwrap();
    vec![extend, other, plain]
}

fn run(root: &Path, log: &Path, files: &[PathBuf], policy: &str) {
    let wrapper = install_fake_java(root, log);
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(root)
        .env("NOTLIN_JAVA", &wrapper)
        .env("NOTLIN_JVM_FACTS_JAR", root.join("helper.sh"))
        .args([
            policy,
            "-o",
            root.join("out").to_str().unwrap(),
            "--coverage-report",
            root.join("coverage.json").to_str().unwrap(),
        ])
        .args(files)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("notlin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn a_run_starts_one_helper_and_asks_it_once_per_owner() {
    let root = scratch("facts-run-scoped");
    let log = root.join("java.log");
    let files = workspace(&root);
    run(&root, &log, &files, "--jvm-facts=auto");

    let invocations = fs::read_to_string(&log).unwrap();
    let spawned = invocations
        .lines()
        .filter(|line| line.contains("-jar"))
        .count();
    assert_eq!(
        spawned, 1,
        "a three-file run must start the helper once, started {spawned}:\n{invocations}"
    );
    // One memo table also means one question per owner, not one per provider.
    let requests = fs::read_to_string(root.join("java.log.requests")).unwrap();
    assert_eq!(
        requests
            .lines()
            .filter(|line| line.contains("\"owner\":\"java.time.Duration\""))
            .count(),
        1,
        "the Duration member list must be fetched once for the whole run:\n{requests}"
    );
    // And the facts did reach the planner: both Duration calls were answered.
    let coverage = fs::read_to_string(root.join("coverage.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&coverage).unwrap();
    assert!(
        !report["unresolved_symbols"]
            .as_object()
            .unwrap()
            .contains_key("plusDays"),
        "classpath facts must resolve plusDays: {coverage}"
    );
}

#[test]
fn the_off_policy_never_starts_the_helper() {
    let root = scratch("facts-off");
    let log = root.join("java.log");
    let files = workspace(&root);
    run(&root, &log, &files, "--jvm-facts=off");
    assert!(
        !log.exists() || log.metadata().unwrap().len() == 0,
        "{} must not be invoked under --jvm-facts=off",
        log.display()
    );
}
